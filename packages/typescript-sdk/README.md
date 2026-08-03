# @cavi-ai/bobby-browser

Typed HTTP client for a Bobby Browser runtime (`bobby serve`) speaking the
authenticated `/v1` interface.

```bash
npm install @cavi-ai/bobby-browser
```

```ts
import { BrowserRuntimeClient } from "@cavi-ai/bobby-browser";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});

const info = await client.runtimeInfo();
console.log(info.version, info.capabilities);
```

Every request sends `Authorization`, `x-interface-version`,
`x-correlation-id`, and `x-deadline`. The Rust crate `bobby-browser-client`
exposes the same surface.

- Documentation: [cavi-ai.xyz/docs/bobby-browser](https://cavi-ai.xyz/docs/bobby-browser)
- Source: [github.com/cavi-ai/bobby-browser](https://github.com/cavi-ai/bobby-browser)
