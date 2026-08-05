#!/usr/bin/env bash
# Build Spotlight-rs.app — a background menu-bar app bundle — and zip it for a
# GitHub release (which the Homebrew cask downloads).
#
# Usage:   scripts/bundle.sh
# Output:  dist/Spotlight-rs.app  and  dist/Spotlight-rs.zip
#
# Requirements: a Rust toolchain plus the macOS built-ins `iconutil`, `codesign`,
# and `ditto` (all ship with the Xcode command-line tools / macOS).
#
# Signing (see the SIGNING section below for the full story):
#   SIGN_IDENTITY   "Developer ID Application: Name (TEAMID)". Unset → ad-hoc.
#   NOTARY_PROFILE  a `notarytool store-credentials` profile name (local use).
#   NOTARY_KEY / NOTARY_KEY_ID / NOTARY_ISSUER
#                   App Store Connect API key path + ids (CI use).
# Notarization is skipped unless a real identity AND one of the two credential
# forms are present, so an unconfigured checkout still builds.
set -euo pipefail

export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
cd "$(dirname "$0")/.."

APP_NAME="Spotlight-rs"
BIN="spotlight"
DIST="dist"
APP="$DIST/$APP_NAME.app"
CONTENTS="$APP/Contents"
ENTITLEMENTS="packaging/spotlight-rs.entitlements"

# ---------------------------------------------------------------- SIGNING ----
# macOS records the *designated requirement* of an app when the user grants it a
# TCC permission (for us: Accessibility, which the synthetic ⌘V paste needs).
# An ad-hoc signature has no certificate to anchor that requirement to, so it
# falls back to the cdhash — the hash of the binary itself. Every rebuild
# changes the cdhash, macOS decides this is a different app, and the permission
# is silently dropped. That is why permissions reset on every update.
#
# A Developer ID signature anchors the requirement to the team certificate
# instead, which is stable across rebuilds, so the grant survives updates for as
# long as the bundle ID and Team ID stay put.
SIGN_IDENTITY="${SIGN_IDENTITY:--}"

sign_args=(--force --sign "$SIGN_IDENTITY")
if [ "$SIGN_IDENTITY" != "-" ]; then
  # --options runtime (hardened runtime) and --timestamp (trusted timestamp) are
  # both mandatory for notarization; neither is meaningful for an ad-hoc build.
  sign_args+=(--options runtime --timestamp --entitlements "$ENTITLEMENTS")
fi

# Pick whichever notarytool credential form is configured, if either.
notary_args=()
if [ -n "${NOTARY_PROFILE:-}" ]; then
  notary_args=(--keychain-profile "$NOTARY_PROFILE")
elif [ -n "${NOTARY_KEY:-}" ] && [ -n "${NOTARY_KEY_ID:-}" ] && [ -n "${NOTARY_ISSUER:-}" ]; then
  notary_args=(--key "$NOTARY_KEY" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER")
fi

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

if [ "$SIGN_IDENTITY" = "-" ]; then
  echo "==> Ad-hoc code signing (set SIGN_IDENTITY for a real signature)"
else
  echo "==> Code signing as $SIGN_IDENTITY"
fi
# Sign inside-out: nested code first, then the bundle that seals it. (`--deep`
# is deprecated and applies the outer options to inner code, which is wrong.)
codesign "${sign_args[@]}" "$CONTENTS/MacOS/$BIN"
codesign "${sign_args[@]}" "$APP"
codesign --verify --strict --verbose=2 "$APP"

echo "==> Zipping"
# `ditto` preserves the bundle + resource forks the way macOS expects.
ditto -c -k --sequesterRsrc --keepParent "$APP" "$DIST/$APP_NAME.zip"

if [ "$SIGN_IDENTITY" = "-" ]; then
  echo "==> Skipping notarization (ad-hoc build)"
elif [ ${#notary_args[@]} -eq 0 ]; then
  echo "==> Skipping notarization (no NOTARY_PROFILE or NOTARY_KEY/_ID/_ISSUER)"
else
  echo "==> Notarizing (this waits on Apple; usually a few minutes)"
  xcrun notarytool submit "$DIST/$APP_NAME.zip" "${notary_args[@]}" --wait

  # Stapling writes the ticket *into the .app*, so the shipped zip has to be
  # rebuilt afterwards — the one we just submitted does not contain it.
  echo "==> Stapling and re-zipping"
  xcrun stapler staple "$APP"
  rm -f "$DIST/$APP_NAME.zip"
  ditto -c -k --sequesterRsrc --keepParent "$APP" "$DIST/$APP_NAME.zip"

  # What Gatekeeper will actually say on a user's machine.
  spctl --assess --type execute --verbose=2 "$APP"
fi

echo "==> Done:"
echo "    $APP"
echo "    $DIST/$APP_NAME.zip  (shasum below for the cask)"
shasum -a 256 "$DIST/$APP_NAME.zip"
