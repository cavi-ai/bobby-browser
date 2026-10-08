---
documentedVersion: {{PRODUCT_VERSION}}
---

# OpenShell host

Run agents inside an [NVIDIA OpenShell](https://github.com/NVIDIA/OpenShell) sandbox while bobby and the browser stay on the host. OpenShell controls the sandbox's filesystem, processes and network egress. bobby controls browser automation, capabilities and evidence. Each sandbox gets its own bobby principal and talks to the host over [MCP over HTTP](../surfaces/mcp-http.md).

| Layer | Role |
|---|---|
| OpenShell sandbox | Agent process, skill and MCP client, with egress denied by default |
| OpenShell policy proxy | Allows only MCP traffic to the host |
| Host `bobby serve` | MCP at `POST /v1/mcp` and the browser |
| Host operator | Mints and revokes one principal per sandbox (`authority:admin`) |

## Set up

1. Create an admin credential and pair the browser.

```bash
bobby init --preset unrestricted
bobby install --companion
bobby serve
```

2. Write the pack into your project.

```bash
bobby install --host openshell --yes
```

This creates `openshell/` with `policy.yaml` (full sample policy), `policy-network.yaml` (a fragment to merge into an existing policy), `mcp.json` (client config), the agent skill and a README. The policy denies `evaluate_javascript` and `job_*` at the proxy as a second layer.

The pack allowlists the Codex binary by default. Choose the agent or a custom path:

```bash
bobby openshell install --agent claude
bobby openshell install --agent-binary /opt/agents/custom
bobby openshell install --mcp-host host.containers.internal --mcp-port 7777
```

The default gateway host is `host.docker.internal:7777`.

3. Provision a principal for each sandbox.

```bash
bobby openshell provision --sandbox demo-1
```

This revokes any earlier principal for `demo-1`, mints a new one, and writes its environment file with mode 0600 under `<os-config-dir>/bobby-browser/openshell/`. Inject `AUTOMATION_RUNTIME_TOKEN` from that file into the sandbox credentials, then apply the policy:

```bash
openshell policy set demo-1 --policy openshell/policy.yaml --wait
```

`openshell policy set` replaces the whole policy. To keep an existing policy, merge `policy-network.yaml` into it first.

Set `BOBBY_MCP_TOOLSET=explore` in the sandbox so `tools/list` stays within OpenShell's MCP body budget.

## Capabilities

The default `openshell` preset allows browsing, intents, files, evidence and recovery. It excludes `authority:admin`, JavaScript evaluation, vision, jobs, fingerprint and humanize. Use `--capabilities-preset agent` for the full agent set without admin. Principals expire after 12 hours; change that with `--ttl-hours`.

## Manage sandboxes

| Command | Effect |
|---|---|
| `bobby openshell provision --sandbox <id>` | Mint a principal. Rerunning rotates it |
| `bobby openshell rotate --sandbox <id>` | Same as `provision` |
| `bobby openshell list` | List sandboxes recorded locally. Prints no secrets |
| `bobby openshell status --sandbox <id>` | Non-secret status for one sandbox |
| `bobby openshell revoke --sandbox <id>` | Revoke the principal |

A sandbox id is 1 to 128 characters of `A-Z a-z 0-9 _ -`. Override the secrets directory with `BOBBY_OPENSHELL_SECRETS_DIR`.

## Isolation limits

- Sandboxes that share one Firefox companion share cookies, logins and the context graph, because these belong to the browser profile, not the principal. For stronger isolation, use a separate companion profile per sandbox, or managed Chromium, which uses disposable workers with no persistent logins.
- The default `mcp.json` uses plain HTTP. Keep that path on loopback or a firewalled interface.

## Doctor

When `openshell/` is in the working directory, `bobby doctor` checks `openshell-pack`, `openshell-admin`, `openshell-companion` (two or more sandboxes on one companion), `openshell-mcp-url`, `openshell-cleartext` (non-loopback plain HTTP) and `openshell-sandboxes`.

## Related

- [MCP over HTTP](../surfaces/mcp-http.md)
- [Authentication](auth.md)
- [Firefox companion](firefox-companion.md)
- [Security model](../security/model.md)
