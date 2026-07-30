#!/usr/bin/env bash
# Ordered crates.io publish for the bobby-browser install closure.
# Requires: cargo login (crates.io token). Run from repo root.
set -euo pipefail

crates=(
  bobby-chromiumoxide
  types
  config
  companion-protocol
  dom-engine
  js-engine
  network-engine
  skill-runtime
  workflow-journal
  checkpoint-store
  artifact-store
  test-site
  observability
  companion-core
  intent-engine
  worker-pool
  session-manager
  page-runtime
  interface-core
  sdk-core
  mcp-gateway
  broker
  firefox-companion
  bobby-browser
)

dry_run=0
if [[ "${1:-}" == "--dry-run" ]]; then
  dry_run=1
fi

for crate in "${crates[@]}"; do
  echo "==> publishing $crate"
  if [[ "$dry_run" -eq 1 ]]; then
    cargo publish -p "$crate" --dry-run
  else
    cargo publish -p "$crate"
  fi
done
