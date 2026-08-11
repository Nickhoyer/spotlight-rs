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

  # Launch on a *first install only*. This is a menu-bar agent with no Dock
  # icon, so there is otherwise nothing for the user to click after installing.
  #
  # Upgrades are deliberately left alone. `uninstall quit:` stops the running
  # instance before its bundle is replaced, and Homebrew reopens every bundle id
  # it quit once the upgrade completes — so the restart already happens without
  # help from here.
  #
  # Opening the app from postflight *breaks* that upgrade path. postflight runs
  # inside install_artifacts, before Homebrew copies the old bundle's
  # com.apple.quarantine user-approved flag onto the new one; macOS rewrites the
  # same attribute during the app's first launch. The two writes race, which is
  # what produces
  #
  #   Warning: Homebrew couldn't inherit spotlight-rs's quarantine approval so
  #   macOS will prompt at next launch.
  #
  # and hands the user the Gatekeeper prompt the inheritance exists to suppress.
  #
  # An upgrade renames the predecessor's staged directory to
  # `<caskroom>/<old version>.upgrading` and only purges it once the new cask is
  # fully installed, so that directory existing at postflight time is what
  # distinguishes an upgrade from a first install.
  postflight do
    if caskroom_path.children.none? { |path| path.basename.to_s.end_with?(".upgrading") }
      system_command "/usr/bin/open",
                     args: ["-a", "#{appdir}/Spotlight-rs.app"],
                     sudo: false
    end
  end

  uninstall quit: "com.nickolashoyer.spotlight-rs"

  zap trash: "~/Library/Application Support/spotlight-rs"
end
