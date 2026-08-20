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
  version "0.11.0"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-macos-arm64.tar.gz"
      sha256 "12376759666ae5340e9843b7b46ec8978b392c590c9af6a7659641cb4fd21dc0"
    end
    on_intel do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-macos-x64.tar.gz"
      sha256 "dc65396055197b39201ef61bc2d87580915f97c0fce48082b4317f6e1de4b92b"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-linux-arm64.tar.gz"
      sha256 "97354b1ad25eaa869ba08971820cbcbb5f0477527f52040946309161f2fb7d0b"
    end
    on_intel do
      url "https://github.com/cavi-ai/bobby-browser/releases/download/v#{version}/bobby-browser-#{version}-linux-x64.tar.gz"
      sha256 "a15c6db9a41774239ba211d541a1a050b3bce93611fa702dd5b44bef86ba5343"
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
