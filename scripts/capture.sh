#!/usr/bin/env bash
# Capture the launcher's rendered window to a PNG, from inside the app process.
# Works headlessly / over SSH (unlike `screencapture` or external tools) because
# the capture runs in the GUI-session process that owns the window.
#
# Usage:   scripts/capture.sh [query] [out_png]
# Example: scripts/capture.sh "saf" /tmp/shot.png
# Env:     DELAY_MS (default 1200) — render settle time before the grab.
set -euo pipefail

export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
cd "$(dirname "$0")/.."

QUERY="${1:-}"
OUT="${2:-/tmp/spotlight-capture.png}"

cargo build -q -p spotlight
pkill -f "target/debug/spotlight" 2>/dev/null || true

SPOTLIGHT_CAPTURE="$OUT" \
SPOTLIGHT_CAPTURE_QUERY="$QUERY" \
SPOTLIGHT_CAPTURE_DELAY_MS="${DELAY_MS:-1200}" \
  ./target/debug/spotlight

echo "wrote $OUT"
