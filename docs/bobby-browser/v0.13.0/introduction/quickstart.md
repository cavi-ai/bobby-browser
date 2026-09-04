---
documentedVersion: 0.13.0
---

# Quickstart

## Install for an agent

```bash
bobby install
bobby doctor
```

`bobby install` creates missing private bootstrap and vision credentials,
installs the agent skill, and configures the selected host. It preserves valid
existing credentials and host configuration.

If you selected Firefox, start the dedicated Bobby profile and click **Pair**
in the companion toolbar popup before running doctor:

```bash
make firefox-start
```

## First agent calls

The agent should call:

1. `workflow_start` with `profile: "default"` and an optional `url`.
2. `workflow_observe` with the returned workflow handle.
3. `click`, `type_text`, `navigate`, or an `intent_*` tool for the next action.

The MCP `start_browsing` prompt teaches the same flow without requiring a
session, page, or workflow ID.

## Vision

Users configure vision; Bobby runs the local vision service on demand:

```bash
bobby vision connect
bobby vision status
# Optional foreground run for debugging:
bobby vision start
```

## Advanced: HTTP and SDK applications

Application developers can run the HTTP runtime explicitly:

```bash
bobby serve
```

- `http://127.0.0.1:7777/healthz` — unauthenticated liveness
- Authenticated HTTP under `/v1/*` — see [Authentication](../guides/auth.md)

```ts
import { BrowserRuntimeClient } from "@cavi-ai/bobby-browser";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});
const info = await client.runtimeInfo();
```

The SDK sets the required interface, correlation, deadline, authorization, and
idempotency headers. Raw HTTP callers must set them explicitly.

Next: [First browser session](first-session.md) · [CLI reference](../guides/cli.md)
