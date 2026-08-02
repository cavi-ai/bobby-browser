---
documentedVersion: 0.3.1
---

# Run the server

```bash
bobby serve
bobby serve --config /path/to/config.toml
bobby serve --config ./config.toml --bootstrap-env ./bootstrap.env
# from source:
cargo run -p bobby-browser -- serve --config ./config.toml
```

Environment equivalents: `BOBBY_BROWSER_CONFIG`, `BOBBY_BROWSER_BOOTSTRAP_ENV`.
Prefer `bobby init` before first serve on non-loopback binds. Full flag list:
[CLI reference](cli.md).

Then open:

- `http://127.0.0.1:7777/healthz` — unauthenticated liveness
- Authenticated routes under `/v1/*` (for example `GET /v1/runtime`) — bearer +
  interface headers required

There is no `/runtime` route. Use `/v1/runtime`. See [Authentication](auth.md)
and the [HTTP API reference](../surfaces/http-api.md).

`bobby doctor` can probe `/healthz` after the server is up.

With the server running, submit and inspect jobs via the broker HTTP API:

```bash
bobby jobs submit --name echo --payload '{"message":"hi"}'
bobby jobs status <job_id>
```

Bootstrap needs `job:*` capabilities (`bobby init --force` if an older
`bootstrap.env` lacks them). See [CLI reference](cli.md).

Do not expose the runtime to untrusted networks; reach it over loopback or an
operator-controlled boundary.
