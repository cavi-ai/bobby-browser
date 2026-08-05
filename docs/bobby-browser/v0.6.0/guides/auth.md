---
documentedVersion: 0.6.0
---

# Authentication

The runtime enrolls a SHA-256 digest of a high-entropy bearer credential at startup. Supply the plaintext credential only through a protected process input, secret manager, or the local bootstrap file from `bobby init`, then send it as `Authorization: Bearer <token>`.

Never put a token in a URL, command argument, config committed to source control, or log.

## `bobby init`

```bash
bobby init
# optional:
bobby init --ttl-days 30
bobby init --path /secure/path/bootstrap.env
bobby init --force
```

`bobby init` writes a dotenv secret under the OS config directory
(`…/bobby-browser/bootstrap.env`) with the four `AUTOMATION_RUNTIME_BOOTSTRAP_*`
variables, mode `0600` where supported. It prints the plaintext bearer once.

If the secret file already exists, `bobby init` refuses unless you pass `--force`.
Regeneration invalidates the previous bearer for new enrollment; existing
authority-store records keyed to the old bearer will no longer match.

For SDK / HTTP clients, export that same plaintext as `AUTOMATION_RUNTIME_TOKEN`
(conventional client alias — not a serve bootstrap env var).

## Serve resolution order

`bobby serve` resolves bootstrap in this order:

1. Process env (`AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN`,
   `AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL`,
   `AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES`,
   `AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT`)
2. Local secret file (`BOBBY_BROWSER_BOOTSTRAP_ENV` or the default OS config path)
3. Loopback auto-init (bind is `127.0.0.1` / `::1` only) — generates, writes, prints once
4. Error — fail closed; non-loopback binds never auto-generate

Corrupt or unreadable secret files fail closed and name the path. There is no
empty-auth fallback.

## Shared HTTP request headers

Every `/v1/*` request (except MCP streamable HTTP's own rules — see
[MCP over HTTP](../surfaces/mcp-http.md)) must carry:

| Header | Required | Notes |
|---|---|---|
| `Authorization` | yes | `Bearer <token>`; exactly one header; printable ASCII bearer |
| `x-interface-version` | yes | Current value: `2026-07-23` |
| `x-correlation-id` | yes | UUID string; max 64 bytes |
| `x-deadline` | yes | RFC3339 timestamp; must be after now and within 5 minutes ahead |
| `idempotency-key` | mutating POSTs | 1–128 printable ASCII; enables replay/conflict detection |

`GET /healthz` is unauthenticated and does not use these headers.

Duplicate or conflicting security-sensitive headers are rejected. Request bodies
are bounded (1 MiB by default via interface config). Responses echo
`x-interface-version` and `x-correlation-id` when present.

The TypeScript SDK sets these headers automatically. Pass
`options.idempotencyKey` on mutating calls when you need safe retries.

Error bodies are JSON `{ "error": InterfaceError }` with camelCase fields
(`code`, `layer`, `message`, `correlationId`, …). See the
[HTTP API reference](../surfaces/http-api.md) and TypeScript `InterfaceErrorCode`.

## Mint a scoped principal

Requires `authority:admin` on the caller (bootstrap defaults include it).

```bash
DEADLINE=$(date -u -v+2M +"%Y-%m-%dT%H:%M:%S.000Z" 2>/dev/null \
  || date -u -d '+2 minutes' +"%Y-%m-%dT%H:%M:%S.000Z")
CORRELATION=$(uuidgen | tr '[:upper:]' '[:lower:]')
EXPIRES=$(date -u -v+10M +"%Y-%m-%dT%H:%M:%S.000Z" 2>/dev/null \
  || date -u -d '+10 minutes' +"%Y-%m-%dT%H:%M:%S.000Z")
PRINCIPAL=$(uuidgen | tr '[:upper:]' '[:lower:]')

curl -sS -X POST "http://127.0.0.1:7777/v1/principals" \
  -H "Authorization: Bearer ${AUTOMATION_RUNTIME_TOKEN}" \
  -H "x-interface-version: 2026-07-23" \
  -H "x-correlation-id: ${CORRELATION}" \
  -H "x-deadline: ${DEADLINE}" \
  -H "idempotency-key: issue-${PRINCIPAL}" \
  -H "content-type: application/json" \
  -d "{\"principalId\":\"${PRINCIPAL}\",\"capabilities\":[\"session:read\",\"session:write\"],\"expiresAt\":\"${EXPIRES}\"}"
```

Successful response is `201` with a one-time `bearer`, plus `principalId`,
`capabilities`, and `expiresAt`. Capture the bearer immediately; it is not
retrievable again.

Revoke with `DELETE /v1/principals/{principalId}` (same context headers; no body).

## Multi-principal issuance

After bootstrap, multi-principal issuance uses authenticated HTTP:

- `POST /v1/principals` — mint a scoped bearer (once)
- `DELETE /v1/principals/{id}` — revoke immediately

All issuance requests carry `Authorization`, `x-interface-version`, a bounded
correlation id, a deadline, and an idempotency key for the mutating `POST`.

Common auth failures and capability errors: [Troubleshooting](troubleshooting.md).
