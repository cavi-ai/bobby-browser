---
documentedVersion: {{PRODUCT_VERSION}}
---

# Context graph

The context graph is bobby's memory of page structure. An agent can ask where a described control is and get a target with a confidence score, without pulling a whole accessibility tree into its context.

There are two layers:

- **Session memory.** Observations from the current session. Any command that may have changed the page invalidates them. Never persisted.
- **Persisted memory.** Per profile and per site, promoted from verified intent outcomes. It is written to disk before the command returns, so it survives restarts.

Persisted memory needs a profile with a durable identity: a paired Firefox profile, a Chromium selection with a `profileId`, or managed Chromium (remembered under `managed-chromium` while each session's browser profile stays disposable). A Firefox selection with no paired profile, and a `prefer` list, neither read nor write it. See [Configuration](../guides/configuration.md#engine-selection).

## What it stores

For each site (scheme plus registrable domain, never a full URL), page pattern (query and fragment removed, numeric path segments templated), form and control:

- Role, accessible name, ordinal and form membership.
- Per intent kind: success and failure counts, the day of the last verified success, and whether the record was `observed` or `vision-promoted`.
- Per page: labels. A step's purpose (trimmed, lowercased, at most 128 bytes) mapped to the control a verified vision answer resolved for it, with the same counts.

A completed form or extraction records every field it resolved. A failed one counts the failure against the failing step only. It never stores typed values, credentials, page text, screenshots, journal IDs or exact timestamps.

## Ask before you snapshot

| Surface | Call | Needs |
|---|---|---|
| MCP | `context_ask` | `page:read` |
| MCP | `context_neighbors` | `context:read` |
| HTTP | `GET /v1/context/ask`, `/neighbors`, `/site/{key}` | `context:read` |
| SDKs | `contextAsk`, `contextNeighbors`, `contextSite` | `context:read` |

```json
{"name": "context_ask", "arguments": {"workflowHandle": "wf_0123456789abcdef0123456789abcdef", "description": "Email address field"}}
```

A hit returns the target. A miss returns `hit: false` with `nextStep: "a11y_snapshot"`. Every answer says where it came from: `observedAt` is a live page generation or `persisted`, and remembered answers carry their `source`. A remembered answer never claims to be a live observation.

When neither the page's last snapshot nor memory matches the description's words, and the session has `visionAssist` and the caller `vision:assist`, bobby offers up to 5 observed controls to the vision provider by label, ranked by shared words. It asks only a provider that answers labels without a screenshot, and never captures one. A pick at or above the 0.75 confidence floor is returned with `source: "semantic"`. It is a proposal like any other answer; the action that uses it is still verified.

`workflow_observe` with a `goal` does this for you. When memory answers, it returns the remembered target with `source: "retained"` and takes no snapshot.

## Vision ranking

When an intent escalates to vision on a page with remembered context, bobby orders the stuck step's near-miss candidates (up to 10) before the first 5 go to the provider. A remembered control counts when it matches by role and name, has the same intent kind and has more verified successes than failures. The one with the best record moves first. Ties leave the order unchanged. A record with no verification day is never used. The provider still chooses, and the action is still verified.

## Vision labels

Before a stuck step goes to the provider, bobby checks the page's labels. A label for the step's purpose with more verified successes than failures acts on its control with no screenshot and no provider call, and evidence marks the path `context`. A label only acts on the first control with its role and name. If the act fails, the step goes to the provider as usual. A verified vision answer for the purpose replaces the label, and a failed step counts against it. Labels sit behind the same gates as vision: a session without `visionAssist` never uses them.

## Retention and erasure

- Records without a verified success for `[context].ttl_days` (default 90) are removed when the store opens.
- `bobby context list --profile <id>` lists remembered sites. Use `--profile managed-chromium` for managed Chromium.
- `bobby context forget <site> --profile <id>` erases one site immediately and verifies the erasure.
- `bobby doctor` reports the store path, site count, size and lock health.
- Only the runtime holds the store while it runs. CLI and doctor access is read-only or refused.

## Limits

The in-memory cache keeps up to 256 sites and 64 MiB per profile. Sites that leave the cache reload from disk, and eviction deletes nothing. A site file is limited to 2 MiB and 16,384 records. An oversized or unreadable file is preserved, reported, and skipped. The runtime continues with live context.

```toml
[context.limits]
max_file_bytes = 2097152
max_site_records = 16384
max_resident_sites = 256
max_resident_bytes = 67108864
```

All limits must be positive. `bobby context --config <file> list` applies the same limits.
