---
documentedVersion: {{PRODUCT_VERSION}}
---

# Overview

bobby-browser is a browser automation runtime for AI agents and applications. It drives a real browser (Firefox by default, Chromium when selected) and exposes it through MCP tools, an HTTP API, SDKs for TypeScript, Python and Rust, and an authenticated CDP endpoint.

Every surface runs the same command pipeline. Each call is authenticated, checked against the caller's capabilities, journaled with evidence, and recoverable after a crash. One runtime serves many callers, each with its own scoped bearer token.

Interface version: `{{INTERFACE_VERSION}}`.

## Choose a path

| You are | Start with |
|---|---|
| Connecting an agent host (Claude Code, VS Code, Zed, any MCP client) | [Installation](installation.md), then [Quickstart](quickstart.md) |
| Writing an application in TypeScript, Python or Rust | [First session from code](first-session.md), then the [TypeScript](../surfaces/typescript-sdk.md), [Python](../surfaces/python-sdk.md) or [Rust](../rust/index.md) SDK page |
| Calling HTTP directly | [HTTP API](../surfaces/http-api.md) and [Authentication](../guides/auth.md) |
| Driving Playwright or Puppeteer scripts | [Authenticated CDP](../surfaces/cdp.md) |
| Running agents in an NVIDIA OpenShell sandbox | [OpenShell host](../guides/openshell.md) |
| Operating a deployment | [CLI reference](../guides/cli.md), [Configuration](../guides/configuration.md), [Security model](../security/model.md) |

## What you get

- **Task-level tools.** Intent tools such as `intent_complete_form` and `intent_submit_and_verify` find controls by role and name, act, and verify the result. See [Intent commands](../guides/intents.md).
- **Evidence.** Each command returns structured evidence you can inspect, checkpoint and export. See [Evidence and checkpoints](../concepts/evidence-checkpoints.md).
- **Recovery.** Calls with side effects carry idempotency keys, and workflows resume from verified checkpoints. See [Events and recovery](../guides/events-recovery.md).
- **Scoped access.** Capabilities limit what each caller can do. See [Capabilities](../concepts/capabilities.md).
