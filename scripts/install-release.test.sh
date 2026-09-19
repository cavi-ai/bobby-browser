#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
root="$(mktemp -d)"
trap 'rm -rf "$root"' EXIT

version="0.14.0"
case "$(uname -s)" in
  Linux) asset_os="linux" ;;
  Darwin) asset_os="macos" ;;
  *) echo "unsupported test host" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) asset_arch="x64" ;;
  arm64 | aarch64) asset_arch="arm64" ;;
  *) echo "unsupported test architecture" >&2; exit 1 ;;
esac
stage_name="bobby-browser-${version}-${asset_os}-${asset_arch}"
stage="$root/$stage_name"
archive="$root/${stage_name}.tar.gz"

mkdir -p "$stage/scripts/vision-mlx/providers" "$stage/firefox-companion"
printf '%s\n' '#!/usr/bin/env bash' 'if [[ "${1:-}" == "--version" ]]; then echo "bobby-browser 0.14.0"; exit 0; fi' 'if [[ "${1:-}" == "profiles" && "${2:-}" == "--json" ]]; then echo '\''[{"name":"desktop"},{"name":"headless-ci"},{"name":"openshell"},{"name":"remote"}]'\''; exit 0; fi' 'exit 2' > "$stage/bobby"
printf '%s\n' '#!/usr/bin/env bash' 'exit 0' > "$stage/mcp-gateway"
printf '%s\n' '#!/usr/bin/env bash' 'exit 0' > "$stage/acp-gateway"
chmod +x "$stage/bobby" "$stage/mcp-gateway" "$stage/acp-gateway"
printf '%s\n' 'provider' > "$stage/scripts/vision-mlx/providers/__init__.py"
printf '%s\n' '{"manifest_version":2}' > "$stage/firefox-companion/manifest.json"
COPYFILE_DISABLE=1 tar -czf "$archive" -C "$root" "$stage_name"

python3 "$repo_root/scripts/certify-release-install.py" \
  --archive "$archive" \
  --asset-os "$asset_os" \
  --version "$version"
