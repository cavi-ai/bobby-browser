---
documentedVersion: 0.2.0
---

# Capabilities

Tokens bind one principal to an explicit capability set and expiry. Revocation and expiry are checked again at dispatch, including long-lived MCP and CDP connections.

Typical least-privilege capabilities include:

`session:read`, `session:write`, `page:read`, `page:write`, `browser:mutate`, `file:upload`, `file:download`, `artifact:read`, `artifact:capture`, `recovery:read`, `recovery:write`, `javascript:evaluate`, `intent:execute`, `vision:assist`, and `authority:admin`.

Privileged primitives require their own capability beyond `browser:mutate` (for example file upload requires `file:upload`).
