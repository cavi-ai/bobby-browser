# bobby-browser on OpenShell

Host runs bobby + Firefox companion. The sandbox agent reaches bobby only
through OpenShell's policy proxy (this pack's `policy.yaml`).

## Isolation (read this)

- One OpenShell sandbox ↔ one bobby principal (narrow openshell capability floor by default).
- Sandbox never holds `authority:admin`.
- **Shared Firefox companion is a hard constraint:** cookies, logins, and the
  durable context graph are profile-scoped, not principal-scoped. Two sandboxes
  on the same host companion share site state. For stronger isolation use a
  dedicated companion profile per sandbox, or managed Chromium disposable
  workers (no persistent logins). `bobby doctor` warns when ≥2 local sandboxes
  share one enrolled companion.
- MCP URL defaults to cleartext HTTP across the host gateway — firewall that
  path; do not bind bobby to untrusted networks. Prefer HTTPS or loopback-only
  bind when possible.

## Host (once)

1. `bobby init --preset unrestricted` (mint principals)
2. Pair Firefox: `bobby install --companion`, then Pair in the toolbar
3. `bobby serve` (MCP HTTP at `http://host.docker.internal:7777/v1/mcp`)
4. Bind so the sandbox can reach the host gateway (`host.docker.internal` on
   Docker Desktop, `host.containers.internal` on Podman, or the LAN IP).

## Per sandbox

1. Copy this `openshell/` pack into the sandbox image or sync it in.
2. `bobby openshell provision --sandbox <id>` on the host — revokes any prior
   principal for that id, mints a fresh one (unique idempotency key), writes a
   0600 injection env under the OS config dir; pass that bearer into OpenShell
   credential injection as `AUTOMATION_RUNTIME_TOKEN` (never bake it into the image).
   Use `--capabilities-preset agent` only when you need JS/vision/jobs.
3. Apply policy: greenfield → `openshell policy set <sandbox> --policy policy.yaml --wait`
   (full replace). Existing FS/process customizations → merge
   `policy-network.yaml` into your policy, then `policy set`.
4. Point the agent MCP client at `mcp.json`; set `BOBBY_MCP_TOOLSET=explore`.
5. When the sandbox dies: `bobby openshell revoke --sandbox <id>`
