---
documentedVersion: {{PRODUCT_VERSION}}
---

# Authentication

Every caller authenticates with a bearer token and is limited to the capabilities its principal holds. The runtime stores only a SHA-256 digest of the token. Keep the plaintext in a secret manager, a protected environment variable or the local credential file. Never put a token in a URL, a command argument, committed configuration or a log.

## Get a token

`bobby install` creates a credential for you. To create or rotate one by hand:

```bash
bobby init
bobby init --ttl-days 7 --path /secure/path/bootstrap.env
bobby init --force
```

`bobby init` writes `bootstrap.env` in the config directory (mode 0600) with four `AUTOMATION_RUNTIME_BOOTSTRAP_*` variables and prints the bearer once. It refuses to overwrite an existing file without `--force`, and `--force` invalidates the old bearer.

SDKs and curl read the bearer from `AUTOMATION_RUNTIME_TOKEN`. Print it any time with `bobby token`:

```bash
export AUTOMATION_RUNTIME_TOKEN="$(bobby token)"
```

`bobby token` reads the same file `bobby serve` does and refuses to write to a redirected stdout unless you pass `--stdout`.

`--preset` selects the capability floor: `agent` (default, everything except `authority:admin`), `unrestricted`, `claude`, `codex` or `openshell`. See [Capabilities](../concepts/capabilities.md).

## How the server finds its credential

`bobby serve` looks for the credential in this order:

1. The environment: `AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN`, `_PRINCIPAL`, `_CAPABILITIES`, `_EXPIRES_AT`.
2. The credential file named by `--bootstrap-env` or `BOBBY_BROWSER_BOOTSTRAP_ENV`, else the default in the config directory.
3. On a loopback bind only, a new credential, written and printed once.
4. Otherwise it fails. There is no unauthenticated fallback, and a corrupt or unreadable file fails closed and names the path.

## HTTP headers

Every `/v1/*` request except `/v1/mcp` carries these headers:

| Header | Required | Notes |
|---|---|---|
| `Authorization` | yes | `Bearer <token>`. Exactly one header |
| `x-interface-version` | yes | `{{INTERFACE_VERSION}}` |
| `x-correlation-id` | yes | UUID, up to 64 bytes. Echoed in the response |
| `x-deadline` | yes | RFC 3339 time, later than now and within 5 minutes |
| `idempotency-key` | mutating POSTs | 1 to 128 printable ASCII characters |

`GET /healthz` needs none of them. `/v1/mcp` takes only the bearer; see [MCP over HTTP](../surfaces/mcp-http.md). Duplicate or conflicting security headers are rejected. Request bodies are limited to 1 MiB by default (`interface.max_request_bytes`). The SDKs set all of these headers for you.

Errors are JSON `{"error": {...}}` with `code`, `message` and `correlationId`. See [HTTP API](../surfaces/http-api.md#errors).

## Issue a scoped token

A caller with `authority:admin` (create the credential with `--preset unrestricted`) can mint tokens with narrower capabilities, for example one per tenant or sandbox.

```bash
PRINCIPAL=$(uuidgen | tr 'A-Z' 'a-z')
curl -sS -X POST http://127.0.0.1:7777/v1/principals \
  -H "Authorization: Bearer ${AUTOMATION_RUNTIME_TOKEN}" \
  -H "x-interface-version: {{INTERFACE_VERSION}}" \
  -H "x-correlation-id: $(uuidgen | tr 'A-Z' 'a-z')" \
  -H "x-deadline: $(date -u -v+2M +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '+2 minutes' +%Y-%m-%dT%H:%M:%SZ)" \
  -H "idempotency-key: issue-${PRINCIPAL}" \
  -H "content-type: application/json" \
  -d "{\"principalId\":\"${PRINCIPAL}\",\"capabilities\":[\"session:read\",\"session:write\"],\"expiresAt\":\"$(date -u -v+1H +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '+1 hour' +%Y-%m-%dT%H:%M:%SZ)\"}"
```

The response is `201` with `principalId`, `capabilities`, `expiresAt` and a one-time `bearer`. Save the bearer now; it cannot be read again. Revoke with `DELETE /v1/principals/{principalId}` using the same headers, which returns `204`. See [Multi-principal runtime](../concepts/multi-principal.md).

For auth failures and capability errors, see [Troubleshooting](troubleshooting.md).
