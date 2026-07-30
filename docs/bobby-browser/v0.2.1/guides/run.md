---
documentedVersion: 0.2.1
---

# Run the server

```bash
bobby serve
# or:
cargo run -p bobby-browser -- serve
# with an explicit config file:
BOBBY_BROWSER_CONFIG=/path/to/config.toml bobby serve
```

Override the bootstrap secret path with `BOBBY_BROWSER_BOOTSTRAP_ENV` when it is
not at the default OS config location (`…/bobby-browser/bootstrap.env`). Prefer
`bobby init` before first serve on non-loopback binds.

Then open:

- `http://127.0.0.1:7777/healthz` — unauthenticated liveness
- Authenticated routes under `/v1/*` (for example `GET /v1/runtime`) — bearer + interface headers required

There is no `/runtime` route. Use `/v1/runtime`. See [Authentication](auth.md) and the [HTTP API reference](../surfaces/http-api.md).

Do not expose the runtime to untrusted networks; reach it over loopback or an operator-controlled boundary.

To exercise the deterministic skill course locally, build `@bobby-browser/gauntlet` and run the opt-in production championship described in the [browser gauntlet guide](gauntlet.md).
