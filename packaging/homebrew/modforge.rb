# After a release: replace the version and the SHA-256 (from the .sha256 file next to the release's macOS archive),
# then put this file in a tap (homebrew-modforge/Casks/modforge.rb) or submit it to homebrew-cask.
cask "modforge" do
  version "5.4.0"
  sha256 "329a00105a070e12d3ab0f05039c7deadc1c7df57f4794b92919998c9ff1fbd7"

  url "https://github.com/pigeoonn/modforge/releases/download/v#{version}/modforge-v#{version}-macos-arm64.tar.gz"
  name "ModForge"
  desc "Mod, trainer and crash-hunting hub for games"
  homepage "https://github.com/pigeoonn/modforge"

  depends_on arch: :arm64

  binary "modforge-v#{version}-macos-arm64/modforge"
end
