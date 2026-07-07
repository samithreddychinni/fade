use std::fs;
use std::path::Path;
use std::time::Instant;

use serde::Serialize;

use crate::metadata::MetadataStore;
use crate::path::safe_join;
use crate::Result;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReaperReport {
    pub run_at: i64,
    pub expired_files: u64,
    pub deleted_files: u64,
    pub missing_backing_files: u64,
    pub reclaimed_bytes: u64,
    pub duration_ms: u64,
}

pub fn run_once(store: &MetadataStore, backing_dir: &Path, now: i64) -> Result<ReaperReport> {
    let started = Instant::now();
    let expired_files = store.expire_due(now)?;
    let candidates = store.gc_candidates(now, 10_000)?;

    let mut deleted_files = 0;
    let mut missing_backing_files = 0;
    let mut reclaimed_bytes = 0;

    for record in candidates {
        let backing_path = safe_join(backing_dir, &record.backing_path)?;
        match fs::remove_file(&backing_path) {
            Ok(()) => {
                deleted_files += 1;
                reclaimed_bytes += record.size_bytes;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing_backing_files += 1;
            }
            Err(error) => return Err(error.into()),
        }

        store.mark_deleted(&record.id)?;
    }

    let report = ReaperReport {
        run_at: now,
        expired_files,
        deleted_files,
        missing_backing_files,
        reclaimed_bytes,
        duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
    };

    store.record_reaper_run(now, report.duration_ms, None)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::*;
    use crate::duration::Ttl;
    use crate::metadata::{FileRecord, FileState};
    use crate::path::RelativePath;

    #[test]
    fn deletes_expired_files_after_recovery_deadline() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join(".fade/metadata.sqlite");
        let store = MetadataStore::open(db_path).unwrap();
        let path = RelativePath::new("1s/report.json").unwrap();
        fs::create_dir_all(temp.path().join("1s")).unwrap();
        fs::write(temp.path().join(path.to_path_buf()), b"expired").unwrap();

        let record = FileRecord::new(
            path.clone(),
            path.clone(),
            Ttl::Duration(Duration::from_secs(1)),
            "folder:1s".to_string(),
            100,
            Duration::from_secs(0),
            7,
        )
        .unwrap();
        store.insert_file(&record).unwrap();

        let report = run_once(&store, temp.path(), 101).unwrap();
        let loaded = store.get_by_path(&path).unwrap().unwrap();

        assert_eq!(report.expired_files, 1);
        assert_eq!(report.deleted_files, 1);
        assert_eq!(report.reclaimed_bytes, 7);
        assert_eq!(loaded.state, FileState::Deleted);
        assert!(!temp.path().join(path.to_path_buf()).exists());
    }

    #[test]
    fn tolerates_missing_backing_files() {
        let temp = tempdir().unwrap();
        let store = MetadataStore::open(temp.path().join(".fade/metadata.sqlite")).unwrap();
        let path = RelativePath::new("1s/missing.json").unwrap();
        let record = FileRecord::new(
            path.clone(),
            path.clone(),
            Ttl::Duration(Duration::from_secs(1)),
            "folder:1s".to_string(),
            100,
            Duration::from_secs(0),
            10,
        )
        .unwrap();
        store.insert_file(&record).unwrap();

        let report = run_once(&store, temp.path(), 101).unwrap();
        let loaded = store.get_by_path(&path).unwrap().unwrap();

        assert_eq!(report.missing_backing_files, 1);
        assert_eq!(loaded.state, FileState::Deleted);
    }
}
