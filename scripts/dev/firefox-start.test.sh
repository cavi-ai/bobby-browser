#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
LAUNCHER="$SCRIPT_DIR/firefox-start.sh"

fail() {
  printf 'not ok - %s\n' "$1" >&2
  exit 1
}

make_fixture() {
  FIXTURE="$(mktemp -d)"
  PROFILE="$FIXTURE/profile"
  BIN="$FIXTURE/bin"
  mkdir -p "$PROFILE" "$BIN"
  export FIXTURE PROFILE
  export BOBBY_FIREFOX_PROFILE="$PROFILE"
  export BOBBY_BROWSER_STATE="$FIXTURE/state"
  export BOBBY_FIREFOX_BIN="$FIXTURE/fake-firefox"
  export BOBBY_FIREFOX_DEBUG_PORT=9222
  export PATH="$BIN:/usr/bin:/bin:/usr/sbin:/sbin"
}

cleanup_fixture() {
  if [[ -n "${FIXTURE:-}" && -f "$FIXTURE/firefox.pid" ]]; then
    kill "$(cat "$FIXTURE/firefox.pid")" 2>/dev/null || true
  fi
  if [[ -n "${FIXTURE:-}" && -d "$FIXTURE" ]]; then
    rm -rf "$FIXTURE"
  fi
}

install_owned_listener_fakes() {
  cat >"$BIN/nc" <<'EOF'
#!/usr/bin/env bash
[[ -f "$FIXTURE/firefox.pid" ]]
EOF
  cat >"$BIN/lsof" <<'EOF'
#!/usr/bin/env bash
[[ -f "$FIXTURE/firefox.pid" ]] && cat "$FIXTURE/firefox.pid"
EOF
  cat >"$BIN/sleep" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  chmod +x "$BIN/nc" "$BIN/lsof" "$BIN/sleep"
}

test_rejects_foreign_listener_before_launch() {
  make_fixture
  trap cleanup_fixture RETURN

  cat >"$BOBBY_FIREFOX_BIN" <<'EOF'
#!/usr/bin/env bash
touch "$FIXTURE/firefox-was-launched"
EOF
  chmod +x "$BOBBY_FIREFOX_BIN"

  cat >"$BIN/nc" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  cat >"$BIN/lsof" <<'EOF'
#!/usr/bin/env bash
printf '4242\n'
EOF
  chmod +x "$BIN/nc" "$BIN/lsof"

  if output="$($LAUNCHER start 2>&1)"; then
    fail "foreign listener was accepted"
  fi
  [[ "$output" == *"port 9222 is already in use"* ]] \
    || fail "foreign-listener error did not identify port 9222: $output"
  [[ ! -e "$FIXTURE/firefox-was-launched" ]] \
    || fail "Firefox was launched despite the foreign listener"
  printf 'ok - rejects a foreign listener before launching Firefox\n'
}

test_surfaces_early_exit_diagnostics() {
  make_fixture
  trap cleanup_fixture RETURN

  cat >"$BOBBY_FIREFOX_BIN" <<'EOF'
#!/usr/bin/env bash
printf 'remote agent could not bind requested port\n' >&2
exit 23
EOF
  chmod +x "$BOBBY_FIREFOX_BIN"

  cat >"$BIN/nc" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
  cat >"$BIN/lsof" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
  cat >"$BIN/sleep" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  chmod +x "$BIN/nc" "$BIN/lsof" "$BIN/sleep"

  if output="$($LAUNCHER start 2>&1)"; then
    fail "early Firefox exit was reported as success"
  fi
  [[ "$output" == *"remote agent could not bind requested port"* ]] \
    || fail "Firefox stderr was discarded: $output"
  printf 'ok - surfaces diagnostics when Firefox exits during startup\n'
}

test_rejects_stale_endpoint_file() {
  make_fixture
  trap cleanup_fixture RETURN
  printf '{"ws_host":"127.0.0.1","ws_port":9222}\n' >"$PROFILE/WebDriverBiDiServer.json"
  touch -t 202001010000 "$PROFILE/WebDriverBiDiServer.json"

  cat >"$BOBBY_FIREFOX_BIN" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$$" >"$FIXTURE/firefox.pid"
touch "$PROFILE/.parentlock"
/bin/sleep 2
EOF
  chmod +x "$BOBBY_FIREFOX_BIN"
  install_owned_listener_fakes

  if output="$($LAUNCHER start 2>&1)"; then
    fail "stale BiDi endpoint was accepted"
  fi
  [[ "$output" == *"did not establish a fresh, owned BiDi endpoint"* ]] \
    || fail "stale-endpoint failure was not explained: $output"
  printf 'ok - rejects a stale BiDi endpoint file\n'
}

test_accepts_fresh_endpoint_owned_by_launched_firefox() {
  make_fixture
  trap cleanup_fixture RETURN

  cat >"$BOBBY_FIREFOX_BIN" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$$" >"$FIXTURE/firefox.pid"
touch "$PROFILE/.parentlock"
printf '{\n  "ws_host": "127.0.0.1",\n  "ws_port": 9222\n}\n' >"$PROFILE/WebDriverBiDiServer.json"
/bin/sleep 2
EOF
  chmod +x "$BOBBY_FIREFOX_BIN"
  install_owned_listener_fakes

  output="$($LAUNCHER start 2>&1)" \
    || fail "fresh owned endpoint was rejected: $output"
  [[ "$output" == *"BiDi listening on 127.0.0.1:9222"* ]] \
    || fail "successful launch did not report readiness: $output"
  printf 'ok - accepts a fresh endpoint owned by launched Firefox\n'
}

# 9222 is the port authenticated CDP binds. When this launcher defaulted there
# too, whichever of the two started second failed to bind.
test_default_debug_port_does_not_collide_with_cdp() {
  make_fixture
  trap cleanup_fixture RETURN
  unset BOBBY_FIREFOX_DEBUG_PORT

  cat >"$BOBBY_FIREFOX_BIN" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$$" >"$FIXTURE/firefox.pid"
touch "$PROFILE/.parentlock"
printf '%s\n' "$*" >"$FIXTURE/firefox.args"
printf '{"ws_host":"127.0.0.1","ws_port":9224}\n' >"$PROFILE/WebDriverBiDiServer.json"
/bin/sleep 2
EOF
  chmod +x "$BOBBY_FIREFOX_BIN"
  install_owned_listener_fakes

  output="$($LAUNCHER start 2>&1)" \
    || fail "default-port launch was rejected: $output"
  [[ "$output" == *"BiDi listening on 127.0.0.1:9224"* ]] \
    || fail "default port is not 9224: $output"
  [[ "$(cat "$FIXTURE/firefox.args")" == *"--remote-debugging-port=9224"* ]] \
    || fail "Firefox was not launched on the default 9224: $(cat "$FIXTURE/firefox.args")"
  [[ "$(cat "$FIXTURE/firefox.args")" != *"9222"* ]] \
    || fail "launcher still reaches for the CDP port: $(cat "$FIXTURE/firefox.args")"
  printf 'ok - default debug port does not collide with the CDP port\n'
}

# Firefox pretty-prints this file, leaving the port last on its line.
test_accepts_a_pretty_printed_endpoint_file() {
  make_fixture
  trap cleanup_fixture RETURN

  cat >"$BOBBY_FIREFOX_BIN" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$$" >"$FIXTURE/firefox.pid"
touch "$PROFILE/.parentlock"
printf '{\n  "ws_host": "127.0.0.1",\n  "ws_port": 9222\n}\n' >"$PROFILE/WebDriverBiDiServer.json"
/bin/sleep 2
EOF
  chmod +x "$BOBBY_FIREFOX_BIN"
  install_owned_listener_fakes

  output="$($LAUNCHER start 2>&1)" \
    || fail "pretty-printed endpoint was rejected: $output"
  [[ "$output" == *"BiDi listening on 127.0.0.1:9222"* ]] \
    || fail "pretty-printed endpoint did not report readiness: $output"
  printf 'ok - accepts a pretty-printed endpoint file\n'
}

# The port match must not be a prefix match: :9222 is not :92220.
test_rejects_an_endpoint_whose_port_only_shares_a_prefix() {
  make_fixture
  trap cleanup_fixture RETURN

  cat >"$BOBBY_FIREFOX_BIN" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$$" >"$FIXTURE/firefox.pid"
touch "$PROFILE/.parentlock"
printf '{"ws_host":"127.0.0.1","ws_port":92220}\n' >"$PROFILE/WebDriverBiDiServer.json"
/bin/sleep 2
EOF
  chmod +x "$BOBBY_FIREFOX_BIN"
  install_owned_listener_fakes

  if output="$($LAUNCHER start 2>&1)"; then
    fail "an endpoint on :92220 was accepted for :9222"
  fi
  [[ "$output" == *"did not establish a fresh, owned BiDi endpoint"* ]] \
    || fail "prefix-port failure was not explained: $output"
  printf 'ok - rejects an endpoint whose port only shares a prefix\n'
}

# On macOS the launched binary re-execs through the app bundle, so the browser
# holding the profile lock and the BiDi port is not the pid bash backgrounded.
test_accepts_a_relaunched_browser_under_a_different_pid() {
  make_fixture
  trap cleanup_fixture RETURN

  cat >"$BOBBY_FIREFOX_BIN" <<'EOF'
#!/usr/bin/env bash
/bin/sleep 2 &
printf '%s\n' "$!" >"$FIXTURE/firefox.pid"
touch "$PROFILE/.parentlock"
printf '{"ws_host":"127.0.0.1","ws_port":9222}\n' >"$PROFILE/WebDriverBiDiServer.json"
EOF
  chmod +x "$BOBBY_FIREFOX_BIN"
  install_owned_listener_fakes

  output="$($LAUNCHER start 2>&1)" \
    || fail "re-exec'd browser was rejected: $output"
  [[ "$output" == *"BiDi listening on 127.0.0.1:9222"* ]] \
    || fail "re-exec'd browser did not report readiness: $output"
  printf 'ok - accepts a relaunched browser under a different pid\n'
}

test_rejects_foreign_listener_before_launch
test_surfaces_early_exit_diagnostics
test_default_debug_port_does_not_collide_with_cdp
test_rejects_stale_endpoint_file
test_accepts_fresh_endpoint_owned_by_launched_firefox
test_accepts_a_pretty_printed_endpoint_file
test_rejects_an_endpoint_whose_port_only_shares_a_prefix
test_accepts_a_relaunched_browser_under_a_different_pid
