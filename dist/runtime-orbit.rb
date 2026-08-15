# Homebrew formula for runtime-orbit.
#
# This is a reference template. The release workflow (.github/workflows/release.yml)
# regenerates it in the slothlabsorg/homebrew-tap repo on each tagged release with
# the real version + sha256 values. Users install with:
#
#     brew install slothlabsorg/tap/runtime-orbit
#
class RuntimeOrbit < Formula
  desc "Borrow a beefier machine's container runtime over your LAN, transparently"
  homepage "https://slothlabs.org/runtime-orbit"
  version "0.2.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/slothlabsorg/runtime-orbit/releases/download/v0.2.0/runtime-orbit-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACED_BY_CI"
    end
    on_intel do
      url "https://github.com/slothlabsorg/runtime-orbit/releases/download/v0.2.0/runtime-orbit-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACED_BY_CI"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/slothlabsorg/runtime-orbit/releases/download/v0.2.0/runtime-orbit-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACED_BY_CI"
    end
    on_intel do
      url "https://github.com/slothlabsorg/runtime-orbit/releases/download/v0.2.0/runtime-orbit-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACED_BY_CI"
    end
  end

  def install
    bin.install "runtime-orbit"
    # Short aliases, and the pre-0.2 name so muscle memory keeps working.
    bin.install_symlink bin/"runtime-orbit" => "r-orbit"
    bin.install_symlink bin/"runtime-orbit" => "orbit"
  end

  test do
    assert_match "runtime-orbit", shell_output("#{bin}/runtime-orbit --version")
    assert_match "runtime-orbit", shell_output("#{bin}/r-orbit --version")
  end
end
