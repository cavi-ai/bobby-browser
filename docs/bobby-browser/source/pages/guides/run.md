---
documentedVersion: {{PRODUCT_VERSION}}
---

# Run the server

`bobby serve` runs the HTTP API and the MCP-over-HTTP endpoint. Local agent hosts do not need it, because `bobby mcp-stdio` starts a shared runtime on demand. Run the server when applications, SDKs or remote clients connect by URL.

```bash
bobby serve
bobby serve --config /etc/bobby/config.toml --bootstrap-env /etc/bobby/bootstrap.env
```

| Flag | Environment variable | Meaning |
|---|---|---|
| `--config <path>` | `BOBBY_BROWSER_CONFIG` | `config.toml` to load |
| `--bootstrap-env <path>` | `BOBBY_BROWSER_BOOTSTRAP_ENV` | Credential file |
| `--vision`, `--no-vision` | | Start or skip the managed vision proxy |

Check it:

```bash
curl http://127.0.0.1:7777/healthz
```

Authenticated routes live under `/v1`. See [Authentication](auth.md) and the [HTTP API](../surfaces/http-api.md). On a non-loopback bind, create the credential first with `bobby init`. `bobby doctor` checks a running server's health.

Do not expose the runtime to untrusted networks. Keep it on loopback or behind a boundary you control.

## Deployment profiles

`bobby profiles --json` prints the profile contracts. Validate your setup against one with `bobby doctor --profile <name>`.

| Profile | Transport | Bind | Browser | Storage | Start command |
|---|---|---|---|---|---|
| `desktop` | stdio | loopback | Firefox selection | durable local | `bobby mcp-stdio` |
| `headless-ci` | HTTP | isolated runtime | headless | ephemeral or mounted | `bobby serve` |
| `openshell` | streamable HTTP | loopback | host managed | host durable | `bobby openshell install` |
| `remote` | HTTP | operator controlled | remote managed | operator managed | `bobby serve --config <path>` |

Every profile needs a bootstrap credential. `bobby doctor --fix` updates stale bobby-owned host entries and leaves other host configuration alone.

## Jobs

With the server running, submit and inspect built-in jobs:

```bash
bobby jobs submit --name echo --payload '{"message":"hi"}'
bobby jobs status <job_id>
```

The credential needs `job:submit`, `job:read` and `job:cancel`. If an older `bootstrap.env` lacks them, run `bobby init --force`.

## Docker

The repository includes a `Dockerfile` and `docker-compose.yml` for a non-root image running `bobby serve` with headless Chromium.

```bash
docker compose up -d --build
docker compose exec bobby bobby token --stdout
```

The compose file publishes `127.0.0.1:7777`, so the runtime is reachable only from the host. Change that address only if you mean to expose it. The container needs `BOBBY_CHROME_EXECUTABLE=/usr/bin/chromium` and `AUTOMATION_RUNTIME_BROWSER_SELECTION={"preference":{"mode":"managedChromium"}}`, both set in the compose file. On first start it creates a bootstrap credential without printing it to the logs. Data lives in the `bobby-data` volume at `/var/lib/bobby`.
