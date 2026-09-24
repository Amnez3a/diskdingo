#!/bin/sh
# Build diskdingo in release mode. Binary lands in target/release/diskdingo.
set -eu
cd "$(dirname "$0")"
cargo build --release
echo "built $(pwd)/target/release/diskdingo"
