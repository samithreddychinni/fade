use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum FadeError {
    #[error("invalid duration `{input}`: {reason}")]
    InvalidDuration { input: String, reason: String },

    #[error("invalid relative path `{path}`: {reason}")]
    InvalidPath { path: String, reason: String },

    #[error("path `{path}` is outside the Fade backing directory")]
    PathTraversal { path: PathBuf },

    #[error("no TTL policy applies to `{path}`")]
    MissingTtl { path: String },

    #[error("time calculation overflowed")]
    TimeOverflow,

    #[error("metadata state `{0}` is not recognized")]
    InvalidState(String),

    #[error("metadata size `{0}` cannot be represented")]
    InvalidSize(i64),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, FadeError>;
