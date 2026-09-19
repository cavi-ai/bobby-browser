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
  version "0.15.0"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-macos-arm64.tar.gz"
      sha256 "b309e522aed46ad751f3746a12a11c61bbef3d33ea138b00f396d4eda12324e4"
    end
    on_intel do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-macos-x64.tar.gz"
      sha256 "408507750b6aab3efaa3fed9f3704d6ccd91fadad656c641f64824a64ca5176a"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-linux-arm64.tar.gz"
      sha256 "ddf43d59ff67c84a7df46f21dca43565716052f57a4f0d8d82ff0fe3b5a4bc90"
    end
    on_intel do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-linux-x64.tar.gz"
      sha256 "e5a7d46080e85a4e31f1426a18c732223f7e6d60d51da66e33a56db49189eb47"
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
