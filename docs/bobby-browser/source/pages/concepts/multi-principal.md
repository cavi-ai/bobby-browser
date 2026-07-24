---
documentedVersion: 0.2.0
---

# Multi-principal runtime

A single bobby-browser instance serves many independent tenants. Each principal has:

- A capability-scoped bearer token
- An independent in-flight request quota (`interface.max_in_flight_per_principal`)
- Server state (runtime binding, MCP lifecycle, idempotency) scoped to that principal

The bootstrap credential holds `authority:admin` and is the only principal that can mint or revoke other tokens:

- `POST /v1/principals` issues a scoped bearer (returned once in the response body)
- `DELETE /v1/principals/{id}` revokes a principal immediately

Issuance is capability-bounded: issued capabilities must be a subset of the issuer's, cannot include `authority:admin`, and are TTL-capped (90 days). Only SHA-256 hashes of issued bearers are persisted.
