---
documentedVersion: {{PRODUCT_VERSION}}
---

# Quickstart

Connect an agent host to bobby and run a first browsing workflow. This takes a few minutes after [installation](installation.md).

## 1. Pair Firefox

Skip this step if you use Chromium.

```bash
bobby install --companion
bobby firefox-start
```

Click **Bobby Companion** in the Firefox toolbar, then **Pair**. See [Firefox companion](../guides/firefox-companion.md).

## 2. Check the setup

```bash
bobby doctor
```

A healthy report has no `fail` lines. When something fails, the report starts with the repair command, usually `bobby doctor --fix`.

Restart your agent host so it loads the new MCP server.

## 3. Run a workflow

Ask the agent to open a page. It makes these MCP calls:

1. `workflow_start` with `{"profile": "default", "url": "https://example.com"}` creates a session, a page and a workflow, and returns a `workflowHandle`.
2. `workflow_observe` with `{"workflowHandle": "<handle>"}` returns the page's accessibility tree with a target for each actionable element.
3. `click`, `type_text`, `navigate`, or an `intent_*` tool acts on a target. Pass `workflowHandle` on each call.
4. `session_close` releases the browser when the work is done.

The `start_browsing` MCP prompt teaches an agent this loop. Tool parameters are in [MCP tools](../surfaces/mcp-tools.md).

## Next

- [First session from code](first-session.md) for the HTTP API and SDKs
- [Intent commands](../guides/intents.md) for forms and multi-step actions
- [MCP stdio](../surfaces/mcp-stdio.md) for host configuration details
