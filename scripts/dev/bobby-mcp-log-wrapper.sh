#!/usr/bin/env bash
# Local repro wrapper: bobby mcp-stdio with server stderr captured.
# Point BOBBY_MCP_COMMAND at this file. Not committed.
LOG_DIR="${TMPDIR:-/tmp}/bobby-repro"
mkdir -p "$LOG_DIR"
export RUST_LOG="worker_pool=debug,session_manager=debug,page_runtime=debug"
SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
exec "$SELF_DIR/../../target/debug/bobby" "$@" 2>>"$LOG_DIR/mcp-$(date +%s)-$$.log"
