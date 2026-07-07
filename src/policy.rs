use crate::duration::Ttl;
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
}
