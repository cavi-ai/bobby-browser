---
documentedVersion: 0.2.1
---

# Multi-principal runtime

A single bobby-browser instance serves many independent tenants. Each principal has:

- A capability-scoped bearer token
- An independent in-flight request quota (`interface.max_in_flight_per_principal`)
- Server state (runtime binding, MCP lifecycle, idempotency) scoped to that principal

The bootstrap credential typically holds `authority:admin` (default `bobby init`
capability set) and is the only principal that can mint or revoke other tokens:

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
