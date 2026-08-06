#!/usr/bin/env bash
set -euo pipefail

# Live Firefox behavioral dogfood — same env contract as scripts/dev/firefox-companion.sh.
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

# KeepAlive launch agent (com.bobby_browser.firefox) may own this profile.
# Dogfood must launch its own Firefox, so pause the agent for the duration
# and restore it afterward.
FIREFOX_AGENT_LABEL="com.bobby_browser.firefox"
FIREFOX_AGENT_PLIST="$HOME/Library/LaunchAgents/${FIREFOX_AGENT_LABEL}.plist"
FIREFOX_AGENT_PAUSED=0
restore_firefox_agent() {
  if [[ "$FIREFOX_AGENT_PAUSED" -eq 1 && -f "$FIREFOX_AGENT_PLIST" ]]; then
    echo "Restoring launch agent $FIREFOX_AGENT_LABEL"
    launchctl bootstrap "gui/$(id -u)" "$FIREFOX_AGENT_PLIST" 2>/dev/null \
      || launchctl load "$FIREFOX_AGENT_PLIST" 2>/dev/null \
      || true
  fi
}
trap restore_firefox_agent EXIT

if launchctl print "gui/$(id -u)/${FIREFOX_AGENT_LABEL}" >/dev/null 2>&1 \
  || launchctl list "$FIREFOX_AGENT_LABEL" >/dev/null 2>&1; then
  echo "Pausing KeepAlive launch agent $FIREFOX_AGENT_LABEL for exclusive dogfood use"
  launchctl bootout "gui/$(id -u)/${FIREFOX_AGENT_LABEL}" 2>/dev/null \
    || launchctl unload "$FIREFOX_AGENT_PLIST" 2>/dev/null \
    || true
  FIREFOX_AGENT_PAUSED=1
  # Stop the agent-owned Firefox so the profile lock clears.
  if command -v lsof >/dev/null 2>&1; then
    for _ in 1 2 3 4 5; do
      pids="$(lsof -t "$BOBBY_FIREFOX_PROFILE/.parentlock" 2>/dev/null || true)"
      if [[ -z "${pids}" ]]; then
        break
      fi
      kill $pids 2>/dev/null || true
      sleep 1
    done
  fi
fi

if command -v lsof >/dev/null 2>&1 && lsof "$BOBBY_FIREFOX_PROFILE/.parentlock" >/dev/null 2>&1; then
  echo "BOBBY_FIREFOX_PROFILE is already in use by a running Firefox." >&2
  echo "Quit that instance (dedicated dogfood profile must be exclusive), then retry." >&2
  lsof "$BOBBY_FIREFOX_PROFILE/.parentlock" >&2 || true
  exit 2
fi

pnpm --filter @cavi-ai/bobby-firefox-companion build
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
if [[ ! -f "$manifest_path" ]]; then
  "$repo_root/target/debug/bobby" install-firefox-native-host \
    --wrapper "$wrapper_path" \
    --manifest "$manifest_path" \
    --cli "$repo_root/target/debug/bobby" \
    --descriptor "$descriptor_path"
else
  echo "native host already installed at $manifest_path"
fi

echo "Starting Firefox behavioral dogfood (same BOBBY_FIREFOX_* / native-host setup as firefox-companion.sh)."
cargo test -p runtime-tests --test behavioral_firefox \
  installed_firefox_behavioral_dogfood_passes \
  -- --ignored --exact --nocapture
