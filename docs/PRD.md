# Fade Product Requirements Document

Start Date: 2026-05-03
Last Updated: 2026-07-17
Status: Working draft
Owner: Project maintainers

## Summary

Fade is a Linux FUSE filesystem that assigns files a time-to-live and enforces
expiry through normal filesystem operations. When a file expires, applications
accessing the Fade mount should observe it as missing. A reaper then removes the
backing bytes after a configured recovery window.

The first release should prove that expiry can be treated as filesystem policy
without requiring application-specific cleanup code.

## Problem

Many files are created with an implied lifetime, but the lifetime is not stored
or enforced with the file. Teams depend on cleanup scripts, cron jobs, manual
runbooks, or application code. These mechanisms are easy to miss and hard to
verify.

Common failure modes:

- Temporary artifacts accumulate until disks fill.
- Local development directories contain stale credentials or generated data.
- Test and CI output survives longer than intended.
- Retention policies depend on each application implementing cleanup correctly.
- Operators cannot easily see what data is pending deletion.

Fade addresses the filesystem-local part of this problem by attaching expiry to
files at creation time and enforcing it at access time.

## Goals for v0.1

- Provide a FUSE mount where files receive a TTL automatically.
- Make expired files inaccessible through the mounted filesystem.
- Reclaim disk space automatically through a background reaper.
- Support a simple developer workflow using TTL folders.
- Expose clear CLI inspection and cleanup commands.
- Persist metadata across restarts.
- Document operational limits honestly.

Policy rules, dry runs, and recovery commands are v0.2 work. They should not
delay a useful, testable TTL-folder release.

## Non-goals

- Replacing Vault or other secret managers.
- Providing universal secure erase guarantees on all filesystems and devices.
- Managing backup retention outside the Fade backing store.
- Supporting every operating system in the MVP.
- Building a distributed filesystem.
- Providing multi-node coordination in the MVP.
- Hiding data from users who can access the backing directory directly.

## Target users

### Local developer

Wants a scratch filesystem where temporary data disappears without writing
cleanup scripts. Values simple folder-based behavior and quick feedback.

### Build and platform engineer

Wants CI workspaces, generated reports, and recreatable artifacts to expire
predictably. Values simple operation and visible lifecycle state.

### Application operator (v0.2)

Wants a mounted path with retention behavior that does not depend on every
application code path remembering to delete files. Values config-driven policy,
logs, metrics, and predictable failure behavior.

## Primary use cases

### TTL scratch workspace

A developer mounts Fade locally and writes files into folders named `1h/`,
`24h/`, `7d/`, and `forever/`. Fade applies the TTL based on the folder and
expires files without extra commands.

### CI workspace

A CI worker stores generated output in a Fade mount. Output older than the
configured TTL becomes inaccessible and is physically removed by the reaper.

### Policy-controlled service output (v0.2)

A service writes files into a mounted directory. Fade assigns TTLs using a TOML
config, such as a first-match rule for `*.token` or `*.report`. The service
does not call a Fade-specific API.

### Recovery from accidental expiry (v0.2)

An operator discovers that a file expired unintentionally. If the recovery
window has not elapsed, `fade recover` restores the file to the alive state or
copies it to a recovery path.

## User experience requirements

### Developer mode

- Fade MUST support TTL folders such as `1m`, `1h`, `24h`, `7d`, `30d`, and
  `forever`.
- Fade MUST assign TTL at file creation time based on the nearest valid TTL
  folder.
- Fade SHOULD reject invalid TTL folder names with clear errors when they are
  used as policy folders.
- Fade MUST keep developer mode usable without requiring a config file.

### Policy mode (v0.2)

- Fade MUST support a TOML config file with ordered glob rules.
- Fade MUST assign the first matching rule to newly created files.
- Fade MUST provide a catch-all rule or require an explicit default TTL.
- Fade MUST fail startup on invalid durations, ambiguous config, or missing
  required policy.
- Config rules MUST take precedence over folder hints in policy mode.

Example:

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

### Expiry behavior

- Fade MUST treat expired files as missing for new lookup, stat, open, read,
  write, truncate, and rename operations through the mount.
- Fade MUST hide expired files from normal directory listings.
- Fade MUST record the transition from alive to expired in metadata.
- Fade MUST support a configurable recovery window.
- Fade MUST support recovery window `0`.
- Reads and writes through an already-open handle MUST fail after expiry.
- Fade MUST document that buffered data and `mmap` may outlive path-level
  access because the kernel or process may already hold a copy.

### Reaper behavior

- Fade MUST include a background reaper.
- The reaper MUST physically remove backing files whose recovery window has
  elapsed.
- The reaper MUST be idempotent.
- The reaper MUST tolerate missing backing files and stale metadata without
  crashing the mount.
- Fade MUST provide `fade gc` to trigger an immediate reaper pass.
- Fade MUST expose pending deletion counts and approximate pending bytes.

### Recovery behavior (v0.2)

- Fade MUST provide `fade recover` for files that are expired but still inside
  the recovery window.
- Fade MUST fail recovery with a clear error after the recovery deadline.
- Fade SHOULD support recovery either in place or to a separate output path.
- Fade MUST audit recovery events in policy mode.

### Inspection and dry run

- Fade MUST provide `fade ls` to show path, state, TTL, expiry time, and
  recovery deadline.
- Fade MUST provide `fade status` with machine-readable output.
- In v0.2, Fade MUST provide `fade check --config <file> --path <path>` to
  preview TTL assignment without mounting.
- Fade SHOULD support JSON output for automation.

## CLI requirements

Target command shape:

```bash
fade mount <backing-dir> <mountpoint> [--config fade.toml] [--mode dev|policy]
fade ls <mountpoint> [--json]
fade status <mountpoint> [--json]
fade check --config fade.toml --path ./data [--json]
fade recover <mountpoint-path> [--to ./recovered-file]
fade gc <mountpoint>
fade version
```

CLI behavior:

- Commands MUST return non-zero exit codes on failure.
- Human-readable errors MUST include the path and reason when practical.
- JSON output MUST be stable enough for scripts.
- Commands SHOULD avoid surprising destructive behavior.

## Functional requirements

### Metadata

Fade MUST persist metadata in SQLite.

Minimum fields:

- Stable file identifier.
- User-visible path.
- Backing object path.
- Creation time.
- Last known modification time.
- TTL duration.
- Expiry time.
- Recovery deadline.
- State: alive, expired, recovered, deleted.
- Policy source: folder, config rule, explicit default.
- Size at last observation.

Metadata requirements:

- Metadata writes MUST be atomic with respect to visible file creation whenever
  feasible.
- Startup MUST reconcile metadata with the backing directory.
- Missing metadata for an existing backing file MUST be handled explicitly:
  import, quarantine, or fail startup depending on mode.
- Metadata schema changes MUST be versioned.

### Filesystem operations

The MVP MUST cover:

- create
- open
- read
- write
- release
- lookup
- getattr/stat
- readdir
- rename
- unlink
- mkdir
- rmdir

The production-ready release SHOULD define behavior for:

- symlink
- readlink
- hard link
- chmod
- chown
- utime
- truncate
- fsync
- mmap

### Path and policy rules

- Policy matching MUST be deterministic.
- Rule precedence MUST be documented and tested.
- Paths MUST be normalized before policy matching.
- Fade MUST prevent path traversal into the backing store.
- Symlink behavior MUST avoid escaping the mount's policy boundary.

## Observability requirements

Fade MUST provide enough information for operators to trust the system.

Required v0.1 status fields:

- Alive file count.
- Expired recoverable file count.
- Deleted file count.
- Pending deletion bytes.
- Last reaper run time.
- Last reaper duration.
- Last reaper error.
- Metadata database path.
- Backing directory path.

Policy mode SHOULD also expose the config path and loaded config hash.

Required audit events in policy mode:

- file_created
- file_expired
- file_recovered
- file_deleted
- gc_started
- gc_completed
- gc_failed
- config_loaded
- startup_reconciliation_completed

Audit logs SHOULD be structured JSON Lines.

## Reliability requirements

- Fade MUST survive process restart without losing expiry metadata.
- On startup, Fade MUST expire files that crossed their TTL while unmounted.
- Reaper passes MUST be safe to retry.
- SQLite access MUST be serialized or coordinated safely.
- The mount MUST fail closed on metadata corruption in policy mode.
- Developer mode MAY offer a repair path for local experimentation.
- Fade SHOULD include integration tests that mount the filesystem and verify
  lifecycle behavior end to end.

## Security requirements

- Fade MUST assume that users with direct access to the backing directory can
  bypass the mount.
- Fade MUST document that deletion is not universal secure erase.
- Fade MUST prevent mount-path operations from escaping into arbitrary host
  paths.
- Fade SHOULD run with least privilege.
- Fade SHOULD avoid storing secret file contents in logs.
- Audit logs MUST avoid recording file contents.

## Performance requirements

MVP performance should be good enough for development and CI workloads.

Initial requirements:

- Metadata lookup overhead should be small enough for normal CLI usage and
  moderate artifact directories.
- Status commands should avoid full backing-store scans during normal operation.

Publish measured baselines before setting numeric performance targets or
claiming production performance.

## Compatibility requirements

MVP target:

- Linux.
- FUSE 3.
- Rust.
- SQLite.

Out of scope for MVP:

- macOS.
- Windows.
- Network filesystems as backing stores.
- Multi-node shared mounts.

## Failure modes

Fade MUST have documented behavior for:

- Metadata database unavailable.
- Backing directory unavailable.
- Mountpoint already mounted.
- Invalid config file.
- Clock jumps forward or backward.
- Reaper deletion failure.
- File exists on disk but metadata is missing.
- Metadata exists but backing file is missing.
- Process crash during create, rename, recovery, or delete.

## Acceptance criteria for v0.1

The v0.1 release is acceptable when:

- A user can mount Fade locally in developer mode.
- A file created under a TTL folder is readable before expiry.
- The same file becomes inaccessible through the mount after expiry.
- Expired files are hidden from directory listing.
- Metadata survives unmount and remount.
- Files that expire while Fade is offline are expired on next startup.
- `fade ls` shows alive and expired recoverable files.
- `fade gc` removes expired files whose recovery window elapsed.
- Integration tests cover the lifecycle above.
- README limitations are accurate for the implemented behavior.

## Acceptance criteria for v0.2

The v0.2 release is acceptable when:

- Policy mode accepts a TOML config with ordered glob rules.
- Invalid configs fail with clear errors.
- `fade check` previews rule assignment.
- Config rules override folder hints.
- `fade recover` restores an eligible expired file or copies it to a requested
  recovery path.
- Audit events are emitted for create, expire, recover, and delete.

## Launch plan

1. Build the smallest FUSE lifecycle demo.
2. Add SQLite metadata and tests.
3. Add developer mode TTL folders.
4. Add the reaper and `fade gc`.
5. Publish v0.1 with limitations and known edge cases.
6. Add policy mode and dry-run checks.
7. Harden filesystem semantics before marketing security-sensitive use cases.

## Documentation requirements

Before the first public release, docs MUST include:

- Quickstart.
- Installation.
- Example configs.
- Command reference.
- Lifecycle explanation.
- Operational limitations.
- Security model.
- Known filesystem semantics.
- Troubleshooting.
