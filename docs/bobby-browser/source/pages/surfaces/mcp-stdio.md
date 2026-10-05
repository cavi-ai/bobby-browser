---
documentedVersion: {{PRODUCT_VERSION}}
---

# MCP stdio

The CLI entrypoint `bobby mcp-stdio` starts or connects to the personal shared
runtime. Add `--team engineering --project checkout` to use an organized local
scope; either flag can be used independently. Use the same flags for `install`,
`firefox-start`, `acp-stdio`, `jobs`, `context`, and `doctor`. Scoped host
installation records them in the agent host's command arguments.

`bobby runtime start`, `status`, `stop`, and `restart` manage the selected scope (`stop` and `restart` ask before disconnecting attached agents and refuse without a terminal unless `--disconnect-agents` is given);
`bobby runtime list` lists local scopes. Agents reuse one owner on an assigned
loopback port. Each connection keeps its own MCP state and opening toolset,
while browser profiles, storage, context, and jobs belong to the owner.
Disconnecting an agent leaves the runtime available. An agent always reaches
the running owner, including after the scope's files change; `bobby runtime
status` reports a pending change and `bobby runtime restart` applies it: it shows the attached connections and sessions and asks before disconnecting them (without a terminal it refuses unless `--disconnect-agents` is given), and saves what was attached to `runtime/restart-snapshots/`. A
gateway binary launched directly with the scope's bootstrap credential
attaches to the scope's owner when one is running. Unreadable records in the
journals or the idempotency ledger never stop the runtime from starting:
journal lines are skipped and counted by `bobby doctor`, and an unreadable
ledger is moved aside. Scope sharing is local to the current OS user.

### How many agents

Each attached agent holds one of the runtime's `interface.max_connections`
slots (default 64) until it closes; in-flight HTTP requests draw on the same
slots. When all are held the next agent is refused:
`the shared runtime refused the connection: resourceExhausted: gateway
connection capacity exhausted (retry after 1000 ms)`. Raise the setting to
attach more. `interface.max_in_flight_per_principal` bounds in-flight HTTP
requests per principal only; agents sharing the bootstrap credential are not
limited by it.

Measured on one owner with managed Chromium, each agent running
`workflow_start`, `intent_complete_form`, and `session_close` against a local
page at the same moment (3 runs, Apple M5 Max). `workflow_start` includes
launching that agent's browser.

| Agents at once | Journeys completed | Requests lost | `workflow_start` p95 | `intent_complete_form` p95 | `session_close` p95 |
|---|---|---|---|---|---|
| 1 | 1/1 | 0 | 379–455 ms | 111–121 ms | 21–23 ms |
| 4 | 4/4 | 0 | 847–898 ms | 218–227 ms | 28–33 ms |
| 8 | 8/8 | 0 | 1,638–1,731 ms | 391–398 ms | 24–41 ms |

Reproduce with
`cargo test -p bobby-browser --test shared_runtime_load -- --ignored --nocapture`
(`BOBBY_CHROME_EXECUTABLE` names the browser).

`mcp-gateway` is a single-process MCP server over stdio. It enrolls a startup
bootstrap credential from environment variables, then speaks MCP protocol
version `2025-11-25` on stdin/stdout. Stdout is reserved for newline-delimited
JSON-RPC; diagnostics go to stderr.

## Build

```bash
cargo build -p mcp-gateway --release
# binary: ./target/release/mcp-gateway
```

## Startup credential

At process start the gateway requires all four bootstrap variables (same
contract as `bobby serve` / `bobby init`):

| Variable | Purpose |
|---|---|
| `AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN` | High-entropy plaintext bearer (32–505 printable ASCII bytes) |
| `AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL` | Principal UUID |
| `AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES` | Comma-separated capability wire strings |
| `AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT` | RFC3339 expiry |

Generate them with `bobby init` (writes `…/bobby-browser/bootstrap.env`), then
either export the file into the environment or point your MCP client `env` at
those keys. Missing or invalid startup input fails closed.

For agent hosts, default `bobby init` (and install / loopback auto-init) mints
the **agent** preset: no `authority:admin`, marker
`# bobby-bootstrap-preset: agent`, heal never widens past that floor. Operators
who need to mint principals use `bobby init --preset unrestricted`. Host floors
are narrower: `--preset claude` or `--preset codex` (no JavaScript evaluation,
fingerprint, or humanize) and `--preset openshell`; the
[preset matrix](../concepts/capabilities.md#generated-preset-matrix) lists what
each one withholds. Marker-less existing files still heal as unrestricted
(back-compat).

There is no single `AUTOMATION_RUNTIME_TOKEN` env var for stdio startup. That
name is only a conventional alias for the **client** bearer when talking to the
HTTP runtime / TypeScript SDK.

## Client config example

The easy path is the installer — it writes the bootstrap credential, merges
the server entry into your host's config, installs the agent skill into
`~/.agents/skills/bobby-browser/` (project: `.agents/skills/` with
`--project-skill`; optional `--skill-claude` / `--skill-openclaw` /
`--skill-hermes`), and can
install the Firefox companion (extension + native host; pairing finishes via
toolbar **Pair**, or `bobby enroll-firefox-profile` for CI):

```bash
bobby install                    # interactive checklist
bobby install --host claude --skill --yes   # non-interactive
# or: make install               # builds bobby + the gateway, then runs the installer
```

The merged entry points at `bobby mcp-stdio`, which loads the credential
from `bootstrap.env` itself — the host config carries no secrets and no env
wiring:

```json
{
  "mcpServers": {
    "bobby-browser": {
      "command": "/absolute/path/to/bobby",
      "args": ["mcp-stdio"]
    }
  }
}
```

`bobby init --emit <claude|zed|vscode|json>` remains for hosts that prefer
the raw `mcp-gateway` binary with `${VAR}` placeholders; that form requires
exporting the four bootstrap variables into the host's environment.

`bobby doctor` runs a live handshake against the gateway (`initialize` +
`tools/list`) and reports the tool count and byte size against the 128 KiB
catalog budget, so a dead or oversized surface is caught before the agent
sees it.

## Limits and lifecycle

- Frames limited to 1 MiB; tool input to 256 KiB; event reads to 256 records
- Call `initialize` before tools
- Cancellation, EOF, expiry, and revocation close or reject work without leaking credentials

Tool catalog: [MCP tools](mcp-tools.md). Multi-tenant HTTP alternative: [MCP over HTTP](mcp-http.md).
