# Automation Runtime

A browser automation runtime with three control surfaces:

- Native SDK
- MCP server
- CDP compatibility layer

This scaffold implements the first thin vertical slice:

- broker startup
- in-memory session/page state
- typed domain models
- minimal HTTP health endpoint
- MCP/CDP placeholders

## Run

```bash
cargo run -p cli -- serve
```

Then open:

- `http://127.0.0.1:7777/healthz`
- `http://127.0.0.1:7777/runtime`

## Next steps

1. Replace placeholders with real engine implementations.
2. Add MCP stdio and Streamable HTTP.
3. Add CDP discovery and WebSocket routing.
4. Introduce V8-backed `js-engine`.
