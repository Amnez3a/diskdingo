#!/bin/sh
# Install the already-built diskdingo binary to /usr/bin (or $1 if given).
# Run ./build.sh first.
set -eu
cd "$(dirname "$0")"
DEST="${1:-/usr/bin}"
BIN=target/release/diskdingo
if [ ! -x "$BIN" ]; then
    echo "error: $BIN not found; run ./build.sh first" >&2
    exit 1
fi
if [ -w "$DEST" ]; then SUDO=""; else SUDO="sudo"; fi
$SUDO install -m 0755 "$BIN" "$DEST/diskdingo"
echo "installed $DEST/diskdingo"
