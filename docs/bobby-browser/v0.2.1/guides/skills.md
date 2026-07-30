---
documentedVersion: 0.2.1
---

# Bobby skills

Bobby skills (SkillGhost, SkillZigZagZig, and related recovery tactics) shape
browser preparation and recovery **inside** the skill runtime and live
championship / gauntlet paths. They do **not** bypass the normal command
lifecycle, policy checks, deadlines, or evidence rules.

## Not a public API today

Skills are **not** exposed on:

- HTTP (`/v1/*`)
- MCP tools (`command_execute` and friends)
- `@bobby-browser/sdk`

Do not treat skill router aliases as public user commands for application
integrations. Those aliases exist in the in-process skill router
(`crates/skill-runtime`) for runtime / gauntlet tests, not as broker routes.

Public clients automate with primitives and intents via
[HTTP](../surfaces/http-api.md), [MCP tools](../surfaces/mcp-tools.md), or the
[TypeScript SDK](../surfaces/typescript-sdk.md). Recovery for public surfaces is
`checkpoint` + `recover` — see [Events and recovery](events-recovery.md).

## Internal skill router (contributor / gauntlet)

When exercising the skill runtime or [browser gauntlet](gauntlet.md), the
in-process router recognizes:

### SkillGhost

Use `/ghost on|off|status` (`/ghost` is equivalent to `on`). SkillGhost
negotiates a coherent browser profile before launch, reports the effective
engine and supported capabilities, and freezes that profile for the session.
Required capabilities fail closed; explicitly optional capabilities may degrade
and remain visible in status. Turning Ghost off stops applying it to new work,
but a live browser may report `restartRequired` until the next safe launch
boundary.

SkillGhost reports what the selected browser actually supports. It does not
disguise one engine as another or inject contradictory page-visible values.

### SkillZigZagZig

Use `/zigzagzig run|status|stop` (`/zigzagzig` is equivalent to `run`).
ZigZagZig applies a bounded recovery ladder to the original postcondition:

1. retry read-only observation;
2. resolve the semantic target again;
3. change the interaction method;
4. reconcile the verified checkpoint;
5. start a fresh Ghost session;
6. choose another compatible engine;
7. restart from the last durable boundary.

Each tactic consumes the existing workflow deadline and tactic budget. A
mutation with an unknown effect is never blindly replayed: Bobby inspects or
reconciles it first, returning `effectUncertain` when safety cannot be proven.
Durable recovery receipts bind the issued decision, command identity, evidence,
and terminal result so an interrupted finalization can settle exactly once.

## Failure and evidence contract

Skill outcomes use typed failures such as `unsupportedCapability`,
`targetDrift`, `checkpointMismatch`, `strategyExhausted`, and
`engineUnavailable`. Status and retained evidence are redacted: they may
include effective profile digests, tactic decisions, checkpoint identity,
timing, and attempt lineage, but not raw credentials, cookies, authentication
headers, or unrestricted host paths.
