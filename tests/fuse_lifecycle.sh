#!/usr/bin/env bash
set -euo pipefail

fade_bin=${FADE_BIN:-target/debug/fade}
test_dir=$(mktemp -d "${TMPDIR:-/tmp}/fade-fuse-test.XXXXXX")
backing_dir="$test_dir/backing"
mount_dir="$test_dir/mount"
mount_pid=
mounted=false
mount_args=()

cleanup() {
    if "$mounted"; then
        fusermount3 -u "$mount_dir" 2>/dev/null || true
    fi
    if [ -n "$mount_pid" ]; then
        wait "$mount_pid" 2>/dev/null || true
    fi
    rm -rf "$test_dir"
}
trap cleanup EXIT

start_mount() {
    "$fade_bin" mount "$backing_dir" "$mount_dir" \
        "${mount_args[@]}" \
        --recovery-window 0 --reaper-interval 1h >"$test_dir/mount.log" 2>&1 &
    mount_pid=$!

    for _ in {1..50}; do
        if mountpoint -q "$mount_dir"; then
            mounted=true
            return
        fi
        sleep 0.1
    done

    cat "$test_dir/mount.log" >&2
    return 1
}

stop_mount() {
    fusermount3 -u "$mount_dir"
    wait "$mount_pid"
    mount_pid=
    mounted=false
}

test -x "$fade_bin"
test -c /dev/fuse
mkdir "$backing_dir" "$mount_dir"
start_mount

mkdir "$mount_dir/1s"
printf 'ephemeral\n' >"$mount_dir/1s/ephemeral.txt"
test "$(cat "$mount_dir/1s/ephemeral.txt")" = ephemeral
exec 3<"$mount_dir/1s/ephemeral.txt"
sleep 2
if IFS= read -r _ <&3 2>/dev/null; then
    exit 1
fi
exec 3<&-
test ! -e "$mount_dir/1s/ephemeral.txt"
test -f "$backing_dir/1s/ephemeral.txt"
"$fade_bin" gc "$backing_dir" --json >/dev/null
test ! -e "$backing_dir/1s/ephemeral.txt"

printf 'persistent\n' >"$mount_dir/1h/persistent.txt"
printf 'offline expiry\n' >"$mount_dir/1s/offline.txt"
stop_mount
sleep 2
start_mount
test "$(cat "$mount_dir/1h/persistent.txt")" = persistent
test ! -e "$mount_dir/1s/offline.txt"
stop_mount

printf 'unknown\n' >"$backing_dir/1h/unknown.txt"
if start_mount >/dev/null 2>&1; then
    exit 1
fi
wait "$mount_pid" || true
mount_pid=
rm "$backing_dir/1h/unknown.txt"
start_mount
stop_mount

backing_dir="$test_dir/policy-backing"
mount_dir="$test_dir/policy-mount"
config="$test_dir/fade.toml"
mount_args=(--mode policy --config "$config")
mkdir "$backing_dir" "$mount_dir"
cat >"$config" <<'EOF'
[[rules]]
pattern = "*.token"
ttl = "1s"

[[rules]]
pattern = "*"
ttl = "forever"
EOF
"$fade_bin" check --config "$config" --path reports/session.token --json \
    | grep -q '"ttl": "1s"'
start_mount
mkdir "$mount_dir/7d"
printf 'kept\n' >"$mount_dir/7d/kept.txt"
stop_mount
start_mount
test "$(cat "$mount_dir/7d/kept.txt")" = kept
printf 'policy\n' >"$mount_dir/7d/session.token"
test "$(cat "$mount_dir/7d/session.token")" = policy
sleep 2
test ! -e "$mount_dir/7d/session.token"
