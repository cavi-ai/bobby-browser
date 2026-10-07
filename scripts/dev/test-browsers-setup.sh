#!/usr/bin/env bash
# One-time (idempotent) setup for `make test-browsers`: a scoped Firefox test
# profile, a copy of the companion extension scoped to a test native-host
# name, and ONLY that test host's manifest. The live host manifest
# `com.bobby_browser.companion.json` is never written.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
cd "$repo_root"

out="$repo_root/.tmp/test-browsers"
# The extension only accepts com.bobby_browser.companion[.scope_<16 hex>].
host_name="com.bobby_browser.companion.scope_7465737462726f77"
live_host_name="com.bobby_browser.companion"

case "$(uname -s)" in
  Darwin) manifest_dir="$HOME/Library/Application Support/Mozilla/NativeMessagingHosts" ;;
  Linux) manifest_dir="$HOME/.mozilla/native-messaging-hosts" ;;
  *) echo "unsupported platform: $(uname -s)" >&2; exit 2 ;;
esac
manifest="$manifest_dir/$host_name.json"

if [[ "$host_name" == "$live_host_name" || "$(basename "$manifest")" == "$live_host_name.json" ]]; then
  echo "refusing to write the live native host manifest $live_host_name.json" >&2
  exit 2
fi

cargo build -p bobby-browser -p runtime-tests --locked
pnpm --filter @cavi-ai/bobby-firefox-companion build

mkdir -p "$out/firefox-profile" "$out/extension" "$manifest_dir"
cp -R packages/firefox-companion/dist/. "$out/extension/"
printf '{"nativeHostName":"%s"}\n' "$host_name" > "$out/extension/bobby-scope.json"

# A stable CLI path: rebuilding target/debug/bobby needs no reinstall.
printf '#!/bin/sh\nexec "%s/target/debug/bobby" "$@"\n' "$repo_root" > "$out/bobby-cli"
chmod 755 "$out/bobby-cli"

"$repo_root/target/debug/bobby" install-firefox-native-host \
  --wrapper "$out/firefox-native-host" \
  --manifest "$manifest" \
  --cli "$out/bobby-cli" \
  --descriptor "$out/native-host-descriptor.json"

echo "test native host installed: $manifest"
