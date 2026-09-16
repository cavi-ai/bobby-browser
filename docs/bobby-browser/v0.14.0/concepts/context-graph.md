---
documentedVersion: 0.14.0
---

# Context graph

The context graph is bobby's memory of page structure. It exists so an agent
can ask "where is the control described as X" and get a bound target with a
confidence score instead of pulling a whole accessibility tree into its
context.

Two layers:

- **Session-hot** — observations from the current session, invalidated on any
  command that may have changed the page. Always available, never persisted.
- **Persisted** — per-profile, per-site structural memory promoted from
  verified intent outcomes. Only runtimes whose engine selection carries a
  durable profile identity (a Firefox companion enrollment) write or read
  this layer. Chromium sessions have disposable profiles and no durable
  identity, so they read nothing and write nothing — by design, not by
  accident.

## What persists

Per site (keyed by scheme + registrable domain, never a full URL), per page
pattern (query/fragment stripped, numeric path segments templated), per form,
per control:

- `role`, `accessible_name`, `ordinal`, form membership
- Per intent kind: success/failure counters, the day of the last verified
  success, and how the record entered the graph (`observed` or
  `vision-promoted`)

**Never persisted:** typed values, credentials, page text, screenshots,
journal ids, exact timestamps. Timestamps are day-precision by construction.
The CI privacy canary (`context_privacy`) fills a form with a canary value
through the live harness and scans every byte of the store for it.

## Provenance

Every `context_ask` answer says where it came from: `observedAt` is a live
page generation or `persisted`, and remembered answers carry their `source`.
A remembered answer never claims to be a live observation.

## Vision candidate ranking

When an intent escalates to vision on a page with retained context, the
runtime orders the stuck step's near-miss candidates (up to 10) before the
first 5 go to the provider, so a verified control outside the first 5 can
still reach it. A retained control counts only for the same intent kind,
with more verified successes than failures, matched to a candidate by role
and accessible name. The
candidate with the best record (net successes, then successes, then the most
recent verification day, then observed over vision-promoted) moves first.

- Two candidates tied for best leave the order unchanged
  (`ambiguousRefusal`).
- A retained record with no verification day is never used; when it is the
  only match, the lookup reports `staleRejection`.
- Ranking only reorders. The provider still chooses, and the action is still
  verified.

`runtime_info.operationalMetrics.contextRankedVision` counts these lookups
without values, names, URLs, or selectors: `attempted`; the record source
(`sourceObserved`, `sourceVisionPromoted`, `sourceUnreported`); the outcome
(`hit`, `miss`, `ambiguousRefusal`, `staleRejection`, `error`); provider
escalations by transport (`providerEscalations`, `providerHttp`,
`providerAcp`, `providerDirectLocal`); `candidateRankingLatencyMs`;
`confidence`; and `verificationAccepted` / `verificationRejected`.

## Retention and erasure

- Records not verified within `[context].ttl_days` (default 90) are swept at
  store open.
- `bobby context list --profile <id>` shows remembered sites.
- `bobby context forget <site-key> --profile <id>` erases one site
  immediately and totally, and verifies the erasure before reporting.
- `bobby doctor` reports the store path, site count, bytes, and lock health.
- The store is single-writer: only the runtime process holds it. CLI and
  doctor access is read-only or refused while the runtime runs.

## Reading

- MCP `context_ask` (requires `page:read`) — live first, persisted fallback.
- MCP `context_neighbors` (requires `context:read`) — the remembered form
  structure around a located control.
- HTTP `GET /v1/context/ask` and `GET /v1/context/site/{key}` (both require
  `context:read`).

On a known site, ask before you snapshot: `context_ask` answers before the
first accessibility observation of a session.
