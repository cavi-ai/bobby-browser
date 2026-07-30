---
documentedVersion: 0.2.1
---

# Security model

bobby-browser executes untrusted actions against real browsers on behalf of authenticated principals. Security is a primary design goal.

This project is in **alpha**. Do not expose the runtime to untrusted networks; it is designed to be reached over loopback or an operator-controlled boundary.

Key properties:

- **Fail closed.** Authentication and authorization deny by default.
- **Capability-scoped tokens.** Re-checked at dispatch, including long-lived connections.
- **Bounded issuance.** Only `authority:admin` mints tokens; hashes are persisted, never bearers.
- **No credentials in URLs, logs, or committed config.**
- **Deny-by-default JavaScript and vision assists** with double gates.
- **Per-principal isolation** and bounded request/frame/result sizes.

Operational setup: [Authentication](../guides/auth.md). Capability matrix:
[Capabilities](../concepts/capabilities.md).

See [SECURITY.md](https://github.com/cavi-ai/bobby-browser/blob/main/SECURITY.md)
in the repository root for the full policy (this page summarizes only).
