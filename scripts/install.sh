#!/usr/bin/env bash
# Install bobby (+ mcp-gateway, acp-gateway) from the latest (or $BOBBY_VERSION)
# GitHub Release archive.
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/cavi-ai/bobby-browser/main/scripts/install.sh | bash
# Optional env:
#   BOBBY_VERSION=0.6.0   # without leading v; default = latest release tag
#   INSTALL_DIR=~/.local/bin
set -euo pipefail

REPO="${BOBBY_REPO:-cavi-ai/bobby-browser}"
INSTALL_DIR="${INSTALL_DIR:-${HOME}/.local/bin}"

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "install.sh: need \`$1\` on PATH" >&2
    exit 1
  }
}

need curl
need tar
need uname

os="$(uname -s | tr '[:upper:]' '[:lower:]')"
arch="$(uname -m)"
case "$os" in
  linux) asset_os=linux ;;
  darwin) asset_os=macos ;;
  *)
    echo "install.sh: unsupported OS: $os (use linux or macos; Windows: download the .zip from Releases)" >&2
    exit 1
    ;;
esac
case "$arch" in
  x86_64 | amd64) asset_arch=x64 ;;
  arm64 | aarch64) asset_arch=arm64 ;;
  *)
    echo "install.sh: unsupported arch: $arch" >&2
    exit 1
    ;;
esac

if [[ -n "${BOBBY_VERSION:-}" ]]; then
  VERSION="${BOBBY_VERSION#v}"
  TAG="v${VERSION}"
else
  need python3
  TAG="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["tag_name"])')"
  VERSION="${TAG#v}"
fi

ASSET="bobby-browser-${VERSION}-${asset_os}-${asset_arch}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${TAG}/${ASSET}"
STAGE="bobby-browser-${VERSION}-${asset_os}-${asset_arch}"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

echo "install.sh: fetching ${URL}"
curl -fsSL -o "${tmpdir}/${ASSET}" "$URL"
tar -xzf "${tmpdir}/${ASSET}" -C "$tmpdir"

src_dir="${tmpdir}/${STAGE}"
if [[ ! -f "${src_dir}/bobby" ]]; then
  echo "install.sh: archive missing ${STAGE}/bobby" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
install -m 755 "${src_dir}/bobby" "${INSTALL_DIR}/bobby"
echo "install.sh: installed ${INSTALL_DIR}/bobby"

for bin in mcp-gateway acp-gateway; do
  if [[ -f "${src_dir}/${bin}" ]]; then
    install -m 755 "${src_dir}/${bin}" "${INSTALL_DIR}/${bin}"
    echo "install.sh: installed ${INSTALL_DIR}/${bin}"
  else
    echo "install.sh: warn: archive missing ${bin} (older release?); MCP/ACP hosts need it beside bobby" >&2
  fi
done

vision_share="$(dirname "$INSTALL_DIR")/share/bobby-browser/scripts"
if [[ -d "${src_dir}/scripts/vision-mlx" ]]; then
  mkdir -p "$vision_share"
  cp -R "${src_dir}/scripts/vision-mlx" "$vision_share/vision-mlx"
  echo "install.sh: installed ${vision_share}/vision-mlx"
fi

companion_share="$(dirname "$INSTALL_DIR")/share/bobby-browser/firefox-companion"
if [[ -d "${src_dir}/firefox-companion" ]]; then
  mkdir -p "$companion_share"
  cp -R "${src_dir}/firefox-companion/." "$companion_share/"
  echo "install.sh: installed ${companion_share}"
fi

if ! command -v bobby >/dev/null 2>&1; then
  echo "install.sh: add ${INSTALL_DIR} to PATH, then run: bobby doctor" >&2
else
  echo "install.sh: next: bobby doctor"
fi
