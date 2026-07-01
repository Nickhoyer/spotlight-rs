#!/usr/bin/env bash
# Cut a release: pick the next version from Conventional Commits, bump it
# everywhere, build the bundle, tag, push, publish a GitHub release, and (if a
# tap is configured) update the Homebrew cask.
#
# Usage:
#   scripts/release.sh                 # auto: bump from commits since last tag
#   scripts/release.sh patch|minor|major   # force a bump level
#   scripts/release.sh 1.2.3           # set an explicit version
#   scripts/release.sh --yes           # skip the confirmation prompt
#
# Env:
#   SPOTLIGHT_TAP_DIR=/path/to/homebrew-tap   # local clone of your tap; if set,
#                                             # the cask is copied there + pushed
#
# Requires: a Rust toolchain, `gh` (authenticated), and the macOS built-ins used
# by scripts/bundle.sh. The FIRST release is expected to be done manually; this
# script takes over from the second onward (it needs a prior vX.Y.Z tag to diff).
set -euo pipefail

export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
cd "$(dirname "$0")/.."

APP_NAME="Spotlight-rs"
ZIP="dist/$APP_NAME.zip"
CASK="packaging/spotlight-rs.rb"
PLIST="packaging/Info.plist"

die() {
	echo "error: $*" >&2
	exit 1
}

FORCE_LEVEL=""
EXPLICIT_VERSION=""
ASSUME_YES=0
for arg in "$@"; do
	case "$arg" in
	major | minor | patch) FORCE_LEVEL="$arg" ;;
	--yes | -y) ASSUME_YES=1 ;;
	[0-9]*.[0-9]*.[0-9]*) EXPLICIT_VERSION="$arg" ;;
	*) die "unrecognized argument: $arg" ;;
	esac
done

# --- preconditions --------------------------------------------------------
command -v gh >/dev/null || die "'gh' not found (brew install gh, then 'gh auth login')"
gh auth status >/dev/null 2>&1 || die "'gh' is not authenticated; run 'gh auth login'"
git remote get-url origin >/dev/null 2>&1 || die "no 'origin' remote; push the repo to GitHub first"
[ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || die "not on main"
[ -z "$(git status --porcelain)" ] || die "working tree is dirty; commit or stash first"

# --- current version + bump decision --------------------------------------
last_tag="$(git describe --tags --abbrev=0 2>/dev/null || true)"
if [ -n "$last_tag" ]; then
	base="${last_tag#v}"
	range="${last_tag}..HEAD"
else
	base="$(sed -nE 's/^version = "([0-9]+\.[0-9]+\.[0-9]+)"/\1/p' Cargo.toml | head -n1)"
	range="HEAD"
	echo "note: no prior tag; treating Cargo.toml version ($base) as the base."
fi

decide_bump() {
	local subjects
	subjects="$(git log --format='%s' "$range" 2>/dev/null || true)"
	if git log --format='%B' "$range" 2>/dev/null | grep -qE '^BREAKING CHANGE' ||
		printf '%s\n' "$subjects" | grep -qE '^[a-z]+(\(.+\))?!:'; then
		echo major
	elif printf '%s\n' "$subjects" | grep -qE '^feat(\(.+\))?:'; then
		echo minor
	elif printf '%s\n' "$subjects" | grep -qE '^fix(\(.+\))?:'; then
		echo patch
	else
		echo patch # no conventional signal; safest default
	fi
}

if [ -n "$EXPLICIT_VERSION" ]; then
	new="$EXPLICIT_VERSION"
else
	level="${FORCE_LEVEL:-$(decide_bump)}"
	IFS=. read -r MA MI PA <<<"$base"
	case "$level" in
	major)
		MA=$((MA + 1))
		MI=0
		PA=0
		;;
	minor)
		MI=$((MI + 1))
		PA=0
		;;
	patch) PA=$((PA + 1)) ;;
	esac
	new="$MA.$MI.$PA"
fi

tag="v$new"
git rev-parse "$tag" >/dev/null 2>&1 && die "tag $tag already exists"

echo "==> Releasing $tag  (from ${last_tag:-<none>}, base $base)"
git log --format='  %s' "$range" 2>/dev/null | sed '/^  chore(release):/d' || true
if [ "$ASSUME_YES" -ne 1 ]; then
	printf "Proceed? [y/N] "
	read -r reply
	[ "$reply" = "y" ] || [ "$reply" = "Y" ] || die "aborted"
fi

# --- bump version in the tracked sources ----------------------------------
sed -i '' -E "s/^version = \"[0-9]+\.[0-9]+\.[0-9]+\"/version = \"$new\"/" Cargo.toml
plutil -replace CFBundleShortVersionString -string "$new" "$PLIST"
plutil -replace CFBundleVersion -string "$new" "$PLIST"
sed -i '' -E "s/^  version \"[0-9]+\.[0-9]+\.[0-9]+\"/  version \"$new\"/" "$CASK"

# --- build the bundle (embeds the new version) + checksum -----------------
echo "==> Building bundle"
scripts/bundle.sh >/dev/null
sha="$(shasum -a 256 "$ZIP" | awk '{print $1}')"
sed -i '' -E "s/^  sha256 \".*\"/  sha256 \"$sha\"/" "$CASK"
echo "    $ZIP  sha256=$sha"

# --- commit, tag, push, release -------------------------------------------
git add Cargo.toml Cargo.lock "$PLIST" "$CASK"
git commit -m "chore(release): $tag"
git tag -a "$tag" -m "$tag"
git push origin main --follow-tags

echo "==> Creating GitHub release"
gh release create "$tag" "$ZIP" --title "$tag" --generate-notes

# --- optional: publish the cask to your tap -------------------------------
if [ -n "${SPOTLIGHT_TAP_DIR:-}" ]; then
	echo "==> Publishing cask to $SPOTLIGHT_TAP_DIR"
	mkdir -p "$SPOTLIGHT_TAP_DIR/Casks"
	cp "$CASK" "$SPOTLIGHT_TAP_DIR/Casks/spotlight-rs.rb"
	git -C "$SPOTLIGHT_TAP_DIR" add Casks/spotlight-rs.rb
	git -C "$SPOTLIGHT_TAP_DIR" commit -m "spotlight-rs $new"
	git -C "$SPOTLIGHT_TAP_DIR" push
else
	echo "note: SPOTLIGHT_TAP_DIR unset — copy $CASK into your tap's Casks/ to publish."
fi

echo "==> Released $tag"
