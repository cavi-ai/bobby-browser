# JavaScript Evaluation — Implementation Design

**Status:** Approved (Franco: deny-by-default, per-session opt-in). Branch `feat/oss-alpha`.

**Goal:** Realize the spec's in-scope "Evaluate JavaScript under execution policy" (browser-automation-runtime-design.md §41, §115). Today only the `javascript:evaluate` capability string exists; there is no command, no policy, no execution path. Add all of it, deny-by-default.

## Security model — two independent gates (both must pass)

1. **Token capability gate** — the bearer must hold `javascript:evaluate`. Enforced at the existing chokepoint `AuthenticatedRuntime::submit` by extending E1's `command_extra_capability` map: `EvaluateJavaScript → Capability::JavascriptEvaluate`. A token without it → `MissingCapability` (403 / JSON-RPC error) before dispatch.
2. **Per-session execution-policy gate** — the *session* must have opted into JS. `ExecutionPolicy { javascript_evaluation: bool }` defaults to `false`. Enforced in `RuntimeService::submit`: if the command is `EvaluateJavaScript` and the owning session's policy does not allow it → `CommandOutcome::PolicyDenied` (403). Deny-by-default: a session created without an explicit grant cannot run JS even if the token holds the capability.

Defense-in-depth: the ChromiumWorker also refuses `evaluate_javascript` if its session policy denies (so no dispatch path can bypass the policy). The `RuntimeService` gate is the primary, the worker check is the backstop.

## Task breakdown

### F1 — types
- `crates/types/src/commands.rs`:
  - `PrimitiveCommand::EvaluateJavaScript(EvaluateJavaScriptCommand)`.
  - `struct EvaluateJavaScriptCommand { expression: String, timeout_ms: u64, #[serde(default)] await_promise: bool }` (camelCase, deny_unknown_fields consistent with siblings).
  - `struct ExecutionPolicy { #[serde(default)] javascript_evaluation: bool }` — `Default` = all false. Add `execution_policy: ExecutionPolicy` (`#[serde(default)]`) to `CreateSessionRequest`.
- `crates/types/src/state.rs`: add `execution_policy: ExecutionPolicy` to `SessionState`.
- `crates/types/src/outcomes.rs`: `Evidence::JavaScriptResult { value: serde_json::Value, truncated: bool }`.
- `command_extra_capability` (sdk-core) and every exhaustive `match PrimitiveCommand` in the workspace must get the new arm — the compiler lists them; handle each (adaptive.rs dispatch, eligibility.classify already has a Chromium catch-all so JS routes to Chromium automatically — verify).
- Tests: serde round-trip of the command and ExecutionPolicy; ExecutionPolicy::default() denies.

### F2 — js-engine (bounded result serialization)
- Fill `crates/js-engine`: `pub fn bound_result(value: serde_json::Value, max_bytes: usize) -> (serde_json::Value, bool /*truncated*/)` — serialize, and if over the cap return a truncated string marker + `truncated=true`. This is the home for result-shaping so the worker stays thin. Unit-test the boundary.
- Wire js-engine as a dependency of worker-pool (it is currently an orphan).

### F3 — worker-pool execution
- `BrowserWorker` trait: `async fn evaluate_javascript(&self, page_id: &PageId, command: &EvaluateJavaScriptCommand) -> Result<Vec<Evidence>, CommandError>` with a default that returns `CommandError` unsupported (so non-Chromium workers don't silently succeed).
- `ChromiumWorker`: implement via `chromiumoxide` `EvaluateParams` (reuse the internal `evaluate_in_context` pattern in targeting.rs) with `await_promise`, a hard `timeout_ms` bound (wrap in `tokio::time::timeout`), and pass the raw JSON result through `js_engine::bound_result` (cap from config, e.g. a `max_js_result_bytes`). Returns `Evidence::JavaScriptResult`.
- Defense-in-depth: the worker must know its session's policy — thread `ExecutionPolicy` into the worker at launch (WorkerFactory / session config) and refuse with a policy `CommandError` if JS not allowed. If threading policy into the worker is heavy, at minimum keep the RuntimeService gate (F4) authoritative and document the worker backstop as a follow-up — but prefer wiring it.

### F4 — RuntimeService policy gate + session storage
- `crates/session-manager`: `create` stores `execution_policy` from the request into `SessionState`.
- `crates/sdk-core` `RuntimeService::submit`: if `matches!(envelope.command, PrimitiveCommand::EvaluateJavaScript(_))`, look up `self.sessions.get(&envelope.session_id)`; if the session's `execution_policy.javascript_evaluation` is false → return `CommandOutcome::PolicyDenied { .. }` (match the existing PolicyDenied shape). Otherwise proceed to `self.pages.execute`.
- `crates/sdk-core` `AuthenticatedRuntime`: extend `command_extra_capability` map with `EvaluateJavaScript → JavascriptEvaluate`.

### F5 — page-runtime dispatch
- `crates/page-runtime/src/adaptive.rs` `browser_execute` match: `PrimitiveCommand::EvaluateJavaScript(command) => lease.worker().evaluate_javascript(page_id, command).await?`.
- Confirm `eligibility.classify` routes EvaluateJavaScript to the Chromium path (catch-all `IneligibleCommand` → Chromium). Add an explicit arm if clarity warrants.

### F6 — mcp-gateway schema + broker surface
- `crates/mcp-gateway/src/schema.rs`: `session_create` args gain optional `executionPolicy: { javascriptEvaluation: bool }`. `command_execute` already refs `CommandEnvelope`, so the new primitive flows without schema change (verify the CommandEnvelope `$defs` are generated from the type, not hand-listed — if hand-listed, add the variant).
- No broker route change — HTTP `submit_command` and MCP `command_execute` both inherit both gates via the runtime.

### F7 — conformance + acceptance tests
- `crates/interface-conformance/tests/` (or extend mcp_http.rs): 
  - token WITHOUT `javascript:evaluate` → EvaluateJavaScript → capability denied.
  - token WITH capability, session WITHOUT policy grant → `PolicyDenied`.
  - token WITH capability, session WITH `executionPolicy.javascriptEvaluation=true` → JS runs, `JavaScriptResult` evidence returned (Chromium-gated test may be `#[ignore]` like the other live-Chrome tests; a non-Chrome unit path should still prove the two gates independently of a real browser).
- The two-gate denials must be provable WITHOUT a real browser (they fail before execution); only the happy-path JS run needs Chromium and can be `#[ignore]`d in CI.

## Guardrails
- Deny-by-default is the invariant: any new session, any code path, no JS unless BOTH the token capability and the explicit per-session policy grant are present.
- Result is always bounded (`js_engine::bound_result`) and execution always timeout-bounded — no unbounded JS.
- Exhaustive matches on `PrimitiveCommand` everywhere (no wildcards) so this and future privileged primitives force a compile-time decision.
