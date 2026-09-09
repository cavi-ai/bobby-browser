#!/usr/bin/env bash
# Real end-to-end proof for the bobby-browser Docker image: builds the
# compose profile, waits for the runtime to become healthy, pulls the
# bootstrap bearer out of the named volume, then drives one MCP streamable
# HTTP session (initialize -> notifications/initialized -> session_create)
# exactly the way an external MCP client would. Exit status is the proof.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

for bin in docker jq curl; do
    if ! command -v "$bin" >/dev/null 2>&1; then
        echo "smoke: required binary '$bin' not found on PATH" >&2
        exit 1
    fi
done

BASE_URL="http://127.0.0.1:7777"

cleanup() {
    local status=$?
    echo "smoke: docker compose down -v" >&2
    docker compose down -v >/dev/null 2>&1 || true
    exit "$status"
}
trap cleanup EXIT

echo "smoke: docker compose up -d --build" >&2
docker compose up -d --build

echo "smoke: waiting up to 120s for GET /healthz == 200" >&2
healthy=0
for _ in $(seq 1 60); do
    code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE_URL/healthz" || true)
    if [ "$code" = "200" ]; then
        healthy=1
        break
    fi
    sleep 2
done
if [ "$healthy" -ne 1 ]; then
    echo "smoke: /healthz never returned 200" >&2
    docker compose logs bobby >&2 || true
    exit 1
fi
echo "smoke: /healthz OK" >&2

echo "smoke: reading bootstrap bearer from the container" >&2
BEARER=$(docker compose exec -T bobby bobby token --stdout)
if [ -z "$BEARER" ]; then
    echo "smoke: empty bearer from 'bobby token --stdout'" >&2
    exit 1
fi

# MCP over streamable HTTP is bearer-only (docs/.../surfaces/mcp-http.md):
# no x-interface-version / x-correlation-id / x-deadline on this route.
mcp_post() {
    curl -sS -X POST "$BASE_URL/v1/mcp" \
        -H "Authorization: Bearer ${BEARER}" \
        -H "Content-Type: application/json" \
        -H "Accept: application/json" \
        -d "$1"
}

echo "smoke: POST /v1/mcp initialize" >&2
INIT_RESPONSE=$(mcp_post '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"smoke.sh","version":"0"}}}')
PROTOCOL_VERSION=$(echo "$INIT_RESPONSE" | jq -r '.result.protocolVersion // empty')
if [ -z "$PROTOCOL_VERSION" ]; then
    echo "smoke: initialize did not return a protocolVersion: $INIT_RESPONSE" >&2
    exit 1
fi
echo "smoke: initialized, protocolVersion=$PROTOCOL_VERSION" >&2

echo "smoke: POST /v1/mcp notifications/initialized" >&2
mcp_post '{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}' >/dev/null

echo "smoke: POST /v1/mcp tools/call session_create" >&2
SESSION_RESPONSE=$(mcp_post '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"session_create","arguments":{"profile":"default"}}}')
IS_ERROR=$(echo "$SESSION_RESPONSE" | jq -r '.result.isError // false')
SESSION_ID=$(echo "$SESSION_RESPONSE" | jq -r '.result.structuredContent.id // empty')
if [ "$IS_ERROR" = "true" ] || [ -z "$SESSION_ID" ]; then
    echo "smoke: session_create failed: $SESSION_RESPONSE" >&2
    exit 1
fi

echo "smoke: session_create OK, session id = $SESSION_ID" >&2
echo "SESSION_ID=$SESSION_ID"
