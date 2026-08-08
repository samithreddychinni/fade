use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::Instant;

use serde::Serialize;

use crate::metadata::{FileState, MetadataStore};
use crate::path::{RelativePath, safe_join};
use crate::{FadeError, Result};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReaperReport {
    pub run_at: i64,
    pub expired_files: u64,
    pub deleted_files: u64,
    pub missing_backing_files: u64,
    pub reclaimed_bytes: u64,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReconciliationReport {
    pub completed_renames: u64,
    pub canceled_renames: u64,
    pub missing_backing_files: u64,
}

pub fn reconcile(store: &MetadataStore, backing_dir: &Path) -> Result<ReconciliationReport> {
    let mut report = ReconciliationReport::default();

    for rename in store.pending_renames()? {
        let from = safe_join(backing_dir, &rename.from)?;
        let to = safe_join(backing_dir, &rename.to)?;
        match (path_exists(&from)?, path_exists(&to)?) {
            (false, true) => {
                if rename.is_directory {
                    store.rename_prefix(&rename.from, &rename.to, 0)?;
                } else {
                    store.rename_path(&rename.from, &rename.to, 0)?;
                }
                store.complete_rename(&rename.from)?;
                report.completed_renames += 1;
            }
            (true, false) => {
                store.complete_rename(&rename.from)?;
                report.canceled_renames += 1;
            }
            _ => {
                return Err(FadeError::UnresolvedRename { from, to });
            }
        }
    }

    for record in store.list_files(false)? {
        let backing_path = safe_join(backing_dir, &record.backing_path)?;
        if !path_exists(&backing_path)? {
            store.mark_deleted(&record.id)?;
            report.missing_backing_files += 1;
        }
    }

    let tracked_paths: HashSet<_> = store
        .list_files(false)?
        .into_iter()
        .filter(|record| record.state != FileState::Deleted)
        .map(|record| record.backing_path)
        .collect();
    reject_untracked_files(backing_dir, &RelativePath::root(), &tracked_paths)?;

    Ok(report)
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn reject_untracked_files(
    backing_dir: &Path,
    relative_dir: &RelativePath,
    tracked_paths: &HashSet<RelativePath>,
) -> Result<()> {
    let directory = safe_join(backing_dir, relative_dir)?;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".fade" {
            continue;
        }
        let Some(name) = name.to_str() else {
            continue;
        };
        let relative_path = relative_dir.join_segment(name)?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            reject_untracked_files(backing_dir, &relative_path, tracked_paths)?;
        } else if file_type.is_file() && !tracked_paths.contains(&relative_path) {
            return Err(FadeError::UntrackedBackingFile {
                path: entry.path(),
            });
        }
    }
    Ok(())
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

    #[test]
    fn reconciles_interrupted_create_rename_and_delete() {
        let temp = tempdir().unwrap();
        let store = MetadataStore::open(temp.path().join(".fade/metadata.sqlite")).unwrap();
        fs::create_dir_all(temp.path().join("1h")).unwrap();

        let create = sample_record("1h/create.txt");
        let delete = sample_record("1h/delete.txt");
        let rename = sample_record("1h/old.txt");
        store.insert_file(&create).unwrap();
        store.insert_file(&delete).unwrap();
        store.insert_file(&rename).unwrap();
        store
            .begin_rename(
                &rename.path,
                &RelativePath::new("1h/new.txt").unwrap(),
                false,
            )
            .unwrap();

        fs::write(temp.path().join("1h/new.txt"), b"renamed").unwrap();
        let report = reconcile(&store, temp.path()).unwrap();

        assert_eq!(report.completed_renames, 1);
        assert_eq!(report.missing_backing_files, 2);
        assert_eq!(
            store.get_by_path(&create.path).unwrap().unwrap().state,
            FileState::Deleted
        );
        assert_eq!(
            store.get_by_path(&delete.path).unwrap().unwrap().state,
            FileState::Deleted
        );
        assert!(store
            .get_by_path(&RelativePath::new("1h/new.txt").unwrap())
            .unwrap()
            .is_some());
        assert!(store.pending_renames().unwrap().is_empty());
    }

    #[test]
    fn rejects_untracked_backing_files() {
        let temp = tempdir().unwrap();
        let store = MetadataStore::open(temp.path().join(".fade/metadata.sqlite")).unwrap();
        fs::create_dir_all(temp.path().join("1h")).unwrap();
        fs::write(temp.path().join("1h/unknown.txt"), b"unknown").unwrap();

        assert!(matches!(
            reconcile(&store, temp.path()),
            Err(FadeError::UntrackedBackingFile { .. })
        ));
    }

    fn sample_record(path: &str) -> FileRecord {
        let path = RelativePath::new(path).unwrap();
        FileRecord::new(
            path.clone(),
            path,
            Ttl::Duration(Duration::from_secs(3_600)),
            "folder:1h".to_string(),
            100,
            Duration::from_secs(3_600),
            0,
        )
        .unwrap()
    }
}
