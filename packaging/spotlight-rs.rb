# Homebrew cask for Spotlight-rs.
#
# Reference copy only — the cask Homebrew actually serves lives in the
# Nickhoyer/homebrew-tap repo, and .github/workflows/release.yml rewrites its
# `version` and `sha256` on every release. Edits here do not reach users; keep
# the two in sync by hand when the structure changes.
#
# Releases from v0.10.0 on are Developer ID signed and notarized, with the
# ticket stapled into the bundle, so Gatekeeper clears them on first launch
# with no right-click → Open and no quarantine workaround.
cask "spotlight-rs" do
  version "0.1.0"
  sha256 "1b3fbc3562163346e488716d25ad9c036083c1d295820071870e3be81ddfe7f1"

  url "https://github.com/Nickhoyer/spotlight-rs/releases/download/v#{version}/Spotlight-rs.zip"
  name "Spotlight-rs"
  desc "Background menu-bar launcher (GPUI)"
  homepage "https://github.com/Nickhoyer/spotlight-rs"

  depends_on macos: :ventura # 13.0+, for SMAppService (Launch at Login)

  app "Spotlight-rs.app"

  # Restart the app across an upgrade. `brew upgrade` uninstalls the old cask
  # before installing the new one, so `uninstall quit:` is what stops the
  # running instance before its bundle is replaced — without it the old process
  # survives, running from the deleted bundle, until it is manually restarted.
  # postflight then brings the new version straight back up.
  #
  # This also launches the app after a plain `brew install`, which is the useful
  # behaviour for a menu-bar agent with no Dock icon to click.
  postflight do
    system_command "/usr/bin/open",
                   args: ["-a", "#{appdir}/Spotlight-rs.app"],
                   sudo: false
  end

  uninstall quit: "com.nickolashoyer.spotlight-rs"

  zap trash: "~/Library/Application Support/spotlight-rs"
end
