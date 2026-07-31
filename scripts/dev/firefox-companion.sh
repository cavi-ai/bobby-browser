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
cargo build -p bobby-browser -p runtime-tests

if [[ -n "${BOBBY_NATIVE_MESSAGING_DIR:-}" ]]; then
  native_messaging_dir="$BOBBY_NATIVE_MESSAGING_DIR"
elif [[ "$(uname -s)" == "Darwin" ]]; then
  native_messaging_dir="${HOME}/Library/Application Support/Mozilla/NativeMessagingHosts"
elif [[ "$(uname -s)" == "Linux" ]]; then
  native_messaging_dir="${HOME}/.mozilla/native-messaging-hosts"
else
  echo "Firefox Native Messaging installation is unsupported on this platform" >&2
  exit 2
fi

wrapper_path="$repo_root/target/firefox-companion-proof/firefox-native-host"
descriptor_path="$repo_root/target/firefox-companion-proof/native-host-descriptor.json"
manifest_path="$native_messaging_dir/com.bobby_browser.companion.json"
"$repo_root/target/debug/bobby" install-firefox-native-host \
  --wrapper "$wrapper_path" \
  --manifest "$manifest_path" \
  --cli "$repo_root/target/debug/bobby" \
  --descriptor "$descriptor_path"

echo "Starting the loopback-only companion proof. Pairing material remains in owner-only files."
cargo test -p runtime-tests --test firefox_companion \
  installed_firefox_completes_verified_native_input_workflow \
  -- --ignored --exact --nocapture
