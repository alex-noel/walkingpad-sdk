#!/bin/sh
set -eu

sdk_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$sdk_root"
if [ "$(uname -s)" != Darwin ]; then
  printf '%s\n' 'This packaging script requires macOS.' >&2
  exit 1
fi
cargo_bin=$(command -v cargo || true)
if [ -z "$cargo_bin" ]; then cargo_bin="$HOME/.cargo/bin/cargo"; fi
"$cargo_bin" build --locked --release --bin walkingpad
bundle="$sdk_root/dist/WalkingPad SDK.app"
mkdir -p "$bundle/Contents/MacOS"
cp target/release/walkingpad "$bundle/Contents/MacOS/walkingpad"
cp crates/walkingpad-cli/Info.plist "$bundle/Contents/Info.plist"
codesign --force --sign - "$bundle"
printf '%s\n' "$bundle/Contents/MacOS/walkingpad"

