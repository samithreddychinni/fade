use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use serde::Serialize;

use crate::Result;
use crate::layout::metadata_dir;
use crate::path::RelativePath;

#[derive(Serialize)]
struct AuditEvent<'a> {
    timestamp: i64,
    event: &'a str,
    path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    policy_source: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    destination: Option<&'a Path>,
}

pub fn append(
    backing_dir: &Path,
    timestamp: i64,
    event: &str,
    path: &RelativePath,
    policy_source: Option<&str>,
    destination: Option<&Path>,
) -> Result<()> {
    let mut entry = serde_json::to_vec(&AuditEvent {
        timestamp,
        event,
        path: path.as_str(),
        policy_source,
        destination,
    })
    .map_err(std::io::Error::other)?;
    entry.push(b'\n');

    let mut audit = OpenOptions::new()
        .append(true)
        .create(true)
        .open(metadata_dir(backing_dir).join("audit.jsonl"))?;
    audit.write_all(&entry)?;
    audit.sync_data()?;
    Ok(())
}
