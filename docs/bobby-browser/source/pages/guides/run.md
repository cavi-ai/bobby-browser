---
documentedVersion: 0.2.0
---

# Run the server

```bash
bobby serve
# or:
cargo run -p cli -- serve
# with an explicit config file:
BOBBY_BROWSER_CONFIG=/path/to/config.toml bobby serve
```

Override the bootstrap secret path with `BOBBY_BROWSER_BOOTSTRAP_ENV` when it is
not at the default OS config location (`…/bobby-browser/bootstrap.env`). Prefer
`bobby init` before first serve on non-loopback binds.

Then open:

- `http://127.0.0.1:7777/healthz`
- `http://127.0.0.1:7777/runtime`

Do not expose the runtime to untrusted networks; reach it over loopback or an operator-controlled boundary.

To exercise the deterministic skill course locally, build `@bobby-browser/gauntlet` and run the opt-in production championship described in the [browser gauntlet guide](gauntlet.md).
