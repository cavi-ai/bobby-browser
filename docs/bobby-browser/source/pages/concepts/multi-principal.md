---
documentedVersion: {{PRODUCT_VERSION}}
---

# Multi-principal runtime

One bobby instance serves many independent callers. Each caller is a principal with:

- A bearer token limited to a set of [capabilities](capabilities.md).
- Its own in-flight request quota (`interface.max_in_flight_per_principal`).
- Its own sessions, pages, idempotency keys and MCP state.

A principal cannot see another principal's sessions or pages. Closing a session releases that principal's browser worker. When a principal exceeds its quota, the API returns HTTP 429 with `Retry-After`. See [HTTP API](../surfaces/http-api.md#rate-limits).

Remembered site context is keyed by the browser profile, not the principal. Any principal with `context:read` on a runtime with a durable profile can read it, and principals without that capability are denied on every surface. It holds structure and counters, never typed values or page content. See [Context graph](context-graph.md).

## Issue and revoke principals

The credential from `bobby init --preset unrestricted` holds `authority:admin` and is the only kind that can mint principals. The default `agent` preset cannot.

- `POST /v1/principals` issues a scoped bearer, returned once.
- `DELETE /v1/principals/{id}` revokes it immediately.

Issued capabilities must be a subset of the issuer's, cannot include `authority:admin`, and expire within 90 days. Only SHA-256 digests of issued tokens are stored.

```json
{
  "principalId": "10000000-0000-0000-0000-000000000051",
  "capabilities": ["session:read", "session:write"],
  "expiresAt": "2027-01-01T00:00:00.000Z"
}
```

The `201` response repeats these fields and adds `bearer`. A caller without `authority:admin` gets `403`. After revocation (`204`), the token gets `401`. The full request is in [Authentication](../guides/auth.md#issue-a-scoped-token).

## Practice

- Issue the narrowest capabilities each job needs, and keep `authority:admin` off worker hosts.
- Rotate by issuing a new principal and revoking the old one.
- Over MCP HTTP, rotating a bearer resets that principal's MCP state, so clients send `initialize` again.

## Next

- [Capabilities](capabilities.md)
- [Authentication](../guides/auth.md)
- [Security model](../security/model.md)
