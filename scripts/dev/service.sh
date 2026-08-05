#!/usr/bin/env bash
#
# Manage the long-running launchd service that hosts `target/release/bobby serve`.
#
#   service.sh start    bootstrap the launchd agent (no rebuild)
#   service.sh stop     bootout the launchd agent (KeepAlive cannot respawn)
#   service.sh reload   rebuild, restart, verify
#   service.sh verify   restart and verify without rebuilding
#   service.sh status   report launchd state, port health, and binary freshness
#
# Why this exists: the service keeps serving whatever binary existed when it last
# started, so editing Rust source changes nothing until both a rebuild AND a
# restart happen. A stale binary looks completely healthy — port open, /healthz
# green — while answering with old protocol behavior.
#
# KeepAlive=true on the plist means `kill` alone does nothing useful — launchd
# respawns. Use `stop` (bootout) to actually shut the MCP HTTP server down.
#
# The MCP check sends `initialize` twice on purpose. One `mcp_gateway::Server` is
# cached per principal for the life of the process, so a server that rejects a
# re-`initialize` answers the first handshake and then fails every reconnect for
# as long as the process lives. Streamable-HTTP clients re-`initialize` on every
# reconnect, so only the second handshake catches that class of regression.
#
# Environment:
#   SERVICE_LABEL        launchd label             (default com.mirza.bobby-browser)
#   SERVICE_PLIST        launchd plist path        (default ~/Library/LaunchAgents/$SERVICE_LABEL.plist)
#   BOBBY_BROWSER_PORT   serve port                (default 7777)
#   BOBBY_BROWSER_TOKEN  bearer for the MCP check  (default: skip the MCP check)

set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
SERVICE_LABEL="${SERVICE_LABEL:-com.mirza.bobby-browser}"
SERVICE_PLIST="${SERVICE_PLIST:-$HOME/Library/LaunchAgents/$SERVICE_LABEL.plist}"
PORT="${BOBBY_BROWSER_PORT:-7777}"
BINARY="$REPO_ROOT/target/release/bobby"

log() { printf '==> %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || die "launchd service management is macOS-only"

service_pid() {
  launchctl print "gui/$(id -u)/$SERVICE_LABEL" 2>/dev/null \
    | awk '/^[[:space:]]*pid = /{print $3; exit}'
}

mtime() { stat -f %m "$1" 2>/dev/null || echo 0; }

build() {
  log "building release cli"
  cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml" -p bobby-browser
}

restart() {
  log "restarting $SERVICE_LABEL"
  if ! launchctl kickstart -k "gui/$(id -u)/$SERVICE_LABEL" 2>/dev/null; then
    [ -f "$SERVICE_PLIST" ] || die "service not loaded and no plist at $SERVICE_PLIST"
    launchctl bootstrap "gui/$(id -u)" "$SERVICE_PLIST"
  fi
}

start() {
  if launchctl print "gui/$(id -u)/$SERVICE_LABEL" >/dev/null 2>&1; then
    log "$SERVICE_LABEL already loaded"
    return 0
  fi
  [ -f "$SERVICE_PLIST" ] || die "no plist at $SERVICE_PLIST"
  log "starting $SERVICE_LABEL"
  launchctl bootstrap "gui/$(id -u)" "$SERVICE_PLIST"
}

stop() {
  if ! launchctl print "gui/$(id -u)/$SERVICE_LABEL" >/dev/null 2>&1; then
    log "$SERVICE_LABEL not loaded"
    return 0
  fi
  log "stopping $SERVICE_LABEL (bootout — KeepAlive will not respawn)"
  launchctl bootout "gui/$(id -u)/$SERVICE_LABEL"
}

wait_healthy() {
  log "waiting for /healthz on port $PORT"
  for _ in $(seq 1 60); do
    if curl -fsS --max-time 2 "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  # KeepAlive respawns a binary that exits at startup, so the service reports
  # "running" while never listening. The startup error is the useful signal.
  local stderr_log
  stderr_log="$(plutil -extract StandardErrorPath raw -o - "$SERVICE_PLIST" 2>/dev/null || true)"
  if [ -n "$stderr_log" ] && [ -f "$stderr_log" ]; then
    printf 'last lines of %s:\n' "$stderr_log" >&2
    tail -n 5 "$stderr_log" >&2
    if tail -n 5 "$stderr_log" | grep -q 'journal line .* is corrupt'; then
      cat >&2 <<'HINT'

That journal was written by an older build whose CommandEnvelope schema no
longer deserializes; a SCHEMA_VERSION bump is the marker, and the journal has no
migration path. Move data/storage/commands.jsonl aside (relative to the
service's working directory) and re-run, or point storage.journal_path at a
fresh file.
HINT
    fi
  fi
  die "service did not become healthy on port $PORT"
}

mcp_check() {
  if [ -z "${BOBBY_BROWSER_TOKEN:-}" ]; then
    log "BOBBY_BROWSER_TOKEN unset — skipping MCP handshake check"
    return 0
  fi
  # Read the pinned version out of the source rather than duplicating it here.
  # The server accepts exactly one protocol version and answers anything else
  # with -32602, so a hardcoded copy would silently start failing on the next bump.
  local version
  version="$(
    sed -n 's/.*MCP_PROTOCOL_VERSION: &str = "\([^"]*\)".*/\1/p' \
      "$REPO_ROOT/crates/mcp-gateway/src/protocol.rs"
  )"
  [ -n "$version" ] || die "could not read MCP_PROTOCOL_VERSION from crates/mcp-gateway/src/protocol.rs"

  log "MCP handshake check (initialize, then re-initialize)"
  local attempt response
  for attempt in 1 2; do
    response="$(
      curl -fsS --max-time 10 "http://127.0.0.1:$PORT/v1/mcp" \
        -H 'content-type: application/json' \
        -H 'accept: application/json' \
        -H "authorization: Bearer $BOBBY_BROWSER_TOKEN" \
        -d "{\"jsonrpc\":\"2.0\",\"id\":$attempt,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"$version\",\"capabilities\":{},\"clientInfo\":{\"name\":\"service.sh\",\"version\":\"1\"}}}"
    )" || die "initialize #$attempt: request failed"
    case "$response" in
      *'"result"'*) ;;
      *) die "initialize #$attempt rejected: $response" ;;
    esac
  done
}

# Freshness is judged against HEAD's commit timestamp and the running process,
# never against source file mtimes: editor saves, formatters, and repo tooling
# all touch mtimes without changing content, which makes an mtime comparison
# report stale constantly and train everyone to ignore it.
status() {
  local failed=0 pid binary_mtime head_epoch proc_start

  log "launchd"
  pid="$(service_pid || true)"
  if [ -n "$pid" ]; then
    note "running, pid $pid"
  else
    note "not loaded"
    failed=1
  fi

  log "port $PORT"
  if curl -fsS --max-time 2 "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
    note "healthz ok"
  else
    note "healthz unreachable"
    failed=1
  fi

  log "binary"
  if [ ! -x "$BINARY" ]; then
    note "no release binary — run: make reload"
    return 1
  fi
  binary_mtime="$(mtime "$BINARY")"

  head_epoch="$(git -C "$REPO_ROOT" log -1 --format=%ct 2>/dev/null || echo 0)"
  if [ "$head_epoch" -gt 0 ] && [ "$binary_mtime" -lt "$head_epoch" ]; then
    note "STALE: binary predates HEAD ($(git -C "$REPO_ROOT" log -1 --format=%h)) — run: make reload"
    failed=1
  else
    note "built at or after HEAD"
  fi

  if git -C "$REPO_ROOT" status --porcelain -- '*.rs' 2>/dev/null | grep -q .; then
    note "uncommitted .rs changes present — binary may not include them"
  fi

  if [ -n "$pid" ]; then
    proc_start="$(ps -o lstart= -p "$pid" 2>/dev/null || true)"
    if [ -n "$proc_start" ]; then
      proc_start="$(date -j -f '%a %b %e %T %Y' "$proc_start" +%s 2>/dev/null || echo 0)"
      if [ "$proc_start" -gt 0 ] && [ "$binary_mtime" -gt "$proc_start" ]; then
        note "STALE: binary is newer than the running process — run: make verify"
        failed=1
      fi
    fi
  fi

  return "$failed"
}

case "${1:-reload}" in
  start)
    start
    wait_healthy
    log "done"
    ;;
  stop)
    stop
    log "done"
    ;;
  reload)
    build
    [ -x "$BINARY" ] || die "missing release binary: $BINARY"
    restart
    wait_healthy
    mcp_check
    log "done"
    ;;
  verify)
    [ -x "$BINARY" ] || die "missing release binary: $BINARY"
    restart
    wait_healthy
    mcp_check
    log "done"
    ;;
  status)
    status
    ;;
  *)
    die "usage: service.sh {start|stop|reload|verify|status}"
    ;;
esac
