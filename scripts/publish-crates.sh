#!/usr/bin/env bash
# Publish the Rust SDK crate to crates.io.
# Requires: cargo login (crates.io token). Run from repo root.
set -euo pipefail

crate=bobby-browser-client

dry_run=0
if [[ "${1:-}" == "--dry-run" ]]; then
  dry_run=1
fi

echo "==> publishing $crate"
if [[ "$dry_run" -eq 1 ]]; then
  cargo publish -p "$crate" --dry-run
else
  cargo publish -p "$crate"
fi
