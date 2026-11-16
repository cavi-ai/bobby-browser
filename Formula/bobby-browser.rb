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
  version "0.25.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-macos-arm64.tar.gz"
      sha256 "e75eae8d7f2b5aed5302aa647ee384f65983b17c870b8f4d0f9de024c952cd6c"
    end
    on_intel do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-macos-x64.tar.gz"
      sha256 "02dee33859b63e1b4597a8b367dd353b898e9e0685bc68373032683ad535c9d5"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-linux-arm64.tar.gz"
      sha256 "b9dec2d1cd896e35b82d7c1998a6e9eb313aae30fbf2a10d660e50ca8bd466c7"
    end
    on_intel do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-linux-x64.tar.gz"
      sha256 "2ec30b351e23d7b0f12d6de3bb3525b82c4b35dafcd642e3c1e1c500e082115a"
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
