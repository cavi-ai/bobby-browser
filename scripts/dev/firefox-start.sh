#!/usr/bin/env bash
#
# Start the Bobby Firefox profile with BiDi remote debugging.
# No launchd. Local agents use `bobby mcp-stdio` — this only needs Firefox up
# so you can Pair from the companion toolbar popup.
#
#   firefox-start.sh start   launch (default)
#   firefox-start.sh stop    quit the profile's Firefox (+ legacy KeepAlive bootout)
#
# Env:
#   BOBBY_FIREFOX_BIN          firefox binary (default: discover)
#   BOBBY_FIREFOX_PROFILE      profile dir (default: <config>/firefox-profile)
#   BOBBY_FIREFOX_DEBUG_PORT   remote debugging port (default: 9224)
#   BOBBY_BROWSER_STATE        override config/state dir (default: platform bobby-browser dir)
#
# 9224, not 9222: 9222 is the port authenticated CDP binds ([cdp].port), and the
# two ran in the same product on the same host. Whichever started second failed
# to bind. CDP keeps 9222 because that is the port DevTools clients expect; this
# endpoint is internal to bobby and is discovered from the profile's
# WebDriverBiDiServer.json, so moving it costs nothing.

set -euo pipefail

action="${1:-start}"
PORT="${BOBBY_FIREFOX_DEBUG_PORT:-9224}"

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
log() { printf '==> %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }

state_dir() {
  if [[ -n "${BOBBY_BROWSER_STATE:-}" ]]; then
    printf '%s\n' "$BOBBY_BROWSER_STATE"
    return
  fi
  case "$(uname -s)" in
    Darwin)
      printf '%s\n' "${HOME}/Library/Application Support/bobby-browser"
      ;;
    Linux)
      printf '%s\n' "${XDG_CONFIG_HOME:-$HOME/.config}/bobby-browser"
      ;;
    *)
      die "unsupported platform: $(uname -s)"
      ;;
  esac
}

discover_firefox() {
  if [[ -n "${BOBBY_FIREFOX_BIN:-}" ]]; then
    printf '%s\n' "$BOBBY_FIREFOX_BIN"
    return
  fi
  local candidate
  for candidate in \
    "/Applications/Firefox Developer Edition.app/Contents/MacOS/firefox" \
    "/Applications/Firefox.app/Contents/MacOS/firefox" \
    "$(command -v firefox-developer-edition 2>/dev/null || true)" \
    "$(command -v firefox 2>/dev/null || true)"; do
    if [[ -n "$candidate" && -x "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return
    fi
  done
  die "Firefox not found; set BOBBY_FIREFOX_BIN or install Firefox Developer Edition"
}

profile_lock_pids() {
  local profile="$1"
  if [[ ! -e "$profile/.parentlock" ]] || ! command -v lsof >/dev/null 2>&1; then
    return 0
  fi
  lsof -t "$profile/.parentlock" 2>/dev/null || true
}

bidi_reachable() {
  if command -v nc >/dev/null 2>&1; then
    nc -z 127.0.0.1 "$PORT" >/dev/null 2>&1
  else
    return 1
  fi
}

listener_pids() {
  if command -v lsof >/dev/null 2>&1; then
    lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t 2>/dev/null || true
  fi
}

endpoint_is_fresh() {
  local endpoint="$1"
  [[ -f "$endpoint" ]] || return 1
  tr -d '[:space:]' <"$endpoint" \
    | grep -Eq '"ws_port":'"$PORT"'([,}])'
}

print_startup_log() {
  local log_file="$1"
  if [[ -s "$log_file" ]]; then
    printf '%s\n' "--- Firefox startup log: $log_file ---" >&2
    tail -n 40 "$log_file" >&2
  fi
}

start_firefox() {
  local bin profile pids listener endpoint log_file firefox_pid status
  bin="$(discover_firefox)"
  profile="${BOBBY_FIREFOX_PROFILE:-$(state_dir)/firefox-profile}"

  [[ -d "$profile" ]] || die "profile missing at $profile (run: make firefox / bobby install --companion)"

  pids="$(profile_lock_pids "$profile")"
  if [[ -n "${pids}" ]]; then
    if bidi_reachable; then
      log "Bobby Firefox already running (profile lock held; BiDi :$PORT up)"
      note "Pair from the toolbar popup if needed. Agents: bobby mcp-stdio (no serve)."
      return 0
    fi
    die "profile already in use but BiDi :$PORT is not reachable — quit that Firefox and retry
$(lsof "$profile/.parentlock" 2>/dev/null || true)"
  fi

  listener="$(listener_pids)"
  if [[ -n "$listener" ]] || bidi_reachable; then
    die "port $PORT is already in use by another process (listener pids: ${listener:-unknown}); stop it or set BOBBY_FIREFOX_DEBUG_PORT"
  fi

  endpoint="$profile/WebDriverBiDiServer.json"
  rm -f "$endpoint"
  log_file="${BOBBY_FIREFOX_LOG:-$(state_dir)/logs/firefox-start.log}"
  mkdir -p "$(dirname "$log_file")"
  : >"$log_file"

  log "starting Bobby Firefox"
  note "bin:     $bin"
  note "profile: $profile"
  note "BiDi:    --remote-debugging-port=$PORT"
  "$bin" --no-remote --foreground \
    --profile "$profile" \
    --remote-debugging-port="$PORT" \
    about:blank >"$log_file" 2>&1 &
  firefox_pid=$!
  disown || true

  local i
  for i in 1 2 3 4 5 6 7 8 9 10; do
    if ! kill -0 "$firefox_pid" 2>/dev/null; then
      set +e
      wait "$firefox_pid"
      status=$?
      set -e
      print_startup_log "$log_file"
      die "Firefox exited during startup (status $status)"
    fi

    pids="$(profile_lock_pids "$profile")"
    listener="$(listener_pids)"
    if endpoint_is_fresh "$endpoint" \
      && bidi_reachable \
      && [[ " $pids " == *" $firefox_pid "* ]] \
      && [[ " $listener " == *" $firefox_pid "* ]]; then
      log "BiDi listening on 127.0.0.1:$PORT"
      note "Next: Pair from the companion toolbar popup."
      note "Local agents use bobby mcp-stdio — bobby serve is optional (HTTP only)."
      return 0
    fi
    sleep 0.5
  done

  print_startup_log "$log_file"
  die "Firefox is running but did not establish a fresh, owned BiDi endpoint on :$PORT"
}

stop_firefox() {
  local profile pids label="com.bobby_browser.firefox"
  profile="${BOBBY_FIREFOX_PROFILE:-$(state_dir)/firefox-profile}"

  # Legacy KeepAlive agent, if someone still has it loaded.
  if [[ "$(uname -s)" == "Darwin" ]] \
    && launchctl print "gui/$(id -u)/$label" >/dev/null 2>&1; then
    log "stopping legacy launchd agent $label (bootout)"
    launchctl bootout "gui/$(id -u)/$label" || true
  fi

  pids="$(profile_lock_pids "$profile")"
  if [[ -z "${pids}" ]]; then
    log "Bobby Firefox not running"
    return 0
  fi

  log "stopping Bobby Firefox (pids: $pids)"
  # shellcheck disable=SC2086
  kill $pids 2>/dev/null || true
  local i
  for i in 1 2 3 4 5 6 7 8 9 10; do
    pids="$(profile_lock_pids "$profile")"
    [[ -z "${pids}" ]] && break
    # shellcheck disable=SC2086
    kill $pids 2>/dev/null || true
    sleep 0.5
  done
  pids="$(profile_lock_pids "$profile")"
  if [[ -n "${pids}" ]]; then
    # shellcheck disable=SC2086
    kill -9 $pids 2>/dev/null || true
  fi
  log "stopped"
}

case "$action" in
  start) start_firefox ;;
  stop) stop_firefox ;;
  *) die "usage: $0 {start|stop}" ;;
esac
