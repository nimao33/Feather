#!/bin/sh
# Builds Feather and installs it for the current user, on any Linux
# distribution: the program in ~/.local/bin, an applications-menu entry,
# and (with --autostart) a start at sign-in. Needs only Rust's cargo and a
# C compiler (gcc or clang), which every distribution packages.
#
#   sh scripts/install-linux.sh [--autostart] [--static]
#
# --static builds with musl, for one binary that runs anywhere.
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

if ! command -v cargo >/dev/null 2>&1; then
    echo "Feather needs Rust. Install it from https://rustup.rs (or your package manager), then run this again." >&2
    exit 1
fi
if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1 && ! command -v clang >/dev/null 2>&1; then
    echo "Feather needs a C compiler for its TLS library. Install gcc or clang (for example build-essential, base-devel, or @development-tools)." >&2
    exit 1
fi

cd "$(dirname "$0")/.."
if [ -n "$target" ]; then
    rustup target add "$target" >/dev/null 2>&1 || true
    cargo build --release -p feather-app --target "$target"
    binary="target/$target/release/feather"
else
    cargo build --release -p feather-app
    binary="target/release/feather"
fi

bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
mkdir -p "$bin"
install -m 755 "$binary" "$bin/feather"
"$bin/feather" install $autostart

case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "Add $bin to your PATH to run \`feather\` from a terminal." ;;
esac
echo "Done. Open Feather from your applications menu, or run: feather"
