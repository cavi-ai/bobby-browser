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

One OpenShell sandbox ↔ one bobby principal (agent capability floor — no
`authority:admin` in the sandbox).

## Install the pack

```bash
bobby install --host openshell --yes
# or:
bobby openshell install
bobby init --emit openshell
```

Writes project `openshell/`:

- `policy.yaml` — OpenShell `protocol: mcp` allowlist sample
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
2. Firefox companion paired (`bobby install --companion`, then Pair)
3. `bobby serve` reachable from the sandbox via the host gateway address
4. Keep bind scoped — loopback plus the gateway interface OpenShell can dial

## Per-sandbox provision

```bash
bobby openshell provision --sandbox demo-1
# writes ~/.config/bobby-browser/openshell/demo-1.env (0600)
# inject AUTOMATION_RUNTIME_TOKEN into the OpenShell sandbox credentials
openshell policy set demo-1 --policy openshell/policy.yaml --wait
```

When the sandbox ends:

```bash
bobby openshell revoke --sandbox demo-1
```

## Doctor

If `openshell/` is present in the working directory, `bobby doctor` reports
`openshell-pack`.

## Non-goals

- Running Chromium/Firefox *inside* the OpenShell sandbox
- A bobby-side relay control plane (use OpenShell’s supervisor proxy)
- Minting tokens from inside the sandbox

## Related

- [MCP over HTTP](../surfaces/mcp-http.md)
- [Authentication](auth.md)
- [Firefox companion](firefox-companion.md)
- [Security model](../security/model.md)
