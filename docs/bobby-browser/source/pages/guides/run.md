---
documentedVersion: 0.2.0
---

# Run the server

```bash
cargo run -p cli -- serve
# or with an explicit config file:
BOBBY_BROWSER_CONFIG=/path/to/config.toml cargo run -p cli -- serve
```

Then open:

- `http://127.0.0.1:7777/healthz`
- `http://127.0.0.1:7777/runtime`

Do not expose the runtime to untrusted networks; reach it over loopback or an operator-controlled boundary.
