#!/usr/bin/env bash
# Build Spotlight-rs.app — a background menu-bar app bundle — and zip it for a
# GitHub release (which the Homebrew cask downloads).
#
# Usage:   scripts/bundle.sh
# Output:  dist/Spotlight-rs.app  and  dist/Spotlight-rs.zip
#
# Requirements: a Rust toolchain plus the macOS built-ins `iconutil`, `codesign`,
# and `ditto` (all ship with the Xcode command-line tools / macOS).
set -euo pipefail

export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
cd "$(dirname "$0")/.."

APP_NAME="Spotlight-rs"
BIN="spotlight"
DIST="dist"
APP="$DIST/$APP_NAME.app"
CONTENTS="$APP/Contents"

echo "==> Building release binary"
cargo build --release -q -p "$BIN"

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"
cp "target/release/$BIN" "$CONTENTS/MacOS/$BIN"
cp packaging/Info.plist "$CONTENTS/Info.plist"

echo "==> Rendering app icon"
ICONSET="$(mktemp -d)/AppIcon.iconset"
mkdir -p "$ICONSET"
"target/release/$BIN" --emit-iconset "$ICONSET"
iconutil -c icns "$ICONSET" -o "$CONTENTS/Resources/AppIcon.icns"
rm -rf "$(dirname "$ICONSET")"

echo "==> Ad-hoc code signing"
# Ad-hoc signature (-s -): enough for local/personal install. Real distribution
# should use a Developer ID + notarization; see packaging/spotlight-rs.rb notes.
codesign --force --deep --sign - "$APP"

echo "==> Zipping"
# `ditto` preserves the bundle + resource forks the way macOS expects.
ditto -c -k --sequesterRsrc --keepParent "$APP" "$DIST/$APP_NAME.zip"

echo "==> Done:"
echo "    $APP"
echo "    $DIST/$APP_NAME.zip  (shasum below for the cask)"
shasum -a 256 "$DIST/$APP_NAME.zip"
