---
documentedVersion: {{PRODUCT_VERSION}}
---

# Bobby skills

Skills are packaged instructions and recovery behavior that sit on top of the browser tools. There are two kinds.

## Agent skill

The agent skill teaches an agent host how to drive bobby over MCP. Install it with:

```bash
bobby install --skill
```

| Flag | Installs to |
|---|---|
| `--skill` | `~/.agents/skills/bobby-browser/` (or `.agents/skills/` in the project with `--project-skill`) |
| `--skill-claude` | `~/.claude/skills/` |
| `--skill-openclaw` | `$OPENCLAW_STATE_DIR/skills/`, else `~/.openclaw/skills/` |
| `--skill-hermes` | `$HERMES_HOME/skills/`, else `~/.hermes/skills/` (drives the [Python SDK](../surfaces/python-sdk.md)) |

## Recovery skills: Ghost and ZigZagZig

Ghost and ZigZagZig change how a session prepares and recovers. They run inside the runtime and respect the normal deadlines, policy checks and evidence rules.

The way to use them is a ZigZagZig session. Set `zigzagzig: true` on `POST /v1/sessions`, `session_create` or `workflow_start`. Every policy flag is forced on, so the caller needs `browser:fingerprint` and `browser:humanize`. Each page-bound command in the session then runs under the recovery ladder below.

The skill router also accepts these commands:

- `/ghost on|off|status` negotiates a coherent browser profile before launch, reports the engine and the capabilities it supports, and freezes the profile for the session. Required capabilities fail closed. Optional ones may degrade and stay visible in status. After `off`, a running browser may report `restartRequired` until the next safe launch.
- `/zigzagzig run|status|stop` applies the recovery ladder to the original postcondition.

Ghost reports what the selected engine supports. It does not present one engine as another.

### Recovery ladder

ZigZagZig tries these tactics in order until the postcondition holds:

1. Retry the read-only observation.
2. Resolve the target again.
3. Change the interaction method.
4. Solve a blocking verification challenge in place. Sessions without vision assist skip this step.
5. Reconcile from the verified checkpoint.
6. Start a fresh Ghost session.
7. Choose another compatible engine.
8. Restart from the last durable boundary.

Each tactic spends the workflow's existing deadline and tactic budget. An action whose effect is unknown is never replayed blindly: bobby inspects or reconciles first, and returns `effectUncertain` when it cannot prove the outcome.

### Failures

Skill failures are typed: `unsupportedCapability`, `targetDrift`, `checkpointMismatch`, `strategyExhausted`, `engineUnavailable`, `effectUncertain`. Status and evidence are redacted. They hold profile digests, tactic decisions, checkpoint identity, timing and attempt lineage, never credentials, cookies, auth headers or host paths.
