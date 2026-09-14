use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use uuid::Uuid;

use crate::audit;
use crate::duration::Ttl;
use crate::path::RelativePath;
use crate::{FadeError, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    Alive,
    Expired,
    Recovered,
    Deleted,
}

impl FileState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Alive => "alive",
            Self::Expired => "expired",
            Self::Recovered => "recovered",
            Self::Deleted => "deleted",
        }
    }

    fn from_str(input: &str) -> Result<Self> {
        match input {
            "alive" => Ok(Self::Alive),
            "expired" => Ok(Self::Expired),
            "recovered" => Ok(Self::Recovered),
            "deleted" => Ok(Self::Deleted),
            other => Err(FadeError::InvalidState(other.to_string())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileRecord {
    pub id: String,
    pub path: RelativePath,
    pub backing_path: RelativePath,
    pub created_at: i64,
    pub modified_at: i64,
    pub ttl_seconds: Option<i64>,
    pub expires_at: Option<i64>,
    pub recovery_deadline: Option<i64>,
    pub state: FileState,
    pub policy_source: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingRename {
    pub from: RelativePath,
    pub to: RelativePath,
    pub is_directory: bool,
}

impl FileRecord {
    pub fn new(
        path: RelativePath,
        backing_path: RelativePath,
        ttl: Ttl,
        policy_source: String,
        now: i64,
        recovery_window: Duration,
        size_bytes: u64,
    ) -> Result<Self> {
        let expires_at = ttl.expires_at(now)?;
        let recovery_deadline = match expires_at {
            Some(expires_at) => {
                let recovery_seconds = recovery_window.as_secs().min(i64::MAX as u64) as i64;
                Some(
                    expires_at
                        .checked_add(recovery_seconds)
                        .ok_or(FadeError::TimeOverflow)?,
                )
            }
            None => None,
        };

        Ok(Self {
            id: Uuid::new_v4().to_string(),
            path,
            backing_path,
            created_at: now,
            modified_at: now,
            ttl_seconds: ttl.ttl_seconds(),
            expires_at,
            recovery_deadline,
            state: FileState::Alive,
            policy_source,
            size_bytes,
        })
    }

    pub fn is_expired_at(&self, now: i64) -> bool {
        self.state == FileState::Alive && self.expires_at.is_some_and(|expires_at| expires_at <= now)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StoreStats {
    pub alive_files: u64,
    pub expired_recoverable_files: u64,
    pub deleted_files: u64,
    pub pending_deletion_files: u64,
    pub pending_deletion_bytes: u64,
    pub last_reaper_run_at: Option<i64>,
    pub last_reaper_duration_ms: Option<u64>,
    pub last_reaper_error: Option<String>,
}

#[derive(Debug)]
pub struct MetadataStore {
    conn: Connection,
    db_path: PathBuf,
}

impl MetadataStore {
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(&db_path)?;
        let store = Self { conn, db_path };
        store.migrate()?;
        Ok(store)
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn insert_file(&self, record: &FileRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO files (
                id,
                path,
                backing_path,
                created_at,
                modified_at,
                ttl_seconds,
                expires_at,
                recovery_deadline,
                state,
                policy_source,
                size_bytes
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                record.id,
                record.path.as_str(),
                record.backing_path.as_str(),
                record.created_at,
                record.modified_at,
                record.ttl_seconds,
                record.expires_at,
                record.recovery_deadline,
                record.state.as_str(),
                record.policy_source,
                record.size_bytes as i64,
            ],
        )?;
        Ok(())
    }

    pub fn get_by_path(&self, path: &RelativePath) -> Result<Option<FileRecord>> {
        self.conn
            .query_row(
                "SELECT
                    id,
                    path,
                    backing_path,
                    created_at,
                    modified_at,
                    ttl_seconds,
                    expires_at,
                    recovery_deadline,
                    state,
                    policy_source,
                    size_bytes
                FROM files
                WHERE path = ?1
                ORDER BY state = 'deleted', created_at DESC
                LIMIT 1",
                params![path.as_str()],
                read_record,
            )
            .optional()
            .map_err(FadeError::from)
    }

    pub fn list_files(&self, include_deleted: bool) -> Result<Vec<FileRecord>> {
        let sql = if include_deleted {
            "SELECT
                id,
                path,
                backing_path,
                created_at,
                modified_at,
                ttl_seconds,
                expires_at,
                recovery_deadline,
                state,
                policy_source,
                size_bytes
            FROM files
            ORDER BY path"
        } else {
            "SELECT
                id,
                path,
                backing_path,
                created_at,
                modified_at,
                ttl_seconds,
                expires_at,
                recovery_deadline,
                state,
                policy_source,
                size_bytes
            FROM files
            WHERE state != 'deleted'
            ORDER BY path"
        };

        let mut statement = self.conn.prepare(sql)?;
        let records = statement
            .query_map([], read_record)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(records)
    }

    pub fn expire_due(&self, now: i64) -> Result<u64> {
        let mut statement = self.conn.prepare(
            "UPDATE files
            SET state = 'expired'
            WHERE state = 'alive'
              AND expires_at IS NOT NULL
              AND expires_at <= ?1
            RETURNING path, policy_source",
        )?;
        let expired = statement
            .query_map(params![now], |row| {
                let path = RelativePath::new(row.get::<_, String>(0)?)
                    .map_err(to_sql_conversion_error(0))?;
                Ok((path, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);

        if let Some(backing_dir) = self.audit_backing_dir() {
            for (path, policy_source) in &expired {
                audit::append(
                    backing_dir,
                    now,
                    "file_expired",
                    path,
                    Some(policy_source),
                    None,
                )?;
            }
        }
        Ok(expired.len() as u64)
    }

    pub fn gc_candidates(&self, now: i64, limit: u64) -> Result<Vec<FileRecord>> {
        let mut statement = self.conn.prepare(
            "SELECT
                id,
                path,
                backing_path,
                created_at,
                modified_at,
                ttl_seconds,
                expires_at,
                recovery_deadline,
                state,
                policy_source,
                size_bytes
            FROM files
            WHERE state = 'expired'
              AND recovery_deadline IS NOT NULL
              AND recovery_deadline <= ?1
            ORDER BY recovery_deadline, path
            LIMIT ?2",
        )?;
        let records = statement
            .query_map(params![now, limit as i64], read_record)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(records)
    }

    pub fn mark_deleted(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE files
            SET state = 'deleted'
            WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn is_recoverable(&self, id: &str, now: i64) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM files
            WHERE id = ?1
              AND state = 'expired'
              AND recovery_deadline IS NOT NULL
              AND recovery_deadline > ?2
                )",
                params![id, now],
                |row| row.get(0),
            )
            .map_err(FadeError::from)
    }

    pub fn mark_deleted_by_path(&self, path: &RelativePath) -> Result<()> {
        self.conn.execute(
            "UPDATE files
            SET state = 'deleted'
            WHERE path = ?1
              AND state != 'deleted'",
            params![path.as_str()],
        )?;
        Ok(())
    }

    pub fn begin_rename(
        &self,
        from: &RelativePath,
        to: &RelativePath,
        is_directory: bool,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO pending_renames (from_path, to_path, is_directory)
            VALUES (?1, ?2, ?3)",
            params![from.as_str(), to.as_str(), is_directory],
        )?;
        Ok(())
    }

    pub fn pending_renames(&self) -> Result<Vec<PendingRename>> {
        let mut statement = self.conn.prepare(
            "SELECT from_path, to_path, is_directory
            FROM pending_renames
            ORDER BY from_path",
        )?;
        let renames = statement
            .query_map([], |row| {
                Ok(PendingRename {
                    from: RelativePath::new(row.get::<_, String>(0)?)
                        .map_err(to_sql_conversion_error(0))?,
                    to: RelativePath::new(row.get::<_, String>(1)?)
                        .map_err(to_sql_conversion_error(1))?,
                    is_directory: row.get(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(renames)
    }

    pub fn complete_rename(&self, from: &RelativePath) -> Result<()> {
        self.conn.execute(
            "DELETE FROM pending_renames WHERE from_path = ?1",
            params![from.as_str()],
        )?;
        Ok(())
    }

    pub fn rename_path(&self, from: &RelativePath, to: &RelativePath, modified_at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE files
            SET path = ?2,
                backing_path = ?2,
                modified_at = ?3
            WHERE path = ?1
              AND state != 'deleted'",
            params![from.as_str(), to.as_str(), modified_at],
        )?;
        Ok(())
    }

    pub fn rename_prefix(
        &self,
        from: &RelativePath,
        to: &RelativePath,
        modified_at: i64,
    ) -> Result<()> {
        let from_prefix = format!("{}/", from.as_str());
        let to_prefix = format!("{}/", to.as_str());
        let suffix_start = (from_prefix.len() + 1) as i64;

        self.conn.execute(
            "UPDATE files
            SET path = ?2 || substr(path, ?3),
                backing_path = ?2 || substr(backing_path, ?3),
                modified_at = ?4
            WHERE state != 'deleted'
              AND path LIKE ?1 || '%'",
            params![from_prefix, to_prefix, suffix_start, modified_at],
        )?;
        Ok(())
    }

    pub fn update_observed_size(
        &self,
        path: &RelativePath,
        size_bytes: u64,
        modified_at: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE files
            SET size_bytes = ?2,
                modified_at = ?3
            WHERE path = ?1
              AND state != 'deleted'",
            params![path.as_str(), size_bytes as i64, modified_at],
        )?;
        Ok(())
    }

    pub fn stats(&self, now: i64) -> Result<StoreStats> {
        let alive_files = self.count("state = 'alive'", [])?;
        let expired_recoverable_files = self.count(
            "state = 'expired' AND recovery_deadline IS NOT NULL AND recovery_deadline > ?1",
            params![now],
        )?;
        let deleted_files = self.count("state = 'deleted'", [])?;
        let pending_deletion_files = self.count(
            "state = 'expired' AND recovery_deadline IS NOT NULL AND recovery_deadline <= ?1",
            params![now],
        )?;
        let pending_deletion_bytes = self.sum_size(
            "state = 'expired' AND recovery_deadline IS NOT NULL AND recovery_deadline <= ?1",
            params![now],
        )?;

        Ok(StoreStats {
            alive_files,
            expired_recoverable_files,
            deleted_files,
            pending_deletion_files,
            pending_deletion_bytes,
            last_reaper_run_at: self.runtime_i64("last_reaper_run_at")?,
            last_reaper_duration_ms: self.runtime_u64("last_reaper_duration_ms")?,
            last_reaper_error: self.runtime_string("last_reaper_error")?,
        })
    }

    pub fn record_reaper_run(
        &self,
        run_at: i64,
        duration_ms: u64,
        error: Option<&str>,
    ) -> Result<()> {
        self.set_runtime_value("last_reaper_run_at", &run_at.to_string())?;
        self.set_runtime_value("last_reaper_duration_ms", &duration_ms.to_string())?;
        match error {
            Some(error) => self.set_runtime_value("last_reaper_error", error)?,
            None => self.delete_runtime_value("last_reaper_error")?,
        }
        Ok(())
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at INTEGER NOT NULL
            );

            INSERT OR IGNORE INTO schema_migrations (version, applied_at)
            VALUES (1, strftime('%s', 'now'));

            CREATE TABLE IF NOT EXISTS files (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                backing_path TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                modified_at INTEGER NOT NULL,
                ttl_seconds INTEGER,
                expires_at INTEGER,
                recovery_deadline INTEGER,
                state TEXT NOT NULL CHECK (state IN ('alive', 'expired', 'recovered', 'deleted')),
                policy_source TEXT NOT NULL,
                size_bytes INTEGER NOT NULL DEFAULT 0 CHECK (size_bytes >= 0)
            );

            CREATE INDEX IF NOT EXISTS idx_files_state_expires_at
            ON files(state, expires_at);

            CREATE INDEX IF NOT EXISTS idx_files_state_recovery_deadline
            ON files(state, recovery_deadline);

            CREATE UNIQUE INDEX IF NOT EXISTS idx_files_active_path
            ON files(path)
            WHERE state != 'deleted';

            CREATE UNIQUE INDEX IF NOT EXISTS idx_files_active_backing_path
            ON files(backing_path)
            WHERE state != 'deleted';

            CREATE TABLE IF NOT EXISTS runtime_state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS pending_renames (
                from_path TEXT PRIMARY KEY,
                to_path TEXT NOT NULL UNIQUE,
                is_directory INTEGER NOT NULL CHECK (is_directory IN (0, 1))
            );

            INSERT OR IGNORE INTO schema_migrations (version, applied_at)
            VALUES (2, strftime('%s', 'now'));
            ",
        )?;
        Ok(())
    }

    fn count<P>(&self, predicate: &str, params: P) -> Result<u64>
    where
        P: rusqlite::Params,
    {
        let sql = format!("SELECT COUNT(*) FROM files WHERE {predicate}");
        let count: i64 = self.conn.query_row(&sql, params, |row| row.get(0))?;
        Ok(count.max(0) as u64)
    }

    fn sum_size<P>(&self, predicate: &str, params: P) -> Result<u64>
    where
        P: rusqlite::Params,
    {
        let sql = format!("SELECT COALESCE(SUM(size_bytes), 0) FROM files WHERE {predicate}");
        let bytes: i64 = self.conn.query_row(&sql, params, |row| row.get(0))?;
        Ok(bytes.max(0) as u64)
    }

    fn runtime_string(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM runtime_state WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(FadeError::from)
    }

    fn runtime_i64(&self, key: &str) -> Result<Option<i64>> {
        Ok(self
            .runtime_string(key)?
            .and_then(|value| value.parse::<i64>().ok()))
    }

    fn runtime_u64(&self, key: &str) -> Result<Option<u64>> {
        Ok(self
            .runtime_string(key)?
            .and_then(|value| value.parse::<u64>().ok()))
    }

    fn set_runtime_value(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO runtime_state (key, value)
            VALUES (?1, ?2)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    fn delete_runtime_value(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM runtime_state WHERE key = ?1", params![key])?;
        Ok(())
    }

    fn audit_backing_dir(&self) -> Option<&Path> {
        let metadata_dir = self.db_path.parent()?;
        (metadata_dir.file_name()?.to_str()? == ".fade")
            .then(|| metadata_dir.parent())
            .flatten()
    }
}

fn read_record(row: &Row<'_>) -> rusqlite::Result<FileRecord> {
    let path: String = row.get(1)?;
    let backing_path: String = row.get(2)?;
    let state: String = row.get(8)?;
    let size_bytes: i64 = row.get(10)?;

    Ok(FileRecord {
        id: row.get(0)?,
        path: RelativePath::new(path).map_err(to_sql_conversion_error(1))?,
        backing_path: RelativePath::new(backing_path).map_err(to_sql_conversion_error(2))?,
        created_at: row.get(3)?,
        modified_at: row.get(4)?,
        ttl_seconds: row.get(5)?,
        expires_at: row.get(6)?,
        recovery_deadline: row.get(7)?,
        state: FileState::from_str(&state).map_err(to_sql_conversion_error(8))?,
        policy_source: row.get(9)?,
        size_bytes: u64::try_from(size_bytes)
            .map_err(|_| to_sql_conversion_error(10)(FadeError::InvalidSize(size_bytes)))?,
    })
}

fn to_sql_conversion_error(
    index: usize,
) -> impl FnOnce(FadeError) -> rusqlite::Error {
    move |error| rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::tempdir;

    use super::*;
    use crate::duration::Ttl;

    #[test]
    fn persists_and_loads_file_records() {
        let temp = tempdir().unwrap();
        let store = MetadataStore::open(temp.path().join("metadata.sqlite")).unwrap();
        let record = sample_record("24h/report.json", 100, Duration::from_secs(3_600));

        store.insert_file(&record).unwrap();
        let loaded = store.get_by_path(&record.path).unwrap().unwrap();

        assert_eq!(loaded.path, record.path);
        assert_eq!(loaded.ttl_seconds, Some(3_600));
        assert_eq!(loaded.state, FileState::Alive);
    }

    #[test]
    fn expires_due_records() {
        let temp = tempdir().unwrap();
        let store = MetadataStore::open(temp.path().join("metadata.sqlite")).unwrap();
        let record = sample_record("1s/report.json", 100, Duration::from_secs(1));

        store.insert_file(&record).unwrap();
        assert_eq!(store.expire_due(100).unwrap(), 0);
        assert_eq!(store.expire_due(101).unwrap(), 1);

        let loaded = store.get_by_path(&record.path).unwrap().unwrap();
        assert_eq!(loaded.state, FileState::Expired);
    }

    #[test]
    fn reports_pending_deletion_after_recovery_deadline() {
        let temp = tempdir().unwrap();
        let store = MetadataStore::open(temp.path().join("metadata.sqlite")).unwrap();
        let record = sample_record("1s/report.json", 512, Duration::from_secs(1));

        store.insert_file(&record).unwrap();
        store.expire_due(101).unwrap();

        let recoverable = store.stats(101).unwrap();
        assert_eq!(recoverable.expired_recoverable_files, 1);
        assert_eq!(recoverable.pending_deletion_files, 0);

        let pending = store.stats(3_701).unwrap();
        assert_eq!(pending.expired_recoverable_files, 0);
        assert_eq!(pending.pending_deletion_files, 1);
        assert_eq!(pending.pending_deletion_bytes, 512);
    }

    #[test]
    fn allows_recreating_deleted_paths() {
        let temp = tempdir().unwrap();
        let store = MetadataStore::open(temp.path().join("metadata.sqlite")).unwrap();
        let first = sample_record("1h/output.txt", 10, Duration::from_secs(3_600));
        let mut second = sample_record("1h/output.txt", 20, Duration::from_secs(3_600));

        store.insert_file(&first).unwrap();
        store.mark_deleted_by_path(&first.path).unwrap();
        second.created_at = first.created_at + 1;
        second.modified_at = first.modified_at + 1;
        store.insert_file(&second).unwrap();

        let loaded = store.get_by_path(&first.path).unwrap().unwrap();
        assert_eq!(loaded.id, second.id);
        assert_eq!(loaded.state, FileState::Alive);
    }

    #[test]
    fn allows_recovery_only_before_the_deadline() {
        let temp = tempdir().unwrap();
        let store = MetadataStore::open(temp.path().join("metadata.sqlite")).unwrap();
        let record = sample_record("1s/report.json", 10, Duration::from_secs(1));
        store.insert_file(&record).unwrap();
        store.expire_due(101).unwrap();

        assert!(store.is_recoverable(&record.id, 3_700).unwrap());
        assert!(!store.is_recoverable(&record.id, 3_701).unwrap());
    }

    #[test]
    fn renames_active_file_records() {
        let temp = tempdir().unwrap();
        let store = MetadataStore::open(temp.path().join("metadata.sqlite")).unwrap();
        let from = sample_record("1h/from.txt", 10, Duration::from_secs(3_600));
        let to = RelativePath::new("1h/to.txt").unwrap();

        store.insert_file(&from).unwrap();
        store.rename_path(&from.path, &to, 200).unwrap();

        assert!(store.get_by_path(&from.path).unwrap().is_none());
        let loaded = store.get_by_path(&to).unwrap().unwrap();
        assert_eq!(loaded.path, to);
        assert_eq!(loaded.backing_path, to);
        assert_eq!(loaded.modified_at, 200);
    }

    #[test]
    fn renames_directory_prefix_records() {
        let temp = tempdir().unwrap();
        let store = MetadataStore::open(temp.path().join("metadata.sqlite")).unwrap();
        let first = sample_record("1h/old/a.txt", 10, Duration::from_secs(3_600));
        let second = sample_record("1h/old/nested/b.txt", 20, Duration::from_secs(3_600));

        store.insert_file(&first).unwrap();
        store.insert_file(&second).unwrap();
        store
            .rename_prefix(
                &RelativePath::new("1h/old").unwrap(),
                &RelativePath::new("24h/new").unwrap(),
                200,
            )
            .unwrap();

        let moved_first = RelativePath::new("24h/new/a.txt").unwrap();
        let moved_second = RelativePath::new("24h/new/nested/b.txt").unwrap();
        assert!(store.get_by_path(&first.path).unwrap().is_none());
        assert_eq!(
            store.get_by_path(&moved_first).unwrap().unwrap().path,
            moved_first
        );
        assert_eq!(
            store.get_by_path(&moved_second).unwrap().unwrap().path,
            moved_second
        );
    }

    fn sample_record(path: &str, size_bytes: u64, ttl: Duration) -> FileRecord {
        let path = RelativePath::new(path).unwrap();
        FileRecord::new(
            path.clone(),
            path,
            Ttl::Duration(ttl),
            "folder:1s".to_string(),
            100,
            Duration::from_secs(3_600),
            size_bytes,
        )
        .unwrap()
    }
}
