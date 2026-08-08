---
documentedVersion: 0.7.0
---

# OpenShell host

bobby-browser integrates with [NVIDIA OpenShell](https://github.com/NVIDIA/OpenShell)
as a **host runtime**: the sandboxed agent stays inside OpenShell; bobby and the
Firefox companion stay on the host. OpenShell owns filesystem, process, and
egress policy. bobby owns browser automation, capabilities, and evidence.

## Topology

| Layer | Role |
|---|---|
| OpenShell sandbox | Agent process, skill, MCP client; deny-by-default egress |
| OpenShell policy proxy | Only allowlisted MCP Streamable HTTP to host bobby |
| Host `bobby serve` | MCP at `POST /v1/mcp` + Firefox companion |
| Host operator | Mint/revoke one principal per sandbox (`authority:admin`) |

One OpenShell sandbox ↔ one bobby principal. Default capability floor is the
narrow **openshell** preset (no `authority:admin`, no JS eval / vision / jobs /
fingerprint / humanize). Use `--capabilities-preset agent` only when needed.

## Isolation constraints

- **Shared Firefox companion:** cookies, logins, and the durable context graph
  are **profile-scoped**, not principal-scoped. Two sandboxes on the same host
  companion share site state. For stronger isolation use a dedicated companion
  profile per sandbox, or managed Chromium disposable workers (no persistent
  logins).
- **Cleartext MCP:** default `mcp.json` uses `http://` to the host gateway.
  Firewall that path; do not bind bobby to untrusted networks.
- **Policy replace:** `openshell policy set` replaces the entire sandbox policy.
  Merge the pack’s `network_policies` into an existing policy when you already
  customize filesystem/process sections.

## Install the pack

```bash
bobby install --host openshell --yes
# or:
bobby openshell install
bobby init --emit openshell
```

Writes project `openshell/`:

- `policy.yaml` — OpenShell `protocol: mcp` allowlist (denies
  `evaluate_javascript` / `job_*` at the proxy as defense in depth)
- `mcp.json` — streamable-HTTP client config (`Bearer ${AUTOMATION_RUNTIME_TOKEN}`)
- `skills/bobby-browser/SKILL.md` — agent skill copy
- `README.md` — operator steps

Default gateway host is `host.docker.internal:7777` (Docker Desktop). Override:

```bash
bobby openshell install --mcp-host host.containers.internal --mcp-port 7777 \
  --agent-binary /usr/local/bin/claude
```

## Host prerequisites

1. `bobby init --preset unrestricted` (needed to mint principals)
2. Firefox companion paired (`bobby install --companion`, then Pair) — or accept
   shared-profile risk / use Chromium disposable instead
3. `bobby serve` reachable from the sandbox via the host gateway address
4. Keep bind scoped — loopback plus the gateway interface OpenShell can dial

## Per-sandbox provision

```bash
bobby openshell provision --sandbox demo-1
# revokes any prior principal for demo-1, mints a fresh one (unique idempotency key)
# writes ~/.config/bobby-browser/openshell/demo-1.env (0600)
# inject AUTOMATION_RUNTIME_TOKEN into the OpenShell sandbox credentials
openshell policy set demo-1 --policy openshell/policy.yaml --wait
```

Prefer `BOBBY_MCP_TOOLSET=explore` (or `act`) inside the sandbox so `tools/list`
stays under OpenShell’s MCP body budget.

Wider capabilities when required:

```bash
bobby openshell provision --sandbox demo-1 --capabilities-preset agent
```

When the sandbox ends:

```bash
bobby openshell revoke --sandbox demo-1
```

Re-running `provision` for the same sandbox id rotates: prior principal is
revoked first, then a new principal is minted.

## Doctor

If `openshell/` is present in the working directory, `bobby doctor` reports
`openshell-pack` (and warns when the policy lacks hardened deny_rules).

## Non-goals

- Running Chromium/Firefox *inside* the OpenShell sandbox
- A bobby-side relay control plane (use OpenShell’s supervisor proxy)
- Minting tokens from inside the sandbox

## Related

- [MCP over HTTP](../surfaces/mcp-http.md)
- [Authentication](auth.md)
- [Firefox companion](firefox-companion.md)
- [Multi-principal](../concepts/multi-principal.md)
- [Security model](../security/model.md)
