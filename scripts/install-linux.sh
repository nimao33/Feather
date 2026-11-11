#!/bin/sh
# Installs Feather for the current user, on any Linux distribution: the
# program in ~/.local/bin, an applications-menu entry, and (with
# --autostart) a start at sign-in.
#
#   sh scripts/install-linux.sh [--autostart] [--static]
#
# In the source tree it builds Feather first, which needs only Rust's cargo
# and a C compiler (gcc or clang); --static builds with musl, for one binary
# that runs anywhere. In a release download (a `feather` binary next to this
# script, as ./install.sh) it installs that binary and builds nothing.
set -eu

autostart=""
target=""
for argument in "$@"; do
    case "$argument" in
        --autostart) autostart="--autostart" ;;
        --static) target="x86_64-unknown-linux-musl" ;;
        *) echo "unknown option: $argument" >&2; exit 2 ;;
    esac
done

here="$(cd "$(dirname "$0")" && pwd)"
if [ -f "$here/feather" ] && [ ! -f "$here/../Cargo.toml" ]; then
    # A release download: the binary is already built.
    binary="$here/feather"
else
    if ! command -v cargo >/dev/null 2>&1; then
        echo "Feather needs Rust. Install it from https://rustup.rs (or your package manager), then run this again." >&2
        exit 1
    fi
    if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1 && ! command -v clang >/dev/null 2>&1; then
        echo "Feather needs a C compiler for its TLS library. Install gcc or clang (for example build-essential, base-devel, or @development-tools)." >&2
        exit 1
    fi
    if [ -n "$target" ] && ! command -v musl-gcc >/dev/null 2>&1; then
        echo "--static needs musl-gcc. Install musl-tools (Debian, Ubuntu), musl (Arch), or musl-gcc (Fedora)." >&2
        exit 1
    fi
    cd "$here/.."
    if [ -n "$target" ]; then
        rustup target add "$target" >/dev/null 2>&1 || true
        cargo build --release -p feather-app --target "$target"
        binary="target/$target/release/feather"
    else
        cargo build --release -p feather-app
        binary="target/release/feather"
    fi
fi

bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
mkdir -p "$bin"
# An older copy that is running keeps its files open; stop it first.
if [ -x "$bin/feather" ]; then
    "$bin/feather" quit >/dev/null 2>&1 || true
fi
install -m 755 "$binary" "$bin/feather"
"$bin/feather" install $autostart

case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "Add $bin to your PATH to run \`feather\` from a terminal." ;;
esac
echo "Done. Open Feather from your applications menu, or run: feather"
