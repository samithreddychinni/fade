# Fade

Fade gives Linux directories a time-to-live.

Most filesystems treat time as metadata, not policy. A file created for a
temporary job can sit on disk for years unless an application, cron job, or
human remembers to remove it. Fade moves that cleanup decision closer to the
storage boundary: files receive a time-to-live, and the filesystem enforces
that lifetime.

Status: v0.1.0 is a developer preview. Policy mode now supports ordered TOML
rules. Do not use Fade for important or irreplaceable data.

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
- End-to-end FUSE lifecycle coverage for expiry, garbage collection, and
  remount persistence.
- Startup reconciliation for interrupted create, rename, and delete operations.
- Policy-mode mounts with ordered TOML glob rules.
- `fade check` policy previews without mounting.
- Recovery of expired files during the configured recovery window.
- JSON Lines audit events for file creation, expiry, recovery, and deletion.

Not implemented yet:

- Mountpoint discovery for inspection commands. For now `fade ls`, `fade
  status`, and `fade gc` operate on the Fade backing directory.

## Install

Requirements:

- Linux.
- Rust 1.95 or newer.
- FUSE runtime support, including `fusermount3` or `fusermount`.

Install Fade for the current user:

```bash
./install.sh
```

The script installs `fade` in `~/.local/bin`. Add that directory to `PATH` if
you need to. To remove Fade, run:

```bash
./install.sh --uninstall
```

Use `FADE_PREFIX` to install in another prefix:

```bash
FADE_PREFIX=/opt/fade ./install.sh
```

## Build From Source

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
./tests/fuse_lifecycle.sh # requires /dev/fuse
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

Build artifacts, exported reports, test fixtures, generated archives, and local
scratch data often start with an obvious lifetime. The
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

The goal is a small filesystem primitive for local data that belongs on disk
for a known amount of time. Fade does not replace a secret manager, object
storage lifecycle policy, backup policy, or compliance program.

## Why a filesystem?

Linux already has good cleanup tools. `systemd-tmpfiles` removes old files when
a cleanup pass runs, and `tmpfs` drops an entire in-memory filesystem when it is
unmounted. Use either when those semantics are enough.

Fade is for the narrower case where a path must stop resolving on the next
Fade-mediated access after its TTL expires, even if physical deletion happens
later. Applications keep using normal file operations; the retention rule lives
at the mount boundary.

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

## Startup recovery

Fade checks metadata and backing files before each mount.

- Fade marks a tracked file as deleted when its backing file is missing.
- Fade completes a recorded rename when the destination exists.
- Fade cancels a recorded rename when the source still exists.
- Fade refuses to mount when it finds an untracked backing file.
- Fade refuses to mount when a recorded rename has an ambiguous disk state.

Fade does not import unknown files. It cannot know their original TTL. Remove
or move the file before you mount Fade again.

The lifecycle is:

```text
created -> alive -> expired -> recoverable -> deleted
```

The recovery window is configurable. Strict deployments can set it to `0` so
expired files are eligible for deletion immediately.

## Usage model

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
created. The final rule must be `*`, so every file receives a TTL. Reaper
settings in the config override the built-in defaults; mount flags override the
config.

Start a policy-mode mount with:

```bash
fade mount ./fade-data ./mnt --mode policy --config fade.toml
```

Preview a rule assignment without mounting:

```bash
fade check --config fade.toml --path reports/session.token
```

### Inspection

The implemented inspection commands expose expiry without requiring operators
to inspect the SQLite store directly:

```bash
fade ls ./fade-data
fade status ./fade-data
fade gc ./fade-data
```

Mountpoint discovery is planned.

## Command reference

`fade mount <backing-dir> <mountpoint>` starts a developer-mode mount. Use
`--mode policy --config <file>` for TOML rules. Use `--recovery-window
<duration>` to set the physical deletion delay. Use `--reaper-interval
<duration>` to set the reaper interval. Durations use `s`, `m`, `h`, or `d`.
Use `0` for no recovery delay.

`fade ls <backing-dir>` shows tracked files. `fade status <backing-dir>` shows
counts and the last reaper result. `fade gc <backing-dir>` runs one reaper pass.
Use `--json` with any inspection command for machine-readable output.

`fade check --config <file> --path <relative-file>` previews the first matching
policy rule without mounting or changing metadata. Use `--json` for
machine-readable output.

`fade recover <backing-dir> <relative-file> --to <output>` exports an expired
file before its recovery deadline. The original remains expired and Fade refuses
to overwrite an existing output. Successful recoveries are appended to
`.fade/audit.jsonl`.

`fade version` prints the installed version.

## Initial use cases

Fade is intentionally narrow at first:

- Local scratch directories that should clean themselves.
- Temporary exports and reports.
- Build and CI workspaces whose output has a known maximum age.
- Generated test data that is safe to recreate.

The first public release should win these cases before expanding into
service policy or security-sensitive data. Fade can expire files inside its
mount. It cannot delete copies from logs, backups, snapshots, shell history,
editor swap files, or external systems.

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

## Known filesystem limits

Use regular files and directories in a Fade mount. Fade hides symlinks and does
not support hard links, ownership changes, permission changes, or timestamp
changes. Do not rely on `mmap`, `fsync`, or path-level truncate behavior.

Data that the kernel or an application already cached can outlive expiry. Fade
blocks later path lookups and open-handle reads and writes after expiry.

## Troubleshooting

If mounting reports a missing FUSE device, load the `fuse` kernel module. Make
sure `/dev/fuse` exists and `fusermount3` or `fusermount` is installed.

If startup reports an untracked backing file, move or remove that file from the
backing directory. Fade refuses to assign a TTL without metadata.

If unmount fails, close programs that use the mount. Then run:

```bash
fusermount3 -u <mountpoint>
```

Use `fusermount -u <mountpoint>` when your system provides FUSE 2.

## Design principles

- Expiry should be the default, not an afterthought.
- The filesystem should enforce policy at access time.
- Production behavior should be observable and testable before rollout.
- Recovery should be explicit, time-bounded, and easy to disable.
- The tool should be honest about what it can and cannot guarantee.
- Application code should not need to learn a new storage API.

## Planned architecture

Fade is implemented as a Linux FUSE filesystem written in Rust.

The mounted filesystem will store file contents in a backing directory and TTL
metadata in SQLite. Every filesystem operation that resolves a path will check
metadata before returning file data or attributes. A reaper loop will scan for
expired records and delete eligible backing files.

High-level components:

- FUSE mount: path resolution, reads, writes, stats, renames, and directory
  listing behavior.
- Policy engine: TTL folder parsing and ordered config-rule matching.
- Metadata store: SQLite records for path, TTL, creation time, expiry time,
  recovery deadline, state, and backing object location.
- Reaper: periodic and manually triggered physical cleanup.
- CLI: mount, inspect, and garbage-collect operations.

Rust stack:

- `fuser` for FUSE integration.
- `clap` for CLI parsing.
- `rusqlite` for SQLite metadata.
- `serde` for machine-readable output.
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
- `fade recover` for the existing recovery window.

### v0.3: Production Hardening

- Structured audit log.
- Rename, symlink, hard link, and open-file semantics documented and tested.
- Packaging for common Linux distributions.
- Benchmarks against direct filesystem access.

## Open design questions

These questions should be answered before claiming production readiness.

- How should Fade handle buffered I/O and `mmap`, where data may already be in
  the kernel or process address space when the TTL expires?
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
