#!/usr/bin/env bash
set -euo pipefail

for variable in BOBBY_FIREFOX_BIN BOBBY_FIREFOX_PROFILE BOBBY_COMPANION_EXTENSION; do
  if [[ -z "${!variable:-}" ]]; then
    echo "missing required variable: ${variable}" >&2
    exit 2
  fi
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
cd "$repo_root"

if [[ ! -x "$BOBBY_FIREFOX_BIN" ]]; then
  echo "BOBBY_FIREFOX_BIN must name an executable file" >&2
  exit 2
fi
if [[ ! -d "$BOBBY_FIREFOX_PROFILE" ]]; then
  echo "BOBBY_FIREFOX_PROFILE must name a dedicated Firefox profile directory" >&2
  exit 2
fi
pnpm --filter @bobby-browser/firefox-companion build
if [[ ! -f "$BOBBY_COMPANION_EXTENSION/manifest.json" ]]; then
  echo "BOBBY_COMPANION_EXTENSION must name the companion extension build directory" >&2
  exit 2
fi
cargo build -p cli -p runtime-tests

echo "Starting the loopback-only companion proof. The one-time pairing code is printed once by the test harness."
cargo test -p runtime-tests --test firefox_companion \
  installed_firefox_completes_verified_native_input_workflow \
  -- --ignored --exact --nocapture
