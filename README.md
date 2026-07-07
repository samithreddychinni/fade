# Fade

Fade is a self-expiring filesystem for files that should not live forever.

Most filesystems treat time as metadata, not policy. A file created for a
temporary job can sit on disk for years unless an application, cron job, or
human remembers to remove it. Fade moves that cleanup decision closer to the
storage boundary: files receive a time-to-live, and the filesystem enforces
that lifetime.

Status: phase 1 implementation in progress. The repository now contains the
first Rust implementation slice for the local developer-mode MVP. The public
contract below still describes the target behavior for the first
production-quality release.

## Current Implementation

Implemented in the current phase 1 slice:

- Rust CLI crate with `fade mount`, `fade ls`, `fade status`, `fade gc`, and
  `fade version`.
- Developer-mode FUSE mount with TTL folders such as `1m`, `1h`, `24h`, `7d`,
  `30d`, and `forever`.
- SQLite metadata for path, TTL, expiry time, recovery deadline, lifecycle
  state, policy source, and observed size.
- Expiry checks for lookup, stat, open, read, write, rename, unlink, and normal
  directory listings.
- Background reaper plus manual `fade gc` for physically deleting expired files
  after the recovery window.
- Unit coverage for duration parsing, path safety, TTL assignment, metadata
  lifecycle transitions, record rename/recreate behavior, and reaper deletion.

Not implemented yet:

- Policy mode and TOML rule matching.
- `fade check` dry run.
- `fade recover`.
- Mountpoint discovery for inspection commands. For now `fade ls`, `fade
  status`, and `fade gc` operate on the Fade backing directory.
- End-to-end mount tests in CI. They require a Linux host with `/dev/fuse`
  available.

## Quickstart From Source

Requirements:

- Linux.
- Rust 1.95 or newer.
- FUSE runtime support for mounting, including `/dev/fuse` and `fusermount3` or
  `fusermount`.

Build and test:

```bash
cargo test
cargo test --no-default-features
cargo build
```

Run a developer-mode mount:

```bash
mkdir -p /tmp/fade-data /tmp/fade-mnt
target/debug/fade mount /tmp/fade-data /tmp/fade-mnt --mode dev
```

The mount command stays in the foreground. In another terminal:

```bash
printf 'hello\n' > /tmp/fade-mnt/1m/hello.txt
cat /tmp/fade-mnt/1m/hello.txt

target/debug/fade ls /tmp/fade-data
target/debug/fade status /tmp/fade-data
target/debug/fade gc /tmp/fade-data
```

Unmount when finished:

```bash
fusermount3 -u /tmp/fade-mnt
```

## Why Fade exists

Temporary files become permanent by accident.

Build artifacts, exported reports, test fixtures, short-lived credentials,
session dumps, and local scratch data often start with an obvious lifetime. The
problem is that the lifetime is rarely attached to the file itself. Cleanup gets
implemented later as a script, a scheduled job, or a runbook. Those approaches
work until they are skipped, misconfigured, or forgotten.

Fade makes expiry part of the filesystem experience:

- New files get a TTL when they are created.
- Expired files are no longer visible or readable through the mounted
  filesystem.
- A background reaper removes expired bytes after the configured recovery
  window.
- Operators can inspect what is alive, expired, recoverable, and pending
  deletion.

The goal is not to replace secret managers, object storage lifecycle policies,
or compliance programs. The goal is to provide a small, reliable filesystem
primitive for local and server-side data that already belongs on disk, but
should not stay there indefinitely.

## Core model

Fade has two layers of expiry.

1. Access enforcement: once a file expires, Fade returns `ENOENT` for normal
   filesystem operations through the mount. From the calling process's point of
   view, the file is gone.
2. Physical cleanup: a reaper process deletes expired backing files after the
   recovery window has passed, reclaiming disk space without a separate cleanup
   job.

This split gives applications immediate expiry semantics while still allowing a
short, explicit recovery path for mistakes.

The lifecycle is:

```text
created -> alive -> expired -> recoverable -> deleted
```

The recovery window is configurable. Strict deployments can set it to `0` so
expired files are eligible for deletion immediately.

## Planned usage

### Developer mode

Developer mode uses TTL folders. The folder name is the policy.

```bash
fade mount ./fade-data ./mnt --mode dev

cp report.json ./mnt/24h/
cp cache.tar ./mnt/7d/
cp notes.txt ./mnt/forever/
```

Files created under `24h/` live for 24 hours. Files created under `7d/` live for
7 days. No application code needs to pass flags or call a Fade API.

### Policy mode

Policy mode uses a config file. It is intended for services and shared
environments where expiry should be controlled centrally.

```toml
[[rules]]
pattern = "*.token"
ttl = "1h"

[[rules]]
pattern = "*.key"
ttl = "7d"

[[rules]]
pattern = "session_*"
ttl = "30m"

[[rules]]
pattern = "*"
ttl = "24h"

[reaper]
interval = "60s"
recovery_window = "1h"
```

Fade evaluates rules in order and assigns the first matching TTL when a file is
created. In policy mode, config wins over folder hints. The catch-all rule is
intentional: production mounts should not silently create files with unknown
retention.

### Inspection

Fade should make expiry visible without requiring operators to inspect the
SQLite store directly.

```bash
fade ls ./mnt
fade status ./mnt
fade check --config fade.toml --path ./sample-data
fade recover ./mnt/session_abc
fade gc ./mnt
```

`fade check` is a dry run. It shows which rule would apply to each file before a
team mounts Fade in a real environment.

## Initial use cases

Fade is intentionally narrow at first.

- Build and CI artifact caches with bounded disk usage.
- Local scratch directories that should clean themselves.
- Temporary exports and reports.
- Test data generated during development or continuous integration.
- Short-lived service files where the application can write directly into a
  Fade mount.
- Container-mounted temporary files that should not outlive the workload.

Security-sensitive use cases are in scope only when the limitations are
understood. Fade can expire files inside its mount. It cannot delete copies from
logs, backups, snapshots, shell history, editor swap files, or external systems.
Future releases may add per-file encryption and key destruction for stronger
erase semantics.

## What Fade is not

Fade is not a secret manager. Tools like Vault solve authentication,
authorization, dynamic secrets, revocation, and audit workflows that Fade does
not attempt to own.

Fade is not a backup system. Expiry and recovery windows are short-lived
operational features, not durable restore guarantees.

Fade is not a legal compliance product by itself. It can help enforce retention
inside a mounted path, but real compliance also depends on application behavior,
backups, snapshots, access controls, audit retention, and organizational policy.

Fade is not secure erase on every storage device. On SSDs, copy-on-write
filesystems, journaled filesystems, and snapshotting environments, overwriting
or deleting a file does not necessarily remove every physical copy.

## Design principles

- Expiry should be the default, not an afterthought.
- The filesystem should enforce policy at access time.
- Production behavior should be observable and testable before rollout.
- Recovery should be explicit, time-bounded, and easy to disable.
- The tool should be honest about what it can and cannot guarantee.
- Application code should not need to learn a new storage API.

## Planned architecture

Fade will be implemented as a Linux FUSE filesystem written in Rust.

The mounted filesystem will store file contents in a backing directory and TTL
metadata in SQLite. Every filesystem operation that resolves a path will check
metadata before returning file data or attributes. A reaper loop will scan for
expired records and delete eligible backing files.

High-level components:

- FUSE mount: path resolution, reads, writes, stats, renames, and directory
  listing behavior.
- Policy engine: TTL folder parsing and config-rule matching.
- Metadata store: SQLite records for path, TTL, creation time, expiry time,
  recovery deadline, state, and backing object location.
- Reaper: periodic and manually triggered physical cleanup.
- CLI: mount, inspect, dry-run, recover, and garbage-collect operations.
- Observability: structured logs and machine-readable status output.

Planned Rust stack:

- `fuser` for FUSE integration.
- `clap` for CLI parsing.
- `rusqlite` for SQLite metadata.
- `serde` and `toml` for configuration.
- `tracing` for structured logs.
- `thiserror` and `anyhow` for error boundaries.

## Roadmap

### v0.1: Local MVP

- Linux FUSE mount.
- TTL folders for developer mode.
- SQLite metadata.
- Expiry checks for lookup, stat, read, write, and directory listing.
- Background reaper.
- `fade ls`, `fade status`, and `fade gc`.
- Integration tests for basic file lifecycle behavior.

### v0.2: Policy Mode

- TOML config rules.
- `fade check` dry run.
- Config validation with clear error messages.
- Rule precedence tests.
- Recovery window support.

### v0.3: Production Hardening

- Structured audit log.
- Prometheus-compatible metrics or JSON status endpoint.
- Crash recovery and startup reconciliation.
- Rename, symlink, hard link, and open-file semantics documented and tested.
- Packaging for common Linux distributions.

### Later

- Per-file encryption and cryptographic erase.
- Kubernetes examples.
- Systemd unit examples.
- Benchmarks against direct filesystem access.

## Open design questions

These questions should be answered before claiming production readiness.

- What happens when a process opens a file before expiry and keeps the handle
  open after expiry?
- Should expired open handles fail immediately, continue until close, or be
  configurable?
- How should Fade handle `mmap`?
- Are hard links allowed, rejected, or represented as shared metadata?
- How are symlinks resolved, and can they escape the mount?
- What are the exact guarantees after crash, remount, and metadata recovery?
- Should file TTL be immutable after creation, or can policy changes shorten
  existing files?
- What is the safest default recovery window?

## Contributing

Fade is early. The best contributions right now are design review, Linux
filesystem edge cases, test scenarios, and small implementation slices that keep
the core behavior easy to reason about.

Project conventions for comments and commit messages live in
[CONTRIBUTING.md](CONTRIBUTING.md).
