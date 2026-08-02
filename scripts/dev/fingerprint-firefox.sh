#!/usr/bin/env bash
set -euo pipefail

# Live Firefox fingerprint collector dogfood — same env contract as
# scripts/dev/behavioral-firefox.sh / firefox-companion.sh.
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
if command -v lsof >/dev/null 2>&1 && lsof "$BOBBY_FIREFOX_PROFILE/.parentlock" >/dev/null 2>&1; then
  echo "BOBBY_FIREFOX_PROFILE is already in use by a running Firefox." >&2
  echo "Quit that instance (dedicated dogfood profile must be exclusive), then retry." >&2
  lsof "$BOBBY_FIREFOX_PROFILE/.parentlock" >&2 || true
  exit 2
fi
pnpm --filter @bobby-browser/firefox-companion build
if [[ ! -f "$BOBBY_COMPANION_EXTENSION/manifest.json" ]]; then
  echo "BOBBY_COMPANION_EXTENSION must name the companion extension build directory" >&2
  exit 2
fi
cargo build -p bobby-browser -p runtime-tests

if [[ -n "${BOBBY_NATIVE_MESSAGING_DIR:-}" ]]; then
  native_messaging_dirs=("$BOBBY_NATIVE_MESSAGING_DIR")
elif [[ "$(uname -s)" == "Darwin" ]]; then
  # Release Firefox uses Mozilla/; Developer Edition / Nightly also check Firefox/.
  native_messaging_dirs=(
    "${HOME}/Library/Application Support/Mozilla/NativeMessagingHosts"
    "${HOME}/Library/Application Support/Firefox/NativeMessagingHosts"
  )
elif [[ "$(uname -s)" == "Linux" ]]; then
  native_messaging_dirs=("${HOME}/.mozilla/native-messaging-hosts")
else
  echo "Firefox Native Messaging installation is unsupported on this platform" >&2
  exit 2
fi

wrapper_path="$repo_root/target/firefox-companion-proof/firefox-native-host"
descriptor_path="$repo_root/target/firefox-companion-proof/native-host-descriptor.json"
for native_messaging_dir in "${native_messaging_dirs[@]}"; do
  mkdir -p "$native_messaging_dir"
  manifest_path="$native_messaging_dir/com.bobby_browser.companion.json"
  if [[ -f "$manifest_path" && -x "$wrapper_path" ]]; then
    echo "Native messaging host already installed; reusing $manifest_path"
    continue
  fi
  if [[ -f "$manifest_path" ]]; then
    # Manifest exists but wrapper path differs — copy from primary Mozilla install if present.
    primary="${HOME}/Library/Application Support/Mozilla/NativeMessagingHosts/com.bobby_browser.companion.json"
    if [[ "$manifest_path" != "$primary" && -f "$primary" ]]; then
      cp "$primary" "$manifest_path"
      echo "Copied native messaging manifest to $manifest_path"
      continue
    fi
  fi
  "$repo_root/target/debug/bobby" install-firefox-native-host \
    --wrapper "$wrapper_path" \
    --manifest "$manifest_path" \
    --cli "$repo_root/target/debug/bobby" \
    --descriptor "$descriptor_path" || true
done

# Ensure Dev Edition path has a manifest even when install was skipped/no-clobber.
if [[ "$(uname -s)" == "Darwin" ]]; then
  primary="${HOME}/Library/Application Support/Mozilla/NativeMessagingHosts/com.bobby_browser.companion.json"
  dev_edition="${HOME}/Library/Application Support/Firefox/NativeMessagingHosts/com.bobby_browser.companion.json"
  if [[ -f "$primary" ]]; then
    mkdir -p "$(dirname "$dev_edition")"
    cp "$primary" "$dev_edition"
  fi
fi

echo "Starting Firefox fingerprint collector dogfood (BrowserLeaks/CreepJS/FingerprintJS)."
cargo test -p runtime-tests --test fingerprint_firefox \
  installed_firefox_fingerprint_collector_dogfood_passes \
  -- --ignored --exact --nocapture
