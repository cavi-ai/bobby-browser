---
documentedVersion: 0.2.0
---

# TypeScript SDK

Install workspace packages with pnpm, then use `@bobby-browser/sdk`:

```ts
import { BrowserRuntimeClient } from "@bobby-browser/sdk";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});
const info = await client.runtimeInfo();
```

The HTTP API requires `Authorization`, the current `X-Interface-Version`, and a bounded correlation identifier. Mutating requests additionally require an idempotency key. Duplicate or conflicting security-sensitive headers are rejected; bodies are limited to 1 MiB by default.
