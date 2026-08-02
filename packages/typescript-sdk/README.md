# @cavi-ai/bobby-browser

Typed client for the [Bobby Browser](https://github.com/cavi-ai/bobby-browser)
runtime: a browser automation runtime with authenticated, capability-scoped
control surfaces. Every call carries a bearer token, a deadline, and a
correlation id, and authentication fails closed.

```bash
npm install @cavi-ai/bobby-browser
```

Requires Node 22+ and a running runtime (`bobby serve`).

## Quick start

```ts
import { BrowserRuntimeClient } from "@cavi-ai/bobby-browser";
import { randomUUID } from "node:crypto";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});

const session = await client.createSession(
  { profile: "default", proxy: null },
  { idempotencyKey: randomUUID() },
);
const page = await client.openPage(
  { session_id: session.id },
  { idempotencyKey: randomUUID() },
);

const outcome = await client.submit(
  {
    schemaVersion: 2,
    commandId: randomUUID(),
    workflowId: randomUUID(),
    attemptId: randomUUID(),
    sessionId: session.id,
    pageId: page.id,
    deadline: new Date(Date.now() + 60_000).toISOString(),
    command: {
      kind: "primitive",
      input: {
        kind: "navigate",
        input: { url: "https://example.com", waitUntil: "domContentLoaded", timeoutMs: 30_000 },
      },
    },
  },
  { idempotencyKey: randomUUID() },
);

console.log(outcome.status);
await client.deleteSession(session.id);
```

## Intents

Intent helpers describe a goal instead of a selector. The runtime resolves the
target, acts, and returns verification evidence; an action without evidence
fails closed.

```ts
import { fillEnvelope } from "@cavi-ai/bobby-browser";

const meta = {
  commandId: randomUUID(),
  workflowId: randomUUID(),
  attemptId: randomUUID(),
  sessionId: session.id,
  pageId: page.id,
  deadline: new Date(Date.now() + 60_000).toISOString(),
};

await client.submit(
  fillEnvelope(
    meta,
    "enter the applicant email",
    { kind: "text", text: "a@example.com", clearFirst: true },
    { role: "textbox", nearText: { kind: "exact", value: "Email address" } },
  ),
  { idempotencyKey: randomUUID() },
);
```

Envelope helpers: `locateEnvelope`, `fillEnvelope`, `submitAndVerifyEnvelope`,
`waitForStateEnvelope`, `followEnvelope`, `dismissObstructionEnvelope`,
`extractEnvelope`, and `intentEnvelope` for multi-field forms built with
`completeFormRuntimeCommand`.

`intentHintsFromAccessibilityTarget` turns a target from an accessibility
snapshot into hints, carrying the ordinal that disambiguates repeated
role/name pairs.

## Client surface

| Method | Purpose |
|---|---|
| `runtimeInfo` | Runtime capability and health information |
| `createSession` / `listSessions` / `deleteSession` | Session lifecycle |
| `openPage` | Open a page in an owned session |
| `formSnapshot` | Read a page's form controls without mutating it |
| `submit` | Submit a `CommandEnvelope`, primitive or intent |
| `checkpoint` / `recoveryStatus` / `recover` | Workflow checkpoint and recovery |
| `events` | Async-iterate retained events from a cursor |
| `artifact` | Stream a digest-verified artifact |

Every method accepts an `idempotencyKey`, so a retried call replays the
retained result instead of acting twice.

## Errors and redaction

Failures throw typed errors carrying the interface error code, whether the call
is retryable, and any capability the principal was missing. The bearer token is
stripped from error text, and the client redacts itself when inspected.

## Capabilities

The runtime advertises only what the bearer holds. Intents need
`intent:execute`. Uploads, downloads, and JavaScript evaluation each need their
own capability, and JavaScript evaluation is additionally gated by session
execution policy.

## Links

- [Documentation](https://cavi-ai.xyz/docs/bobby-browser)
- [TypeScript SDK reference](https://github.com/cavi-ai/bobby-browser/blob/main/docs/bobby-browser/source/pages/surfaces/typescript-sdk.md)
- [Intent commands](https://github.com/cavi-ai/bobby-browser/blob/main/docs/bobby-browser/source/pages/guides/intents.md)
- [Source](https://github.com/cavi-ai/bobby-browser)
- [Changelog](https://github.com/cavi-ai/bobby-browser/blob/main/CHANGELOG.md)
- [Security policy](https://github.com/cavi-ai/bobby-browser/blob/main/SECURITY.md)

MIT licensed.
