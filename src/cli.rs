use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing_subscriber::EnvFilter;

use crate::clock::now_unix_seconds;
use crate::duration::{format_ttl_seconds, parse_duration};
use crate::layout::{init_backing_dir, looks_like_backing_dir, metadata_db_path};
use crate::metadata::{FileRecord, MetadataStore, StoreStats};
use crate::path::RelativePath;
use crate::policy::{DevPolicy, Policy};
use crate::reaper;

#[derive(Debug, Parser)]
#[command(name = "fade", version, about)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Mount a Fade filesystem.
    Mount {
        backing_dir: PathBuf,
        mountpoint: PathBuf,

        #[arg(long, value_enum, default_value_t = MountMode::Dev)]
        mode: MountMode,

        #[arg(long)]
        config: Option<PathBuf>,

        #[arg(long)]
        recovery_window: Option<String>,

        #[arg(long)]
        reaper_interval: Option<String>,
    },

    /// List files tracked by Fade metadata.
    Ls {
        path: PathBuf,

        #[arg(long)]
        json: bool,
    },

    /// Show lifecycle counters and reaper state.
    Status {
        path: PathBuf,

        #[arg(long)]
        json: bool,
    },

    /// Run one immediate garbage-collection pass.
    Gc {
        path: PathBuf,

        #[arg(long)]
        json: bool,
    },

    /// Preview the policy assignment for a path without mounting.
    Check {
        #[arg(long)]
        config: PathBuf,

        #[arg(long)]
        path: PathBuf,

        #[arg(long)]
        json: bool,
    },

    /// Print the Fade version.
    Version,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum MountMode {
    Dev,
    Policy,
}

pub fn run() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command {
        Command::Mount {
            backing_dir,
            mountpoint,
            mode,
            config,
            recovery_window,
            reaper_interval,
        } => mount(
            &backing_dir,
            &mountpoint,
            mode,
            config.as_deref(),
            recovery_window.as_deref(),
            reaper_interval.as_deref(),
        ),
        Command::Ls { path, json } => ls(&path, json),
        Command::Status { path, json } => status(&path, json),
        Command::Gc { path, json } => gc(&path, json),
        Command::Check { config, path, json } => check(&config, &path, json),
        Command::Version => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn mount(
    backing_dir: &Path,
    mountpoint: &Path,
    mode: MountMode,
    config: Option<&Path>,
    recovery_window: Option<&str>,
    reaper_interval: Option<&str>,
) -> anyhow::Result<()> {
    let policy = match mode {
        MountMode::Dev => {
            if config.is_some() {
                bail!("--config requires --mode policy");
            }
            Policy::Dev(DevPolicy)
        }
        MountMode::Policy => {
            let config = config.ok_or_else(|| anyhow!("--mode policy requires --config <file>"))?;
            Policy::from_config(config)
                .with_context(|| format!("failed to load policy `{}`", config.display()))?
        }
    };

    let recovery_window = mount_duration(
        recovery_window,
        "recovery-window",
        true,
        policy.recovery_window(),
        std::time::Duration::from_secs(3_600),
    )?;
    let reaper_interval = mount_duration(
        reaper_interval,
        "reaper-interval",
        false,
        policy.reaper_interval(),
        std::time::Duration::from_secs(60),
    )?;

    init_backing_dir(backing_dir)
        .with_context(|| format!("failed to initialize `{}`", backing_dir.display()))?;
    MetadataStore::open(metadata_db_path(backing_dir))
        .with_context(|| format!("failed to open metadata for `{}`", backing_dir.display()))?;

    #[cfg(feature = "fuse")]
    {
        return crate::fuse_fs::mount(
            backing_dir,
            mountpoint,
            policy,
            recovery_window,
            reaper_interval,
        )
        .with_context(|| format!("failed to mount `{}`", mountpoint.display()));
    }

    #[cfg(not(feature = "fuse"))]
    {
        let _ = mountpoint;
        let _ = policy;
        let _ = recovery_window;
        let _ = reaper_interval;
        bail!("this binary was built without FUSE support")
    }
}

fn mount_duration(
    flag: Option<&str>,
    name: &str,
    allow_zero: bool,
    config: Option<std::time::Duration>,
    default: std::time::Duration,
) -> anyhow::Result<std::time::Duration> {
    flag.map(|value| {
        parse_duration(value, allow_zero).with_context(|| format!("invalid --{name} `{value}`"))
    })
    .transpose()
    .map(|value| value.or(config).unwrap_or(default))
}

fn check(config: &Path, path: &Path, json: bool) -> anyhow::Result<()> {
    let path = RelativePath::new(path)?;
    if path.is_root() {
        bail!("--path must identify a file");
    }
    let assignment = Policy::from_config(config)
        .with_context(|| format!("failed to load policy `{}`", config.display()))?
        .assign_file(&path)?;
    let result = CheckView {
        path: path.as_str().to_string(),
        ttl: assignment.ttl.label(),
        ttl_seconds: assignment.ttl.ttl_seconds(),
        policy_source: assignment.source.as_label(),
    };

    if json {
        print_json(&result)
    } else {
        println!("path: {}", result.path);
        println!("ttl: {}", result.ttl);
        println!("policy_source: {}", result.policy_source);
        Ok(())
    }
}

fn ls(path: &Path, json: bool) -> anyhow::Result<()> {
    let backing_dir = resolve_backing_dir(path)?;
    let store = open_store(&backing_dir)?;
    let now = now_unix_seconds();
    store.expire_due(now)?;
    let records = store.list_files(false)?;

    if json {
        print_json(&records.into_iter().map(FileView::from).collect::<Vec<_>>())
    } else {
        print_file_table(&records);
        Ok(())
    }
}

fn status(path: &Path, json: bool) -> anyhow::Result<()> {
    let backing_dir = resolve_backing_dir(path)?;
    let store = open_store(&backing_dir)?;
    let now = now_unix_seconds();
    store.expire_due(now)?;
    let stats = StatusView::new(
        backing_dir,
        store.db_path().to_path_buf(),
        store.stats(now)?,
    );

    if json {
        print_json(&stats)
    } else {
        println!("alive_files: {}", stats.alive_files);
        println!(
            "expired_recoverable_files: {}",
            stats.expired_recoverable_files
        );
        println!("pending_deletion_files: {}", stats.pending_deletion_files);
        println!("pending_deletion_bytes: {}", stats.pending_deletion_bytes);
        println!("deleted_files: {}", stats.deleted_files);
        println!(
            "last_reaper_run_at: {}",
            stats.last_reaper_run_at.as_deref().unwrap_or("never")
        );
        println!(
            "last_reaper_duration_ms: {}",
            stats
                .last_reaper_duration_ms
                .map(|value| value.to_string())
                .unwrap_or_else(|| "n/a".to_string())
        );
        println!(
            "last_reaper_error: {}",
            stats.last_reaper_error.as_deref().unwrap_or("none")
        );
        println!("metadata_db_path: {}", stats.metadata_db_path.display());
        println!(
            "backing_directory_path: {}",
            stats.backing_directory_path.display()
        );
        Ok(())
    }
}

fn gc(path: &Path, json: bool) -> anyhow::Result<()> {
    let backing_dir = resolve_backing_dir(path)?;
    let store = open_store(&backing_dir)?;
    let report = reaper::run_once(&store, &backing_dir, now_unix_seconds())?;

    if json {
        print_json(&report)
    } else {
        println!("expired_files: {}", report.expired_files);
        println!("deleted_files: {}", report.deleted_files);
        println!("missing_backing_files: {}", report.missing_backing_files);
        println!("reclaimed_bytes: {}", report.reclaimed_bytes);
        println!("duration_ms: {}", report.duration_ms);
        Ok(())
    }
}

fn open_store(backing_dir: &Path) -> anyhow::Result<MetadataStore> {
    MetadataStore::open(metadata_db_path(backing_dir)).with_context(|| {
        format!(
            "failed to open Fade metadata under `{}`",
            backing_dir.display()
        )
    })
}

fn resolve_backing_dir(path: &Path) -> anyhow::Result<PathBuf> {
    if looks_like_backing_dir(path) {
        return Ok(path.to_path_buf());
    }

    Err(anyhow!(
        "`{}` does not look like a Fade backing directory; expected {}/{}",
        path.display(),
        crate::layout::FADE_DIR,
        crate::layout::METADATA_DB
    ))
}

fn print_file_table(records: &[FileRecord]) {
    println!(
        "{:<48} {:<12} {:<10} {:<24} {:<24} {:>10}",
        "PATH", "STATE", "TTL", "EXPIRES", "RECOVERY_DEADLINE", "SIZE"
    );
    for record in records {
        println!(
            "{:<48} {:<12} {:<10} {:<24} {:<24} {:>10}",
            record.path.as_str(),
            record.state.as_str(),
            format_ttl_seconds(record.ttl_seconds),
            record
                .expires_at
                .map(format_timestamp)
                .unwrap_or_else(|| "never".to_string()),
            record
                .recovery_deadline
                .map(format_timestamp)
                .unwrap_or_else(|| "never".to_string()),
            record.size_bytes,
        );
    }
}

fn print_json(value: &impl Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn format_timestamp(timestamp: i64) -> String {
    let Ok(time) = OffsetDateTime::from_unix_timestamp(timestamp) else {
        return timestamp.to_string();
    };

    time.format(&Rfc3339)
        .unwrap_or_else(|_| timestamp.to_string())
}

#[derive(Debug, Serialize)]
struct FileView {
    path: String,
    backing_path: String,
    state: String,
    ttl: String,
    ttl_seconds: Option<i64>,
    created_at: String,
    modified_at: String,
    expires_at: Option<String>,
    recovery_deadline: Option<String>,
    policy_source: String,
    size_bytes: u64,
}

#[derive(Debug, Serialize)]
struct CheckView {
    path: String,
    ttl: String,
    ttl_seconds: Option<i64>,
    policy_source: String,
}

impl From<FileRecord> for FileView {
    fn from(record: FileRecord) -> Self {
        Self {
            path: record.path.as_str().to_string(),
            backing_path: record.backing_path.as_str().to_string(),
            state: record.state.as_str().to_string(),
            ttl: format_ttl_seconds(record.ttl_seconds),
            ttl_seconds: record.ttl_seconds,
            created_at: format_timestamp(record.created_at),
            modified_at: format_timestamp(record.modified_at),
            expires_at: record.expires_at.map(format_timestamp),
            recovery_deadline: record.recovery_deadline.map(format_timestamp),
            policy_source: record.policy_source,
            size_bytes: record.size_bytes,
        }
    }
}

#[derive(Debug, Serialize)]
struct StatusView {
    alive_files: u64,
    expired_recoverable_files: u64,
    deleted_files: u64,
    pending_deletion_files: u64,
    pending_deletion_bytes: u64,
    last_reaper_run_at: Option<String>,
    last_reaper_duration_ms: Option<u64>,
    last_reaper_error: Option<String>,
    metadata_db_path: PathBuf,
    backing_directory_path: PathBuf,
    config_path: Option<PathBuf>,
    config_hash: Option<String>,
}

impl StatusView {
    fn new(backing_dir: PathBuf, metadata_db_path: PathBuf, stats: StoreStats) -> Self {
        Self {
            alive_files: stats.alive_files,
            expired_recoverable_files: stats.expired_recoverable_files,
            deleted_files: stats.deleted_files,
            pending_deletion_files: stats.pending_deletion_files,
            pending_deletion_bytes: stats.pending_deletion_bytes,
            last_reaper_run_at: stats.last_reaper_run_at.map(format_timestamp),
            last_reaper_duration_ms: stats.last_reaper_duration_ms,
            last_reaper_error: stats.last_reaper_error,
            metadata_db_path,
            backing_directory_path: backing_dir,
            config_path: None,
            config_hash: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn mount_flags_override_config_and_defaults() {
        let configured = Duration::from_secs(30);
        let default = Duration::from_secs(60);

        assert_eq!(
            mount_duration(
                Some("1s"),
                "reaper-interval",
                false,
                Some(configured),
                default
            )
            .unwrap(),
            Duration::from_secs(1)
        );
        assert_eq!(
            mount_duration(None, "reaper-interval", false, Some(configured), default).unwrap(),
            configured
        );
        assert_eq!(
            mount_duration(None, "reaper-interval", false, None, default).unwrap(),
            default
        );
    }
}
