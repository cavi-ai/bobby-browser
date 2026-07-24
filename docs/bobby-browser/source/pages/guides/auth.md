---
documentedVersion: 0.2.0
---

# Authentication

The runtime enrolls a SHA-256 digest of a high-entropy bearer credential at startup. Supply the plaintext credential only through a protected process input or secret manager, then send it as `Authorization: Bearer <token>`.

Never put a token in a URL, command argument, config committed to source control, or log.

Multi-principal issuance uses authenticated HTTP:

- `POST /v1/principals` — mint a scoped bearer (once)
- `DELETE /v1/principals/{id}` — revoke immediately

All issuance requests carry `Authorization`, `X-Interface-Version`, a bounded correlation id, a deadline, and an idempotency key for the mutating `POST`.
