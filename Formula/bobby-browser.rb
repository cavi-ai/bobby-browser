# typed: false
# frozen_string_literal: true

# Homebrew formula for bobby-browser GitHub Release binaries.
#
# Ships from the tap (repo cavi-ai/homebrew-tap, which brew addresses as
# cavi-ai/tap):
#   brew tap cavi-ai/tap
#   brew install cavi-ai/tap/bobby-browser
#
# Homebrew rejects a formula outside a tap, so there is no
# `brew install --formula ./Formula/...` path.
#
# Every release: bump `version` and replace all four sha256 values with the
# digests of the published assets:
#   for a in macos-arm64 macos-x64 linux-arm64 linux-x64; do
#     curl -fsSL "https://github.com/cavi-ai/bobby-browser/releases/download/\
# v$VERSION/bobby-browser-$VERSION-$a.tar.gz" | shasum -a 256
#   done
class BobbyBrowser < Formula
  desc "Bobby Browser automation runtime (bobby + MCP/ACP gateways)"
  homepage "https://github.com/cavi-ai/bobby-browser"
  version "0.24.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-macos-arm64.tar.gz"
      sha256 "52fde28e970fe3cba9e015f07cccfa1f4910bce2c8cc356671f7a7e9873efd6b"
    end
    on_intel do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-macos-x64.tar.gz"
      sha256 "201a5c5428fa71d55a9bb931e61cba9509d86b8c237887b8ca28c461465785ef"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-linux-arm64.tar.gz"
      sha256 "eb17b1176d41d5e4cb5aabe5f29a4e29685f95d5b2b5e4735ebcaba63fe1aef8"
    end
    on_intel do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-linux-x64.tar.gz"
      sha256 "e7f6c7041332546522c353bd1821f51f1d3db7397113d757edf12e84caa3accf"
    end
  end

  def install
    bin.install "bobby"
    bin.install "mcp-gateway" if File.exist?("mcp-gateway")
    bin.install "acp-gateway" if File.exist?("acp-gateway")
    (share/"bobby-browser/scripts").install "scripts/vision-mlx"
    (share/"bobby-browser").install "firefox-companion" if File.exist?("firefox-companion")
  end

  test do
    assert_match "bobby", shell_output("#{bin}/bobby --help")
  end
end
