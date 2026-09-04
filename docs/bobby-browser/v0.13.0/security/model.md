---
documentedVersion: 0.13.0
---

# Security model

bobby-browser executes untrusted actions against real browsers on behalf of
authenticated principals. Security is a primary design goal.

This project is in **alpha**. Do not expose the runtime to untrusted networks;
it is designed to be reached over loopback or an operator-controlled boundary.

## Properties

- **Fail closed.** Authentication and authorization deny by default.
- **Capability-scoped tokens.** Re-checked at dispatch, including long-lived
  MCP and CDP connections.
- **Bounded issuance.** Only `authority:admin` mints tokens; hashes are
  persisted, never bearers.
- **No credentials in URLs, logs, or committed config.**
- **Deny-by-default JavaScript and vision assists** with gates
  (capability + session `executionPolicy`, and for vision a configured
  `[vision]` provider). Vision bearers stay in env vars (`token_env`), never
  in committed config.
- **Per-principal isolation** and bounded request/frame/result sizes.

## Deployment checklist

1. Bind `[server]` to loopback unless an explicit trusted network path exists.
2. Use `bobby init` / secret manager for bootstrap; never commit bearers.
3. Mint job principals with least privilege; revoke when done.
4. Keep outbound `[http]` allowlists tight (defaults deny private/loopback egress).
5. If enabling vision, use https (or loopback http) endpoints and rotate
   `token_env` secrets outside git.
6. Treat CDP and MCP as equal trust surfaces to HTTP — same bearer rules.

Operational setup: [Authentication](../guides/auth.md). Capability matrix:
[Capabilities](../concepts/capabilities.md). Multi-tenant model:
[Multi-principal](../concepts/multi-principal.md).

See [SECURITY.md](https://github.com/cavi-ai/bobby-browser/blob/main/SECURITY.md)
in the repository root for the full policy (this page summarizes only).
Reporting: [Reporting vulnerabilities](reporting.md).
