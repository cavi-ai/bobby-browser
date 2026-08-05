# Crate Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish CDP hosting (`bobby cdp`), wire `auth-broker` into vision ACP auth, trim wasteful deps, and align docs/rustdoc with runtime.

**Architecture:** Vertical slices. `CdpGateway` stays an adapter over `Authority` + `RuntimeInterface`; broker/CLI bind a dedicated second port on the same process. Vision auth maps `VisionAuthKind` through `auth_broker::AuthDriver` (harness-mediated). Tokio loses workspace `full`; `js-engine` drops unused deps.

**Tech Stack:** Rust 2021, Tokio, Axum, existing `cdp-gateway` / `broker` / `acp-client` / `auth-broker` / `config` / `cli`.

## Global Constraints

- Work only in `.worktrees/crate-completion` on `feat/crate-completion` (or a rebase onto current `origin/main`).
- Never push to `origin/main`; never use macOS Keychain / `security` / silent OAuth scrape.
- CDP must share the serve process `RuntimeService` / session ownership — no second browser pool.
- CDP discovery stays off the HTTP `/v1` port (dedicated `[cdp]` listener only).
- Editor ACP productization (`bobby acp` / install) is out of scope — docs/rustdoc only.
- God-file splits are out of scope.
- Prefer failing closed over collapsing distinct auth strategies into a boolean.

---

## File Map

### Slice 1 — Hygiene

- `Cargo.toml` (workspace): `tokio` features without `full`.
- `crates/*/Cargo.toml`: per-crate tokio feature adds where compile fails.
- `crates/js-engine/Cargo.toml`: drop unused deps.
- `crates/js-engine/src/lib.rs`: rustdoc clarifying result-bounding role.

### Slice 2 — CDP host

- `crates/config/src/lib.rs`: `CdpConfig` + `AppConfig.cdp`.
- `crates/broker/Cargo.toml`: depend on `cdp-gateway`.
- `crates/broker/src/cdp.rs` (new): build `CdpGateway` + spawn dedicated listener.
- `crates/broker/src/lib.rs`: integrate CDP into `serve_with_runtime` / bootstrap.
- `crates/cli/src/main.rs`: `CliCommand::Cdp`, `--cdp-port`, doctor line when enabled.
- `crates/broker/tests/` or `crates/cli` unit/integration: smoke enabled vs disabled.
- `docs/bobby-browser/source/pages/surfaces/cdp.md`, `README.md`: product docs.

### Slice 3 — Vision auth

- `crates/acp-client/Cargo.toml`: depend on `auth-broker`.
- `crates/acp-client/src/auth_driver.rs` (new): ACP `AuthDriver` impl.
- `crates/acp-client/src/session.rs`: stop boolean-only auth; use strategy.
- `crates/acp-client/src/lib.rs`: re-exports.
- `crates/node-registry/Cargo.toml`: depend on `auth-broker` if mapping lives here.
- `crates/node-registry/src/lib.rs`: map `VisionAuthKind` → `AuthStrategy`; no oauth collapse.
- `crates/config/src/lib.rs` (optional helper): `VisionAuthKind::to_auth_strategy()`.
- `crates/cli/src/vision_connect.rs` / doctor: surface `continue_auth` / failures honestly.
- `docs/bobby-browser/source/pages/guides/configuration.md`: harness-mediated truth.

### Slice 4 — Rustdoc

- `crates/acp-gateway/src/lib.rs`: remove stale “not here yet” claim.
- `crates/bobby-browser-client/src/*.rs`, `crates/cdp-gateway/src/lib.rs`, `crates/mcp-gateway/src/lib.rs`, `crates/sdk-core/src/lib.rs`, `crates/interface-core/src/lib.rs`: pub item docs.
- CLI module docs on public clap commands touched in Slice 2.

---

### Task 1: Trim `js-engine` dependencies

**Files:**
- Modify: `crates/js-engine/Cargo.toml`
- Modify: `crates/js-engine/src/lib.rs`
- Test: `crates/js-engine` unit tests in `lib.rs`

**Interfaces:**
- Produces: unchanged `js_engine::bound_result(value: serde_json::Value, max_bytes: usize) -> (serde_json::Value, bool)`
- Consumes: `serde_json` only for production code

- [ ] **Step 1: Confirm current unused deps**

Run: `rg -n 'anyhow|tokio|tracing|types::|use types' crates/js-engine/`
Expected: matches only in `Cargo.toml` (and possibly comments), not in `src/lib.rs`

- [ ] **Step 2: Rewrite `Cargo.toml` dependencies**

```toml
[dependencies]
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
```

Remove `anyhow`, `tokio`, `tracing`, and `types`. Keep `thiserror` only if used; if unused, remove it too (current `lib.rs` does not use `thiserror` — drop it).

- [ ] **Step 3: Expand crate rustdoc**

Replace the module docs with:

```rust
//! Bounded shaping of JavaScript evaluation results for worker evidence payloads.
//!
//! This crate is **not** a JavaScript runtime. Chromium evaluates scripts;
//! [`bound_result`] enforces a serialized size budget before results leave the worker.
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p js-engine`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/js-engine/Cargo.toml crates/js-engine/src/lib.rs Cargo.lock
git commit -m "chore(js-engine): drop unused deps and clarify crate role"
```

---

### Task 2: Remove workspace `tokio` `full`

**Files:**
- Modify: `Cargo.toml` (`[workspace.dependencies]`)
- Modify: every `crates/*/Cargo.toml` that fails `cargo check` after the cut
- Special cases already pinning features: `crates/task-scheduler/Cargo.toml`, `crates/vision-proxy/Cargo.toml`, `crates/bobby-browser-client/Cargo.toml`, `crates/worker-pool/Cargo.toml`, `crates/fingerprinting/Cargo.toml`

**Interfaces:**
- Produces: workspace `tokio` without `full`; crates compile with explicit features
- Consumes: none

- [ ] **Step 1: Change workspace default**

In root `Cargo.toml`:

```toml
tokio = { version = "1", default-features = false, features = [
  "macros",
  "rt-multi-thread",
  "sync",
  "time",
  "net",
  "io-util",
  "fs",
  "signal",
  "process",
  "io-std",
] }
```

Do **not** include `full`.

- [ ] **Step 2: Soften crates that force `full` again**

In `crates/task-scheduler/Cargo.toml` and `crates/vision-proxy/Cargo.toml`, replace `features = ["full"]` with the specific extras they need (likely `test-util` only for vision-proxy tests). Prefer:

```toml
tokio = { workspace = true, features = ["test-util"] }  # vision-proxy if needed
```

and for task-scheduler inherit workspace features only unless a missing feature fails compile.

- [ ] **Step 3: Compile-fix loop**

Run: `cargo check --workspace --tests 2>&1 | tee /tmp/tokio-check.txt`
For each unresolved tokio API, add the minimal feature to that crate’s `Cargo.toml` (`parking_lot` is not required; use tokio’s own features).

Expected: eventually clean `cargo check --workspace --tests`

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/*/Cargo.toml Cargo.lock
git commit -m "chore: drop tokio full from workspace defaults"
```

---

### Task 3: Add `[cdp]` config

**Files:**
- Modify: `crates/config/src/lib.rs`
- Test: unit tests in `crates/config/src/lib.rs` (same file pattern as other config defaults)

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
  #[serde(default)]
  pub struct CdpConfig {
      pub enabled: bool,
      pub host: String,
      pub port: u16,
  }
  // Default: enabled=false, host="127.0.0.1", port=9222
  ```
- Produces: `AppConfig { …, pub cdp: CdpConfig }`
- Consumes: existing serde defaults pattern on `AppConfig`

- [ ] **Step 1: Write failing default/parse tests**

```rust
#[test]
fn cdp_defaults_disabled_on_loopback_9222() {
    let cfg = AppConfig::default();
    assert!(!cfg.cdp.enabled);
    assert_eq!(cfg.cdp.host, "127.0.0.1");
    assert_eq!(cfg.cdp.port, 9222);
}

#[test]
fn cdp_table_parses_from_toml() {
    let cfg = AppConfig::from_toml_str(
        r#"
        [cdp]
        enabled = true
        host = "127.0.0.1"
        port = 9333
        "#,
    )
    .expect("parse");
    assert!(cfg.cdp.enabled);
    assert_eq!(cfg.cdp.port, 9333);
}
```

- [ ] **Step 2: Run tests — expect FAIL**

Run: `cargo test -p config cdp_defaults -- --nocapture`
Expected: FAIL (no `cdp` field)

- [ ] **Step 3: Implement `CdpConfig` and wire into `AppConfig`**

Add struct + `Default` + `AppConfig.cdp` with `#[serde(default)]`. Update any `AppConfig { … }` literals in this crate’s tests.

- [ ] **Step 4: Fix workspace `AppConfig` literals**

Run: `cargo test -p config`
Then: `cargo check --workspace 2>&1 | rg 'missing field.cdp|AppConfig' | head`
Fix every struct-literal miss (`cdp: CdpConfig::default()` or `..Default::default()`).

- [ ] **Step 5: Commit**

```bash
git add crates/config/src/lib.rs
# plus any AppConfig literal fixes
git commit -m "feat(config): add [cdp] enabled/host/port"
```

---

### Task 4: Broker hosts CDP on a dedicated listener

**Files:**
- Create: `crates/broker/src/cdp.rs`
- Modify: `crates/broker/Cargo.toml` (add `cdp-gateway` path dep)
- Modify: `crates/broker/src/lib.rs` (`mod cdp;`, `serve_with_runtime`)
- Test: `crates/broker/tests/cdp_listen.rs` (new) or extend existing broker tests

**Interfaces:**
- Consumes: `config::CdpConfig`, `cdp_gateway::{CdpGateway, MethodRegistry}`, `sdk_core::AuthenticatedRuntime`, `interface_core::Authority`, `RuntimeService`, session ownership recorder from bootstrap
- Produces: `cdp::spawn_cdp_listener(...)` that binds `host:port`, serves `gateway.router()`, returns `JoinHandle` + bound `SocketAddr`
- Produces: when `config.cdp.enabled`, `serve_with_runtime` spawns CDP alongside HTTP; bind failure aborts startup

**Design notes for implementer:**
- Mirror conformance: `CdpGateway::new(authority, authenticated_runtime, MethodRegistry::compiled(), format!("ws://{host}:{port}"))` plus artifacts/upload roots from `AppConfig.browser`.
- Use `AuthenticatedRuntime::with_session_ownership(service.clone(), startup_handle, ownership.clone())` so CDP session visibility matches HTTP/MCP.
- Do **not** mount CDP routes on the HTTP router.
- On disable: no second bind.

- [ ] **Step 1: Write failing listen test**

```rust
#[tokio::test]
async fn cdp_listener_serves_json_version_when_enabled() {
    // boot broker test app with cdp.enabled=true on port 0 (or ephemeral)
    // GET http://{addr}/json/version with Authorization: Bearer {token}
    // assert 200 and webSocketDebuggerUrl present
}

#[tokio::test]
async fn http_port_does_not_expose_json_version_when_cdp_disabled() {
    // boot with cdp.enabled=false
    // GET http://{http_addr}/json/version -> 404 (or connection semantics proving no CDP routes)
}
```

Prefer extending existing broker test helpers in `crates/broker/src/lib.rs` `#[cfg(test)]` if they already boot `AppState`.

- [ ] **Step 2: Run test — expect FAIL**

Run: `cargo test -p broker cdp_listener -- --nocapture`
Expected: FAIL

- [ ] **Step 3: Implement `crates/broker/src/cdp.rs`**

```rust
pub struct CdpListen {
    pub addr: SocketAddr,
    pub handle: tokio::task::JoinHandle<anyhow::Result<()>>,
}

pub async fn spawn_cdp_listener(
    config: &config::CdpConfig,
    authority: Arc<dyn interface_core::Authority>,
    runtime: Arc<sdk_core::AuthenticatedRuntime>,
    artifacts: artifact_store::ArtifactStore,
    upload_staging_root: PathBuf,
) -> anyhow::Result<CdpListen> {
    let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let ws_base = format!("ws://{}", bound);
    let gateway = Arc::new(
        cdp_gateway::CdpGateway::new(
            authority,
            runtime,
            cdp_gateway::MethodRegistry::compiled(),
            ws_base,
        )
        .with_artifacts(artifacts)
        .with_upload_staging_root(upload_staging_root),
    );
    let handle = tokio::spawn(async move {
        axum::serve(listener, gateway.router())
            .await
            .map_err(anyhow::Error::new)
    });
    Ok(CdpListen { addr: bound, handle })
}
```

Adjust trait objects to match `CdpGateway::new` generics (`Authority + 'static`, `RuntimeInterface + 'static`).

- [ ] **Step 4: Wire into `serve_with_runtime`**

Refactor just enough of bootstrap to retain `Arc` authority, `RuntimeService` / `AuthenticatedRuntime`, ownership, and artifact store after HTTP bind. If `config.cdp.enabled`, call `spawn_cdp_listener` before entering `serve_listener_graceful`. On CDP bind error, return `Err`. On shutdown, abort or await the CDP join handle with the same shutdown timeout.

- [ ] **Step 5: Make tests PASS**

Run: `cargo test -p broker cdp_`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/broker/Cargo.toml crates/broker/src/cdp.rs crates/broker/src/lib.rs crates/broker/tests/
git commit -m "feat(broker): host authenticated CDP on dedicated port"
```

---

### Task 5: CLI `bobby cdp`

**Files:**
- Modify: `crates/cli/src/main.rs` (`CliCommand`, match arm, doctor)
- Modify: `crates/cli/Cargo.toml` only if new deps required (should not be)
- Test: clap parse tests beside existing `serve_and_mcp_stdio_…` tests

**Interfaces:**
- Produces: `CliCommand::Cdp { config, bootstrap, cdp_port: Option<u16>, … same vision flags as Serve as needed }`
- Produces: running `bobby cdp` forces `config.cdp.enabled = true`, applies `--cdp-port` override, then calls the same `broker::serve_with_*` path as Serve
- Consumes: Task 3 `CdpConfig`, Task 4 broker hosting

- [ ] **Step 1: Failing clap tests**

```rust
#[test]
fn cdp_command_parses_port_override() {
    let cli = Cli::try_parse_from(["bobby", "cdp", "--cdp-port", "9333"]).unwrap();
    // assert matches CliCommand::Cdp { cdp_port: Some(9333), .. }
}
```

- [ ] **Step 2: Implement command**

Add to clap enum (help text: “Run the runtime with authenticated CDP enabled on the dedicated port”). In the match arm, load config/startup like Serve, then:

```rust
config.cdp.enabled = true;
if let Some(port) = cdp_port {
    config.cdp.port = port;
}
// then identical worker factory / serve_with_* call as Serve
```

- [ ] **Step 3: Doctor**

When config has `cdp.enabled`, record a check `cdp-listen` with `host:port`. When disabled, either skip or report `cdp disabled` as informational — do not fail doctor solely because CDP is off.

- [ ] **Step 4: Tests + commit**

Run: `cargo test -p bobby-browser cdp_command`
```bash
git add crates/cli/src/main.rs
git commit -m "feat(cli): bobby cdp enables dedicated CDP listener"
```

---

### Task 6: CDP product docs

**Files:**
- Modify: `docs/bobby-browser/source/pages/surfaces/cdp.md`
- Modify: `README.md` (control surfaces blurb if it still implies embeddings-only)
- Mirror versioned docs under `docs/bobby-browser/v0.6.0/` only if the docs build requires it (follow existing mirror practice)

**Interfaces:**
- Produces: docs stating `bobby cdp`, `[cdp]` config, dedicated port, shared runtime with serve

- [ ] **Step 1: Rewrite CDP page lead**

Replace “Production embeddings wire the gateway alongside the runtime” with concrete start:

```markdown
`bobby cdp` starts the runtime like `bobby serve` and binds authenticated CDP
on `[cdp].host`:`[cdp].port` (default `127.0.0.1:9222`). Plain `bobby serve`
leaves CDP off unless `[cdp].enabled = true`.
```

Update Playwright snippet base URL to the CDP port.

- [ ] **Step 2: Commit**

```bash
git add docs/bobby-browser/source/pages/surfaces/cdp.md README.md
git commit -m "docs: CDP hosted by bobby cdp on dedicated port"
```

---

### Task 7: Map `VisionAuthKind` ↔ `AuthStrategy`

**Files:**
- Modify: `crates/config/src/lib.rs` (helper on `VisionAuthKind`) **or** `crates/node-registry/src/lib.rs` (local fn)
- Modify: `crates/node-registry/Cargo.toml` — add `auth-broker` path dep
- Test: `crates/config` or `crates/node-registry` unit test

**Interfaces:**
- Produces:
  ```rust
  pub fn vision_auth_strategy(kind: VisionAuthKind) -> auth_broker::AuthStrategy {
      match kind {
          VisionAuthKind::Advertised => AuthStrategy::Advertised,
          VisionAuthKind::OAuthAuthorizationCode => AuthStrategy::OAuthAuthorizationCode,
          VisionAuthKind::OAuthDeviceCode => AuthStrategy::OAuthDeviceCode,
          VisionAuthKind::Environment => AuthStrategy::Environment,
          VisionAuthKind::ExistingSession => AuthStrategy::ExistingSession,
          VisionAuthKind::None => AuthStrategy::None,
      }
  }
  ```

- [ ] **Step 1: Failing exhaustive mapping test**

```rust
#[test]
fn every_vision_auth_kind_maps_to_distinct_auth_strategy() {
    use auth_broker::AuthStrategy::*;
    assert_eq!(vision_auth_strategy(VisionAuthKind::Advertised), Advertised);
    assert_eq!(vision_auth_strategy(VisionAuthKind::OAuthAuthorizationCode), OAuthAuthorizationCode);
    assert_eq!(vision_auth_strategy(VisionAuthKind::OAuthDeviceCode), OAuthDeviceCode);
    assert_eq!(vision_auth_strategy(VisionAuthKind::Environment), Environment);
    assert_eq!(vision_auth_strategy(VisionAuthKind::ExistingSession), ExistingSession);
    assert_eq!(vision_auth_strategy(VisionAuthKind::None), None);
}
```

- [ ] **Step 2: Implement + PASS + commit**

```bash
git commit -m "feat: map VisionAuthKind to auth-broker AuthStrategy"
```

---

### Task 8: ACP `AuthDriver` and stop boolean collapse

**Files:**
- Create: `crates/acp-client/src/auth_driver.rs`
- Modify: `crates/acp-client/src/session.rs`
- Modify: `crates/acp-client/src/lib.rs`
- Modify: `crates/acp-client/Cargo.toml` — `auth-broker = { path = "../auth-broker" }`
- Modify: `crates/node-registry/src/lib.rs` — pass strategy into `AcpVisionAssist`
- Test: `crates/acp-client/tests/…` using `fake_acp_harness`; `crates/node-registry` regression that oauth modes are not identical no-ops at the wiring layer

**Interfaces:**
- Produces: `AcpAuthDriver` implementing `auth_broker::AuthDriver` for a configured harness command/args
- Produces: `AcpVisionAssist::with_auth_strategy(AuthStrategy)` (replace or deprecate `with_advertised_auth(bool)`)
- Consumes: Task 7 mapping; existing ACP session transport

**Behavior:**
- `discover`: from harness `initialize` auth_methods → `AuthCapabilities`
- `begin(strategy)`: select matching harness method id for that strategy; for `None`/`ExistingSession`/`Environment` follow harness capabilities or return `UnsupportedStrategy` / no-op authenticate per strategy semantics
- `Advertised`: first advertised method (preserve today’s happy path)
- OAuth strategies: require a matching method; do **not** silently fall back to “first method” if discover lacks that strategy
- No Keychain

- [ ] **Step 1: Failing tests**

```rust
#[tokio::test]
async fn oauth_device_code_does_not_silently_use_unrelated_advertised_method() {
    // harness advertises only method id "password"
    // strategy OAuthDeviceCode -> UnsupportedStrategy (or Pending challenge only if harness lists device code)
}

#[tokio::test]
async fn advertised_still_authenticates_first_method() {
    // existing fake harness behavior preserved
}
```

- [ ] **Step 2: Implement driver + wire `node-registry::vision`**

Replace:

```rust
.with_advertised_auth(matches!(profile.auth, Advertised | OAuthAuthorizationCode | OAuthDeviceCode))
```

with strategy-based construction using `vision_auth_strategy(profile.auth)`.

- [ ] **Step 3: PASS tests + commit**

```bash
git commit -m "feat(acp-client): AuthDriver-backed vision auth strategies"
```

---

### Task 9: Vision connect / doctor honesty + docs

**Files:**
- Modify: `crates/cli/src/vision_connect.rs`, `crates/cli/src/main.rs` (doctor auth-path messaging)
- Modify: `docs/bobby-browser/source/pages/guides/configuration.md`
- Modify: `docs/bobby-browser/source/pages/guides/troubleshooting.md` if auth-path text overclaims

**Interfaces:**
- Produces: CLI/docs that describe harness-mediated strategies and real failure modes from Task 8

- [ ] **Step 1: Update configuration.md auth paragraph**

State that Bobby drives harness `authenticate` via `auth-broker` strategies; Bobby does not scrape IDE Keychains; unsupported harness methods fail closed.

- [ ] **Step 2: Align doctor `vision-auth-path` copy with driver outcomes**

- [ ] **Step 3: Commit**

```bash
git commit -m "docs: honest vision ACP auth via auth-broker"
```

---

### Task 10: Adapter rustdoc + stale ACP crate docs

**Files:**
- Modify: `crates/acp-gateway/src/lib.rs` (remove “Deliberately not here yet: the stdio server loop…” — server exists)
- Modify: `pub` modules/types in `bobby-browser-client`, `cdp-gateway`, `mcp-gateway`, `sdk-core`, `interface-core` lacking docs
- Prefer documenting modules and primary entry types first (`CdpGateway`, `Server`, `AuthenticatedRuntime`, `Authority`, client constructors)

**Interfaces:**
- Produces: rust-analyzer hover text on public adapter/client APIs

- [ ] **Step 1: Fix `acp-gateway` crate docs to describe current `AcpServer` + escalation**

- [ ] **Step 2: Add/refresh `//!` / `///` on pub items missing docs in listed crates**

Keep comments short: purpose, params, errors.

- [ ] **Step 3: Generate docs**

Run: `cargo doc -p bobby-browser-client -p cdp-gateway -p mcp-gateway -p acp-gateway -p sdk-core -p interface-core --no-deps`
Expected: success; spot-check missing_docs only if crate enables `#![warn(missing_docs)]` (do not blanket-enable warn across workspace in this task)

- [ ] **Step 4: Commit**

```bash
git commit -m "docs(rust): adapter and client rustdoc for IDE signatures"
```

---

## Spec coverage checklist

| Spec requirement | Task |
|---|---|
| `js-engine` unused deps | Task 1 |
| Tokio without `full` | Task 2 |
| `[cdp]` config defaults off | Task 3 |
| Same-process dedicated CDP port | Task 4 |
| `bobby cdp` CLI | Task 5 |
| CDP product docs | Task 6 |
| auth-broker wiring / no boolean collapse | Tasks 7–8 |
| Vision docs honesty | Task 9 |
| rustdoc scope B + stale ACP comment | Task 10 |
| Editor ACP productization | Explicitly omitted |
| Multi-process CDP attach | Follow-up only |

## Self-review notes

- No TBD placeholders in tasks.
- Default CDP port locked to **9222** (override via config/CLI).
- `CdpGateway` API unchanged; host wiring is broker/CLI only.
- Auth strategies stay harness-mediated; Keychain remains forbidden.
