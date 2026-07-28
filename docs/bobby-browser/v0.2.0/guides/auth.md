---
documentedVersion: 0.2.0
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

## Multi-principal issuance

After bootstrap, multi-principal issuance uses authenticated HTTP:

- `POST /v1/principals` — mint a scoped bearer (once)
- `DELETE /v1/principals/{id}` — revoke immediately

All issuance requests carry `Authorization`, `X-Interface-Version`, a bounded correlation id, a deadline, and an idempotency key for the mutating `POST`.
