# Homebrew cask for Spotlight-rs.
#
# Release checklist:
#   1. scripts/bundle.sh                      # builds dist/Spotlight-rs.zip
#   2. Upload dist/Spotlight-rs.zip to a GitHub release tagged vX.Y.Z
#   3. Fill in `version` and `sha256` (the shasum printed by bundle.sh) below
#   4. Publish via your own tap:  brew install --cask <user>/tap/spotlight-rs
#
# NOTE: releases built without the Developer ID secrets are ad-hoc signed and not
# notarized. On first launch macOS Gatekeeper will block those; users right-click
# → Open once, or run:
#   xattr -dr com.apple.quarantine "/Applications/Spotlight-rs.app"
# The postflight below asks Homebrew to strip the attribute on install.
#
# Once the first notarized release ships (see scripts/bundle.sh), drop that
# postflight — a stapled build passes Gatekeeper on its own, and stripping
# quarantine on a notarized app just skips a check that would have succeeded.
cask "spotlight-rs" do
  version "0.1.0"
  sha256 "1b3fbc3562163346e488716d25ad9c036083c1d295820071870e3be81ddfe7f1"

  url "https://github.com/Nickhoyer/spotlight-rs/releases/download/v#{version}/Spotlight-rs.zip"
  name "Spotlight-rs"
  desc "Background menu-bar launcher (GPUI)"
  homepage "https://github.com/Nickhoyer/spotlight-rs"

  depends_on macos: :ventura # 13.0+, for SMAppService (Launch at Login)

  app "Spotlight-rs.app"

  # Unsigned/un-notarized: clear quarantine so it launches without a right-click.
  postflight do
    system_command "/usr/bin/xattr",
                   args: ["-dr", "com.apple.quarantine", "#{appdir}/Spotlight-rs.app"],
                   sudo: false
  end

  zap trash: "~/Library/Application Support/spotlight-rs"
end
