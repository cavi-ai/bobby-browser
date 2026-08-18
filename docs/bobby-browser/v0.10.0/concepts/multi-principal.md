---
documentedVersion: 0.10.0
---

# Multi-principal runtime

A single bobby-browser instance serves many independent tenants. Each principal has:

- A capability-scoped bearer token
- An independent in-flight request quota (`interface.max_in_flight_per_principal`)
- Server state (runtime binding, MCP lifecycle, idempotency, sessions) scoped to
  that principal

Quota exhaustion returns HTTP **429** with a `Retry-After` header (seconds) and
often `error.retryAfterMs` in the JSON body — see
[HTTP API — Rate limits and retry](../surfaces/http-api.md#rate-limits-and-retry).

Sessions and pages created by principal A are not visible to principal B.
Deleting a session (`DELETE /v1/sessions/{id}` / MCP `session_close`) releases
that principal's worker binding for the session.

Remembered site context (the persisted context graph) is keyed by the durable
browser profile, not by principal: any principal holding `context:read` on a
runtime with a durable-profile engine can read it, and principals without it
are denied on every surface. It contains structure and counters only — never
typed values or page content.

The bootstrap credential holds `authority:admin` only when minted with
`--preset unrestricted` (or a marker-less legacy file healed as unrestricted).
Default `bobby init` is the **agent** floor and cannot mint or revoke principals.
With admin, the bootstrap is the only principal that can mint or revoke other tokens:

- `POST /v1/principals` issues a scoped bearer (returned once in the response body)
- `DELETE /v1/principals/{id}` revokes a principal immediately

Issuance is capability-bounded: issued capabilities must be a subset of the issuer's,
cannot include `authority:admin`, and are TTL-capped (90 days). Only SHA-256 hashes
of issued bearers are persisted.

Full header contract and mint curl: [Authentication](../guides/auth.md).

## Issue request / response

Request (`POST /v1/principals`):

```json
{
  "principalId": "10000000-0000-0000-0000-000000000051",
  "capabilities": ["session:read", "session:write"],
  "expiresAt": "2026-07-28T20:00:00.000Z"
}
```

Response `201`:

```json
{
  "principalId": "10000000-0000-0000-0000-000000000051",
  "capabilities": ["session:read", "session:write"],
  "expiresAt": "2026-07-28T20:00:00.000Z",
  "bearer": "<one-time plaintext>"
}
```

Capture `bearer` immediately. A non-admin caller receives `403`. After
`DELETE /v1/principals/{principalId}` (`204`), the issued bearer yields `401`.

## Operator tips

- Mint least-privilege tokens for each automation job; keep `authority:admin`
  off production worker hosts.
- Rotate by issuing a new principal and revoking the old id.
- MCP HTTP: rotating the bearer resets that principal's MCP initialize state —
  clients must `initialize` again.

## Next

- [Capabilities](capabilities.md)
- [Authentication](../guides/auth.md)
- [Security model](../security/model.md)
