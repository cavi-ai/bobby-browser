# typed: false
# frozen_string_literal: true

# Homebrew formula for bobby-browser GitHub Release binaries.
#
# BLOCKER before publishing the tap: every `sha256` below is `:no_check`, so
# brew downloads and installs the tarball without verifying it. That is
# acceptable for `--formula ./Formula/...` off a trusted checkout and is not
# acceptable for a tap other people install from. Fill the four digests from
# the release assets first:
#   for a in macos-arm64 macos-x64 linux-arm64 linux-x64; do
#     curl -fsSL "https://github.com/cavi-ai/bobby-browser/releases/download/\
# v$VERSION/bobby-browser-$VERSION-$a.tar.gz" | shasum -a 256
#   done
# Install from a checkout:
#   brew install --formula ./Formula/bobby-browser.rb
#
# Or from the tap (repo cavi-ai/homebrew-tap, which brew addresses as
# cavi-ai/tap):
#   brew tap cavi-ai/tap
#   brew install cavi-ai/tap/bobby-browser
class BobbyBrowser < Formula
  desc "Bobby Browser automation runtime (bobby + MCP/ACP gateways)"
  homepage "https://github.com/cavi-ai/bobby-browser"
  version "0.7.0"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-macos-arm64.tar.gz"
      # sha256: update per release (brew fetch --force will print the expected digest)
      sha256 :no_check
    end
    on_intel do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-macos-x64.tar.gz"
      sha256 :no_check
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-linux-arm64.tar.gz"
      sha256 :no_check
    end
    on_intel do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-linux-x64.tar.gz"
      sha256 :no_check
    end
  end

  def install
    bin.install "bobby"
    bin.install "mcp-gateway" if File.exist?("mcp-gateway")
    bin.install "acp-gateway" if File.exist?("acp-gateway")
  end

  test do
    assert_match "bobby", shell_output("#{bin}/bobby --help")
  end
end
