use std::fs;
use std::path::Path;
use std::time::Duration;

use globset::{Glob, GlobMatcher};
use serde::Deserialize;

use crate::duration::{Ttl, parse_duration};
use crate::path::RelativePath;
use crate::{FadeError, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicySource {
    Folder { folder: String },
    ConfigRule { pattern: String },
    ExplicitDefault,
}

impl PolicySource {
    pub fn as_label(&self) -> String {
        match self {
            Self::Folder { folder } => format!("folder:{folder}"),
            Self::ConfigRule { pattern } => format!("config:{pattern}"),
            Self::ExplicitDefault => "default".to_string(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TtlAssignment {
    pub ttl: Ttl,
    pub source: PolicySource,
}

pub enum Policy {
    Dev(DevPolicy),
    Config(ConfigPolicy),
}

impl Policy {
    pub fn from_config(path: &Path) -> Result<Self> {
        Ok(Self::Config(ConfigPolicy::load(path)?))
    }

    pub fn assign_file(&self, path: &RelativePath) -> Result<TtlAssignment> {
        match self {
            Self::Dev(policy) => policy.assign_file(path),
            Self::Config(policy) => policy.assign_file(path),
        }
    }

    pub fn directory_visible(&self, path: &RelativePath) -> bool {
        match self {
            Self::Dev(_) => path
                .components()
                .next()
                .is_some_and(|first| Ttl::parse(first).is_ok()),
            Self::Config(_) => true,
        }
    }

    pub fn validate_root_directory(&self, segment: &str) -> Result<()> {
        if let Self::Dev(policy) = self {
            policy.validate_root_policy_folder(segment)?;
        }
        Ok(())
    }

    pub fn is_dev(&self) -> bool {
        matches!(self, Self::Dev(_))
    }

    pub fn recovery_window(&self) -> Option<Duration> {
        match self {
            Self::Dev(_) => None,
            Self::Config(policy) => policy.recovery_window,
        }
    }

    pub fn reaper_interval(&self) -> Option<Duration> {
        match self {
            Self::Dev(_) => None,
            Self::Config(policy) => policy.reaper_interval,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DevPolicy;

impl DevPolicy {
    pub fn assign_file(&self, path: &RelativePath) -> Result<TtlAssignment> {
        let mut components: Vec<_> = path.components().collect();
        components.pop();

        for component in components.into_iter().rev() {
            if let Ok(ttl) = Ttl::parse(component) {
                return Ok(TtlAssignment {
                    ttl,
                    source: PolicySource::Folder {
                        folder: component.to_string(),
                    },
                });
            }
        }

        Err(FadeError::MissingTtl {
            path: path.to_string(),
        })
    }

    pub fn validate_root_policy_folder(&self, segment: &str) -> Result<Ttl> {
        Ttl::parse(segment)
    }
}

pub struct ConfigPolicy {
    rules: Vec<CompiledRule>,
    recovery_window: Option<Duration>,
    reaper_interval: Option<Duration>,
}

struct CompiledRule {
    pattern: String,
    matcher: GlobMatcher,
    ttl: Ttl,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    rules: Vec<ConfigRule>,
    reaper: Option<ReaperConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigRule {
    pattern: String,
    ttl: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReaperConfig {
    interval: Option<String>,
    recovery_window: Option<String>,
}

impl ConfigPolicy {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)?;
        let config: ConfigFile =
            toml::from_str(&text).map_err(|error| FadeError::InvalidPolicy(error.to_string()))?;
        if config.rules.is_empty() {
            return Err(FadeError::InvalidPolicy(
                "add at least one rule".to_string(),
            ));
        }
        if config.rules.iter().position(|rule| rule.pattern == "*") != Some(config.rules.len() - 1)
        {
            return Err(FadeError::InvalidPolicy(
                "pattern `*` must appear once as the final fallback rule".to_string(),
            ));
        }

        let recovery_window = config
            .reaper
            .as_ref()
            .and_then(|reaper| reaper.recovery_window.as_deref())
            .map(|value| parse_duration(value, true))
            .transpose()?;
        let reaper_interval = config
            .reaper
            .as_ref()
            .and_then(|reaper| reaper.interval.as_deref())
            .map(|value| parse_duration(value, false))
            .transpose()?;

        let rules = config
            .rules
            .into_iter()
            .map(|rule| {
                let ttl = Ttl::parse(&rule.ttl)?;
                let matcher = Glob::new(&rule.pattern)
                    .map_err(|error| FadeError::InvalidPolicy(error.to_string()))?
                    .compile_matcher();
                Ok(CompiledRule {
                    pattern: rule.pattern,
                    matcher,
                    ttl,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            rules,
            recovery_window,
            reaper_interval,
        })
    }

    fn assign_file(&self, path: &RelativePath) -> Result<TtlAssignment> {
        self.rules
            .iter()
            .find(|rule| rule.matcher.is_match(path.as_str()))
            .map(|rule| TtlAssignment {
                ttl: rule.ttl,
                source: PolicySource::ConfigRule {
                    pattern: rule.pattern.clone(),
                },
            })
            .ok_or_else(|| FadeError::MissingTtl {
                path: path.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assigns_ttl_from_nearest_parent_folder() {
        let policy = DevPolicy;
        let path = RelativePath::new("7d/scratch/1h/report.json").unwrap();
        let assignment = policy.assign_file(&path).unwrap();

        assert_eq!(assignment.ttl, Ttl::parse("1h").unwrap());
        assert_eq!(
            assignment.source,
            PolicySource::Folder {
                folder: "1h".to_string()
            }
        );
    }

    #[test]
    fn ignores_file_names_when_assigning_ttl() {
        let policy = DevPolicy;
        let path = RelativePath::new("scratch/24h").unwrap();
        assert!(policy.assign_file(&path).is_err());
    }

    #[test]
    fn supports_forever_folder() {
        let policy = DevPolicy;
        let path = RelativePath::new("forever/notes.txt").unwrap();
        let assignment = policy.assign_file(&path).unwrap();

        assert_eq!(assignment.ttl, Ttl::Forever);
    }

    #[test]
    fn uses_the_first_matching_config_rule() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("fade.toml");
        fs::write(
            &config,
            "[[rules]]\npattern = \"*.token\"\nttl = \"1h\"\n\n[[rules]]\npattern = \"*\"\nttl = \"24h\"\n",
        )
        .unwrap();

        let policy = ConfigPolicy::load(&config).unwrap();
        let assignment = policy
            .assign_file(&RelativePath::new("7d/session.token").unwrap())
            .unwrap();

        assert_eq!(assignment.ttl, Ttl::parse("1h").unwrap());
        assert_eq!(
            assignment.source,
            PolicySource::ConfigRule {
                pattern: "*.token".to_string()
            }
        );
    }

    #[test]
    fn rejects_an_empty_config() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("fade.toml");
        fs::write(&config, "rules = []\n").unwrap();

        assert!(matches!(
            ConfigPolicy::load(&config),
            Err(FadeError::InvalidPolicy(_))
        ));
    }

    #[test]
    fn loads_reaper_settings_and_rejects_incomplete_configs() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("fade.toml");
        fs::write(
            &config,
            "[[rules]]\npattern = \"*\"\nttl = \"24h\"\n\n[reaper]\ninterval = \"30s\"\nrecovery_window = \"0\"\n",
        )
        .unwrap();

        let policy = Policy::from_config(&config).unwrap();
        assert_eq!(policy.reaper_interval(), Some(Duration::from_secs(30)));
        assert_eq!(policy.recovery_window(), Some(Duration::ZERO));

        fs::write(&config, "[[rules]]\npattern = \"*.token\"\nttl = \"1h\"\n").unwrap();
        assert!(matches!(
            ConfigPolicy::load(&config),
            Err(FadeError::InvalidPolicy(_))
        ));

        fs::write(
            &config,
            "[[rules]]\npattern = \"*\"\nttl = \"1h\"\n\n[[rules]]\npattern = \"*\"\nttl = \"24h\"\n",
        )
        .unwrap();
        assert!(matches!(
            ConfigPolicy::load(&config),
            Err(FadeError::InvalidPolicy(_))
        ));

        fs::write(
            &config,
            "[[rules]]\npattern = \"*\"\nttl = \"1h\"\nunknown = true\n",
        )
        .unwrap();
        assert!(matches!(
            ConfigPolicy::load(&config),
            Err(FadeError::InvalidPolicy(_))
        ));
    }
}
