#!/bin/sh
# Packs a built Linux binary into the release download:
#
#   sh scripts/package-linux.sh <feather binary> <version> [out dir]
#
# makes <out dir>/feather-<version>-linux-x86_64.tar.gz with one folder of
# the same name holding the binary, install.sh, README.txt and LICENSE.
# Used by .github/workflows/release.yml.
set -eu

if [ $# -lt 2 ]; then
    echo "usage: $0 <feather binary> <version> [out dir]" >&2
    exit 2
fi
binary="$1"
version="$2"
out="${3:-dist}"
scripts="$(cd "$(dirname "$0")" && pwd)"
license="$scripts/../LICENSE"

name="feather-$version-linux-x86_64"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
folder="$stage/$name"
mkdir -p "$folder" "$out"

cp "$binary" "$folder/feather"
chmod 755 "$folder/feather"
cp "$scripts/install-linux.sh" "$folder/install.sh"
chmod 755 "$folder/install.sh"
if [ -f "$license" ]; then
    cp "$license" "$folder/LICENSE"
fi

cat > "$folder/README.txt" <<EOF
Feather $version for Linux (x86_64)
===================================

A fast file search. On Linux its window is a local page in an app window of
your browser (Chromium, Chrome, Brave, Edge or Vivaldi when one is
installed, otherwise your default browser). It listens on 127.0.0.1 only,
behind a random key. The binary is static: it runs on any distribution.

Install for your user (no root needed):

    ./install.sh               # copies feather to ~/.local/bin, adds a menu entry
    ./install.sh --autostart   # the same, and starts Feather at sign-in

or by hand:

    install -Dm755 feather ~/.local/bin/feather
    ~/.local/bin/feather install [--autostart]

\`feather install\` points the menu entry at the copy of feather you run it
from, so run it from where the program will stay. Then open Feather from
your applications menu, or run \`feather\`.

Pick a browser by hand with FEATHER_BROWSER, for example
FEATHER_BROWSER="firefox --new-window" or FEATHER_BROWSER="chromium --app=%s".

Other commands:

    feather search report pdf   search from the terminal
    feather quit                stop the running Feather
    feather uninstall           remove the menu and autostart entries
    feather --help              everything else

Uninstall: run \`feather uninstall\`, then delete ~/.local/bin/feather.
Settings are in ~/.config/feather and the index in ~/.local/share/feather.

https://github.com/nimao33/Feather
EOF

tarball="$name.tar.gz"
# Same owner and order on every build.
tar -C "$stage" --owner=0 --group=0 --numeric-owner --sort=name \
    -czf "$out/$tarball" "$name"
echo "$out/$tarball"
