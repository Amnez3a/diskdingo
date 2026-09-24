#!/bin/sh
# Cross-compile diskdingo for every supported platform. Binaries land in
# dist/ as diskdingo-<os>-<arch>[.exe]. Run from Linux.
#
# Requirements (a target is skipped, with a message, when its tool is missing):
#   Linux (all four, static musl)  ld.lld       pacman -S lld / apt install lld
#   Windows x86_64                 x86_64-w64-mingw32-gcc   pacman -S mingw-w64-gcc / apt install gcc-mingw-w64-x86-64
#   macOS arm64 + x86_64           zig + cargo-zigbuild     https://ziglang.org/download, cargo install cargo-zigbuild
# Missing rustup targets are added automatically.
#
# Pass target names to build a subset, e.g. ./cross.sh aarch64-unknown-linux-musl
set -eu
cd "$(dirname "$0")"
mkdir -p dist

# target                          output name                 linker tool          cargo subcommand
MATRIX="
x86_64-unknown-linux-musl         diskdingo-linux-x86_64      ld.lld               build
aarch64-unknown-linux-musl        diskdingo-linux-aarch64     ld.lld               build
armv7-unknown-linux-musleabihf    diskdingo-linux-armv7       ld.lld               build
arm-unknown-linux-musleabihf      diskdingo-linux-armv6       ld.lld               build
x86_64-pc-windows-gnu             diskdingo-windows-x86_64.exe x86_64-w64-mingw32-gcc build
aarch64-apple-darwin              diskdingo-macos-arm64       zig                  zigbuild
x86_64-apple-darwin               diskdingo-macos-x86_64      zig                  zigbuild
"

wanted="$*"
failed=""
echo "$MATRIX" | while read -r target name tool sub; do
    [ -n "$target" ] || continue
    if [ -n "$wanted" ]; then
        case " $wanted " in *" $target "*) ;; *) continue ;; esac
    fi
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "skip  $target: $tool not found" >&2
        continue
    fi
    if [ "$sub" = zigbuild ] && ! cargo zigbuild --help >/dev/null 2>&1; then
        echo "skip  $target: cargo-zigbuild not installed (cargo install cargo-zigbuild)" >&2
        continue
    fi
    rustup target list --installed | grep -qx "$target" || rustup target add "$target"
    # rustc warns that no Apple SDK is present for the macOS targets; zig
    # supplies the system library stubs, so that warning is harmless.
    cargo "$sub" --release --target "$target" 2>&1 | grep -Ev 'xcrun|SDK is needed|DEVELOPER_DIR|^ *\| *$|^ *= (note|help)' || true
    bin="target/$target/release/diskdingo"
    case "$name" in *.exe) bin="$bin.exe" ;; esac
    if [ -f "$bin" ]; then
        cp "$bin" "dist/$name"
        echo "built dist/$name"
    else
        echo "FAILED $target" >&2
        exit 1
    fi
done
ls -l dist
