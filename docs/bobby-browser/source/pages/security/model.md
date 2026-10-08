---
documentedVersion: {{PRODUCT_VERSION}}
---

# Security model

bobby drives real browsers on behalf of authenticated callers, so it denies by default. Do not expose the runtime to untrusted networks. Reach it over loopback or a boundary you control.

## Guarantees

- **Fail closed.** Authentication and authorization deny unless a rule allows.
- **Scoped tokens.** Each token carries a set of [capabilities](../concepts/capabilities.md), re-checked on every dispatch, including on long-lived MCP and CDP connections.
- **Bounded issuance.** Only `authority:admin` can mint tokens. Issued tokens are a subset of the issuer's capabilities and expire. The runtime stores digests, never bearers.
- **No secrets in URLs, logs or committed configuration.** Credentials travel in the `Authorization` header only.
- **Opt-in JavaScript and vision.** Each needs a capability, a session `executionPolicy` flag, and for vision a configured provider. Vision tokens live in environment variables named by `token_env`.
- **Per-principal isolation.** Sessions, pages, idempotency keys and quotas belong to one principal. Request, frame and result sizes are bounded.
- **Egress control.** Outbound requests from the runtime deny private and loopback addresses unless `[http]` allows them.

## Deployment checklist

1. Bind `[server]` to loopback unless a trusted network path exists.
2. Create credentials with `bobby init` or a secret manager. Never commit bearers.
3. Give each job or tenant a principal with the least capabilities it needs, and revoke it when done.
4. Keep `[http]` egress settings tight.
5. For vision, use `https` endpoints (or loopback `http`) and rotate `token_env` secrets outside source control.
6. Treat CDP and MCP as equal in trust to HTTP. The same bearer rules apply.

## Related

- [Authentication](../guides/auth.md)
- [Capabilities](../concepts/capabilities.md)
- [Multi-principal runtime](../concepts/multi-principal.md)
- [Prompt injection](prompt-injection.md)
- [Reporting vulnerabilities](reporting.md)
