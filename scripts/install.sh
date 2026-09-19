#!/usr/bin/env bash
# Install bobby (+ mcp-gateway, acp-gateway) from the latest (or $BOBBY_VERSION)
# GitHub Release archive.
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/cavi-ai/bobby-browser/main/scripts/install.sh | bash
# Optional env:
#   BOBBY_VERSION=0.6.0   # without leading v; default = latest release tag
#   INSTALL_DIR=~/.local/bin
#   BOBBY_SHARE_DIR=~/.local/share/bobby-browser
#   BOBBY_ARCHIVE=/path/to/release.tar.gz
set -euo pipefail

REPO="${BOBBY_REPO:-cavi-ai/bobby-browser}"
INSTALL_DIR="${INSTALL_DIR:-${HOME}/.local/bin}"
BOBBY_SHARE_DIR="${BOBBY_SHARE_DIR:-$(dirname "$INSTALL_DIR")/share/bobby-browser}"

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "install.sh: need \`$1\` on PATH" >&2
    exit 1
  }
}

need tar
need uname

os="$(uname -s | tr '[:upper:]' '[:lower:]')"
arch="$(uname -m)"
case "$os" in
  linux) asset_os=linux ;;
  darwin) asset_os=macos ;;
  *)
    echo "install.sh: unsupported OS: $os (use install.ps1 on Windows)" >&2
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
  if [[ -n "${BOBBY_ARCHIVE:-}" ]]; then
    echo "install.sh: BOBBY_VERSION is required with BOBBY_ARCHIVE" >&2
    exit 1
  fi
  need curl
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

if [[ -n "${BOBBY_ARCHIVE:-}" ]]; then
  archive="${BOBBY_ARCHIVE}"
  if [[ ! -f "$archive" ]]; then
    echo "install.sh: archive not found: ${archive}" >&2
    exit 1
  fi
else
  need curl
  archive="${tmpdir}/${ASSET}"
  echo "install.sh: fetching ${URL}"
  curl -fsSL -o "$archive" "$URL"
fi
tar -xzf "$archive" -C "$tmpdir"

src_dir="${tmpdir}/${STAGE}"
if [[ ! -f "${src_dir}/bobby" ]]; then
  echo "install.sh: archive missing ${STAGE}/bobby" >&2
  exit 1
fi

install_binary() {
  local source="$1"
  local destination="$2"
  local pending="${destination}.new.$$"
  install -m 755 "$source" "$pending"
  mv -f "$pending" "$destination"
}

replace_tree() {
  local source="$1"
  local destination="$2"
  local pending="${destination}.new.$$"
  local previous="${destination}.old.$$"
  rm -rf "$pending" "$previous"
  mkdir -p "$(dirname "$destination")"
  cp -R "$source" "$pending"
  if [[ -e "$destination" || -L "$destination" ]]; then
    mv "$destination" "$previous"
  fi
  if mv "$pending" "$destination"; then
    rm -rf "$previous"
  else
    if [[ -e "$previous" || -L "$previous" ]]; then
      mv "$previous" "$destination"
    fi
    return 1
  fi
}

mkdir -p "$INSTALL_DIR"
install_binary "${src_dir}/bobby" "${INSTALL_DIR}/bobby"
echo "install.sh: installed ${INSTALL_DIR}/bobby"

for bin in mcp-gateway acp-gateway; do
  if [[ -f "${src_dir}/${bin}" ]]; then
    install_binary "${src_dir}/${bin}" "${INSTALL_DIR}/${bin}"
    echo "install.sh: installed ${INSTALL_DIR}/${bin}"
  else
    echo "install.sh: warn: archive missing ${bin} (older release?); MCP/ACP hosts need it beside bobby" >&2
  fi
done

vision_share="${BOBBY_SHARE_DIR}/scripts/vision-mlx"
if [[ -d "${src_dir}/scripts/vision-mlx" ]]; then
  replace_tree "${src_dir}/scripts/vision-mlx" "$vision_share"
  echo "install.sh: installed ${vision_share}"
fi

companion_share="${BOBBY_SHARE_DIR}/firefox-companion"
if [[ -d "${src_dir}/firefox-companion" ]]; then
  replace_tree "${src_dir}/firefox-companion" "$companion_share"
  echo "install.sh: installed ${companion_share}"
fi

if ! command -v bobby >/dev/null 2>&1; then
  echo "install.sh: add ${INSTALL_DIR} to PATH, then run: bobby doctor" >&2
else
  echo "install.sh: next: bobby doctor"
fi
