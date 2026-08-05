# Crate completion — design

**Status:** ready for user review (brainstorm 2026-08-04…05; self-reviewed)  
**Base:** `origin/main` @ design-time tip (worktree `feat/crate-completion`)  
**Delivery:** vertical slices (Approach 1)

## Goal

Finish incomplete surfaces and hygiene so product claims match runtime:

1. Authenticated CDP is a first-class, shared-runtime host path (`bobby cdp`).
2. Vision ACP auth modes go through `auth-broker` (no silent collapse to a boolean).
3. Wasteful deps (`js-engine`, workspace `tokio = full`) are trimmed.
4. Product docs and rustdoc (IDE hover / signatures) match reality.

## Decisions

| Topic | Choice |
|---|---|
| Delivery | Vertical slices; merge independently where practical |
| CDP foundation | Adapter stays host-agnostic; v1 host = same process, dedicated port |
| CDP default | Off on plain `bobby serve` |
| CDP CLI | `bobby cdp` starts runtime like serve **and** binds CDP port |
| Future CDP host | Sibling/attach possible later without rewriting `CdpGateway` |
| auth-broker | Wire in this plan (vision ACP / connect / doctor) |
| Editor ACP (`acp-gateway`) | Docs + rustdoc only; start path remains `cargo build -p acp-gateway` |
| rustdoc scope | `bobby-browser-client`, CLI entrypoints, and adapter `pub` APIs (`cdp-gateway`, `mcp-gateway`, `acp-gateway`, `sdk-core`, `interface-core`) |
| Keychain / OAuth scrape | Still forbidden; harness-mediated auth only |
| God-file splits | Out of scope |

## Non-goals

- Second browser pool / CDP process that constructs its own `RuntimeService`
- Mounting CDP discovery on the HTTP `/v1` port (dedicated port only)
- Productizing `bobby acp` / install wiring for editor ACP
- Wiring or deleting unrelated thin crates beyond listed hygiene
- Live-model CI or Keychain access

## Inventory (ground truth at brainstorm)

| Item | Class | Evidence |
|---|---|---|
| `cdp-gateway` (~5.8k LOC) | Live library; **not** hosted by `bobby serve` | Conformance example + tests; broker mounts MCP only |
| `auth-broker` | Contract stub | Zero dependents; landed with ACP vision; strategies mirror `VisionAuthKind` |
| Vision ACP auth today | Incomplete | `node-registry` maps oauth/* + advertised → `with_advertised_auth(bool)`; first harness method only |
| `js-engine` | Misnamed thin helper | Only `bound_result`; unused `anyhow`/`tokio`/`tracing`/`types` |
| Workspace `tokio` | Compile waste | `features = ["full"]` inherited widely |
| `bobby-browser-client` | Intentional publish orphan | `types` re-exports via `#[path]` — keep |
| `acp-gateway` lib comment | Stale | Claims server not present; `server.rs` exists |

---

## Slice 1 — Hygiene

### Tokio

- Remove blanket `full` from `[workspace.dependencies] tokio`.
- Each crate lists the features it needs (`rt`, `rt-multi-thread`, `macros`, `sync`, `time`, `net`, `io-util`, `fs`, `signal`, `process`, …).
- Prefer shared workspace feature sets only where many crates share the same bundle; avoid reintroducing `full`.
- Verify with workspace `cargo check` / targeted test crates after the cut.

### `js-engine`

- Keep the crate and `bound_result` API (callers in `worker-pool`).
- Drop unused dependencies from `Cargo.toml` (`anyhow`, `tokio`, `tracing`, `types` if still unreferenced).
- Optional rename deferred (out of scope); rustdoc must state it is result-bounding, not a JS runtime.

---

## Slice 2 — CDP host

### Architecture

```
bobby cdp  (or serve with cdp.enabled)
    │
    ├─ RuntimeService + Authority  (single pool)
    ├─ HTTP /v1 + /v1/mcp          (existing broker listener)
    └─ CDP listener                (second bind: host/port)
            │
            ▼
       CdpGateway::new(authority, runtime, registry, ws_base)
            .router()  → /json/version, /json/list, DevTools WS
```

- **`cdp-gateway`**: no change to ownership model. Still takes `Authority` + `RuntimeInterface`. Remains embeddable by a future sibling host.
- **Host (`broker` + CLI)**: owns bind lifecycle. Builds one gateway from the same authority/runtime as HTTP/MCP.
- **Not** a separate `RuntimeService` in another process for v1.

### Config

- New optional `[cdp]` table with fixed keys:
  - `enabled` — default `false`
  - `host` — default `127.0.0.1`
  - `port` — dedicated DevTools port (default chosen in implementation plan; CLI `--cdp-port` overrides)
- Plain `bobby serve`: CDP off unless `[cdp].enabled = true`.
- `bobby cdp`: same serve startup path with CDP forced on for that process (config port/host still apply unless overridden).

### Errors

- CDP bind failure → process exits (same class as HTTP bind failure).
- CDP disabled → no CDP listener; HTTP port must not expose `/json/*`.

### Testing

- Smoke: `bobby cdp` answers `/json/version` with bearer; plain `serve` does not.
- Reuse existing `CdpGateway` / interface security & recovery CDP tests.
- Doctor (if extended): report CDP listen address when enabled.

### Docs

- Replace “production embeddings wire alongside” with: `bobby cdp`, dedicated port, shared runtime.
- Snippets use the CDP port, not the HTTP `/v1` port alone.

---

## Slice 3 — Vision auth via `auth-broker`

### Architecture

- `VisionAuthKind` ↔ `auth_broker::AuthStrategy` 1:1 mapping.
- ACP-backed `AuthDriver` implementation (in `acp-client` or thin adapter crate dep): harness-mediated `discover` / `begin` / `continue_auth` / `refresh` / `revoke` / `health`.
- `node-registry::vision` uses the driver; **stops** collapsing OAuth modes to a boolean.
- Unsupported or incomplete interactive flows fail with `AuthError` (no silent “call first advertised method” for distinct kinds unless discover says that method is the strategy).

### Bounds

- No macOS Keychain / `security` / silent IDE OAuth scrape.
- Interactive device-code / auth-code continue paths surface through CLI (`bobby vision connect`) and doctor checks where already promised.

### Testing

- Mapping unit tests.
- Fake `AuthDriver` + existing fake ACP harness: begin/continue paths per strategy.
- Regression: `oauth-authorization-code`, `oauth-device-code`, and `advertised` are not identical no-ops in registry wiring.

---

## Slice 4 — Docs and rustdoc

### Product docs

- CDP surface pages + README control-surface list match Slice 2.
- Vision auth docs match Slice 3 (honest about harness mediation).
- Editor ACP: keep cargo build start path; fix crate-level stale comments.

### Rustdoc (IDE signatures)

Rust equivalent of JSDoc: module/item docs that rust-analyzer shows on hover.

- Required: `bobby-browser-client` public API; CLI public command surface; `pub` items on `cdp-gateway`, `mcp-gateway`, `acp-gateway`, `sdk-core`, `interface-core`.
- When touching files in slices 1–3, document new/`pub` items in the same PR.
- Prefer short purpose + parameter/return notes over essay comments.

---

## Error handling (cross-cutting)

| Case | Behavior |
|---|---|
| CDP port in use / bind fail | Exit nonzero; clear message |
| CDP disabled | No listener |
| Auth strategy unsupported by harness | `UnsupportedStrategy` / doctor fail |
| Auth transport failure | `AuthError::Transport`; fail closed |
| Tokio feature miss at compile | Fix per-crate features (CI `cargo check`) |

## Testing summary

| Slice | Proof |
|---|---|
| 1 | Workspace compiles; `js-engine` tests still pass |
| 2 | CDP smoke + existing gateway suites |
| 3 | Driver/mapping tests + harness fakes |
| 4 | Doc review; `cargo doc -p … --no-deps` for targeted crates |

## Rollout order

1. Hygiene (low risk, compile wins)  
2. CDP host + docs for CDP  
3. auth-broker wiring + vision docs  
4. rustdoc pass on adapter/`client` pubs  

## Open follow-ups (explicitly later)

- True multi-process CDP attach channel (host swap only).
- `bobby acp` / install productization for editor ACP.
- Rename `js-engine` to match role.
- God-file decompositions (`firefox-companion` worker, gateway servers).
