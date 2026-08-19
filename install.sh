#!/usr/bin/env bash
set -euo pipefail

prefix=${FADE_PREFIX:-"$HOME/.local"}
bin_dir="$prefix/bin"
binary="$bin_dir/fade"

fail() {
    printf '%s\n' "error: $*" >&2
    exit 1
}

if [ "${1:-}" = "--uninstall" ]; then
    if [ -e "$binary" ]; then
        rm "$binary"
        printf 'Removed %s\n' "$binary"
    else
        printf 'Fade is not installed at %s\n' "$binary"
    fi
    exit 0
fi

[ "${1:-}" = "" ] || fail "usage: ./install.sh [--uninstall]"
[ "$(uname -s)" = "Linux" ] || fail "Fade requires Linux"
command -v cargo >/dev/null || fail "install Rust and Cargo first"
command -v fusermount3 >/dev/null || command -v fusermount >/dev/null \
    || fail "install FUSE 3 or FUSE before you mount Fade"

cargo build --release --locked
install -Dm755 target/release/fade "$binary"
printf 'Installed %s\n' "$binary"

case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) printf 'Add %s to PATH before you run fade.\n' "$bin_dir" ;;
esac
