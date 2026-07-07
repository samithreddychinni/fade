use std::fmt;
use std::path::{Component, Path, PathBuf};

use crate::{FadeError, Result};

const RESERVED_COMPONENT: &str = ".fade";

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RelativePath(String);

impl RelativePath {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut parts = Vec::new();

        for component in path.components() {
            match component {
                Component::Normal(segment) => {
                    let Some(segment) = segment.to_str() else {
                        return invalid(path, "path must be valid UTF-8");
                    };
                    if segment.is_empty() {
                        continue;
                    }
                    if segment == RESERVED_COMPONENT {
                        return invalid(path, ".fade is reserved for Fade metadata");
                    }
                    parts.push(segment.to_string());
                }
                Component::CurDir => {}
                Component::ParentDir => return invalid(path, "parent traversal is not allowed"),
                Component::RootDir | Component::Prefix(_) => {
                    return invalid(path, "path must be relative");
                }
            }
        }

        Ok(Self(parts.join("/")))
    }

    pub fn root() -> Self {
        Self(String::new())
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn to_path_buf(&self) -> PathBuf {
        if self.is_root() {
            PathBuf::new()
        } else {
            self.0.split('/').collect()
        }
    }

    pub fn components(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.0.split('/').filter(|part| !part.is_empty())
    }

    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }

        let mut parts: Vec<_> = self.components().collect();
        parts.pop();
        Some(Self(parts.join("/")))
    }

    pub fn file_name(&self) -> Option<&str> {
        self.components().next_back()
    }

    pub fn join_segment(&self, segment: &str) -> Result<Self> {
        let segment_path = RelativePath::new(segment)?;
        if segment_path.components().count() != 1 {
            return Err(FadeError::InvalidPath {
                path: segment.to_string(),
                reason: "expected exactly one path segment".to_string(),
            });
        }

        if self.is_root() {
            Ok(segment_path)
        } else {
            Ok(Self(format!("{}/{}", self.0, segment_path.0)))
        }
    }

    pub fn starts_with(&self, parent: &Self) -> bool {
        if parent.is_root() {
            return true;
        }

        self.0 == parent.0 || self.0.starts_with(&format!("{}/", parent.0))
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() {
            formatter.write_str(".")
        } else {
            formatter.write_str(&self.0)
        }
    }
}

pub fn safe_join(root: &Path, relative_path: &RelativePath) -> Result<PathBuf> {
    let joined = root.join(relative_path.to_path_buf());
    if joined.starts_with(root) {
        Ok(joined)
    } else {
        Err(FadeError::PathTraversal { path: joined })
    }
}

fn invalid<T>(path: &Path, reason: &str) -> Result<T> {
    Err(FadeError::InvalidPath {
        path: path.display().to_string(),
        reason: reason.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_simple_relative_paths() {
        let path = RelativePath::new("./24h/reports/output.json").unwrap();
        assert_eq!(path.as_str(), "24h/reports/output.json");
        assert_eq!(path.parent().unwrap().as_str(), "24h/reports");
        assert_eq!(path.file_name(), Some("output.json"));
    }

    #[test]
    fn rejects_absolute_and_parent_paths() {
        assert!(RelativePath::new("/tmp/file").is_err());
        assert!(RelativePath::new("../file").is_err());
        assert!(RelativePath::new("24h/../file").is_err());
    }

    #[test]
    fn rejects_reserved_metadata_path() {
        assert!(RelativePath::new(".fade/metadata.sqlite").is_err());
        assert!(RelativePath::new("24h/.fade/file").is_err());
    }

    #[test]
    fn joins_single_segments() {
        let path = RelativePath::new("24h").unwrap().join_segment("a.txt").unwrap();
        assert_eq!(path.as_str(), "24h/a.txt");
        assert!(path.join_segment("../escape").is_err());
    }
}
