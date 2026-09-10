---
documentedVersion: 0.14.0
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

## Docker

```bash
docker compose up -d --build
bash scripts/docker/smoke.sh
```

Builds a non-root image (`Dockerfile`) running `bobby serve` with managed
headless Chromium and starts it via `docker-compose.yml` (service `bobby`,
named volume `bobby-data` at `/var/lib/bobby`). Two env vars select and
locate the browser engine:

- `BOBBY_CHROME_EXECUTABLE=/usr/bin/chromium`
- `AUTOMATION_RUNTIME_BROWSER_SELECTION={"preference":{"mode":"managedChromium"}}`

`deploy/docker/entrypoint.sh` runs `bobby init` once (only if
`/var/lib/bobby/bootstrap.env` is missing) to generate the bootstrap
credential, without printing the bearer to `docker logs`. Retrieve it with:

```bash
docker compose exec bobby bobby token --stdout
```

Security note: `deploy/docker/config.toml` binds `0.0.0.0` *inside* the
container — Docker's port publishing cannot reach a process bound to the
container's own loopback — but `docker-compose.yml` publishes the port as
`127.0.0.1:7777:7777`, so the runtime stays loopback-only from the host's
perspective. Change that published address only if you intend to expose the
runtime beyond the host.

`scripts/docker/smoke.sh` is the real proof: it builds, waits for
`/healthz`, pulls the bootstrap bearer, and drives one MCP streamable HTTP
session (`initialize` → `session_create`) exactly as an external client
would, then tears the stack down.
