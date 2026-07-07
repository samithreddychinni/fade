use std::fs;
use std::path::{Path, PathBuf};

use crate::Result;

pub const FADE_DIR: &str = ".fade";
pub const METADATA_DB: &str = "metadata.sqlite";

pub fn metadata_dir(backing_dir: &Path) -> PathBuf {
    backing_dir.join(FADE_DIR)
}

pub fn metadata_db_path(backing_dir: &Path) -> PathBuf {
    metadata_dir(backing_dir).join(METADATA_DB)
}

pub fn init_backing_dir(backing_dir: &Path) -> Result<()> {
    fs::create_dir_all(backing_dir)?;
    fs::create_dir_all(metadata_dir(backing_dir))?;
    Ok(())
}

pub fn looks_like_backing_dir(path: &Path) -> bool {
    metadata_db_path(path).is_file()
}
