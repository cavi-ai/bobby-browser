# Changelog

## Unreleased

Release candidate. No version bump yet.

- MCP `initialize` returns short `instructions`: explore startup phase, `toolset_select` + re-list, `error.repair`, `autoCheckpoint` default, `bobby://` recovery docs.
- `tools/list` advertise-only output collapse for `recovery_status`, `page_open`, `session_create`, `session_list`, and `checkpoint_save` (opaque / top-level keys; validation schemas unchanged). Full catalog ~80.9 KiB / 128 KiB (~49 KiB headroom); explore ~25.2 KiB.
- MCP failures carry a machine-readable repair hint: command-layer failures set `error.repair`, RPC-layer rejections set `error.data.repair`, each `{action, doc}` pointing into `bobby://failure-taxonomy`. A `needsReconciliation` outcome always carries the never-retry repair, whatever its error code.
- `runtime_info`'s `capabilities` list reports vision wiring: `vision-assist` and `vision-provider` appear only when configured, so an agent can tell an unconfigured provider apart from a transient vision failure without shell access.
- `tools/list` advertise-only trim: the constant `$schema` URL is dropped from advertised input and output schemas, and `workflow_recover`'s `RecoveryDecision` is advertised as a status-tag projection (the same treatment `Evidence` already had). Validation schemas and `tools/call` are unchanged.
- The `tool_schema_sizes` example prints the per-tool composition (description / input / output / annotations / examples), so future growth is attributable at a glance.

## 0.7.0 - 2026-08-06

- **Breaking (MCP surface):** `tools/list` now defaults to the `explore` phase instead of the full surface. An existing client that connects and does not call `toolset_select` sees the read/snapshot/navigate lifecycle only — no mutation, intent, checkpoint, or `command_execute` tools. `[mcp] startup_toolset`, overridden by `BOBBY_MCP_TOOLSET`, selects the phase at connect: `explore` (default), `act`, `intent`, `verify`, `full`. The first `tools/list` is ~42 KiB on `explore` against 128 KiB on `full`. Capability gates are unchanged and remain the only enforcement boundary; hidden tools stay callable.
- MCP `job_submit` / `job_status` / `job_cancel` mirror HTTP `/v1/jobs` (same caps). Advertised in `full`, `act`, and `verify` when a job port is attached (`bobby mcp-stdio` and `bobby serve` MCP HTTP). Built-in handlers: `echo`, `sleep`, `http_probe`, `http_wait`, and `http_fetch` (SSRF-safe; `http_fetch` returns a truncated GET body so agents need not open a browser for health/API JSON); `bobby://job-handlers` documents payloads; `bobby doctor` reports them under `job-handlers`.
- `tools/list` advertise-only schema trim: opaque deep nests for `WaitCondition` / `IntentHints` / `TargetSpec` / `ScreenshotMode` and a collapsed `form_snapshot` outputSchema. Validation schemas and `tools/call` are unchanged. Frees catalog headroom so `job_*` fit in `full` with ≥1 KiB spare under the 128 KiB frame budget.
- **Breaking (bootstrap):** default `bobby init` / `bobby install` / loopback auto-init mint the **agent** preset (no `authority:admin`). Use `--preset unrestricted` for the operator floor. Marker-less existing `bootstrap.env` files still heal as unrestricted. `bobby doctor` reports `bootstrap-preset`.
- README / install docs: not on homebrew-core yet; checklist for a future core submission (formula name, three binaries, bottles, audit).
- Unix release binaries are `strip`ped before packaging. Installation docs cover curl download of GitHub Release assets. `scripts/install.sh` is the one-liner installer (`BOBBY_VERSION`, `INSTALL_DIR`).
- Docs: public agent skill (`bobby install --skill`) vs internal Ghost / ZigZagZig recovery (Rust: `SkillGhost` / `SkillZigZagZig`) — navigation title "Internal skill runtime (Ghost / ZigZagZig)".
- `context_ask` falls back to the persisted per-profile store, with `source` of `observed`, `persisted`, or `visionPromoted` on every answer.
- `context_neighbors` returns remembered form structure around a control.
- `context:read` capability, over MCP and `/v1`. Bootstrap heal floors (unrestricted and `agent`) include `context:read` so agents are not stranded without it after init.
- `bobby context list` and `bobby context forget <site>`; `bobby doctor` reports store size; retention sweeps on open.
- Release-gate canary asserts no typed values or credentials reach the context store.
- `IntentHints.accessibleName`: an `a11y_snapshot` node's `target` passes into any `intent_*` tool verbatim. Equivalent to an exact `nearText`; both set to different values is refused as `intentCompileFailed`.
- The `tools/list` byte-budget gate measures all 21 capabilities, not the 15 it had listed. `Capability::ALL` is the single source.
- Idempotent retry works over MCP. The digest covered the whole `CommandEnvelope`, including `deadline` and the per-attempt `commandId`/`attemptId`/`workflowId`, all of which the gateway mints fresh on every dispatch with no caller override — so a retry never matched its own first try and every retry answered `idempotencyConflict` on a command that may already have landed. Identity is now the command: schema version, session, page, the command itself, and the one-shot vision consent. Same key with a different command still conflicts. A replayed outcome no longer has the current call's `workflowId`/`attemptId` stamped onto it, because that pair never ran.
- `Evidence::Wait` carries `observed`, the value the condition matched on: the element text or value, the URL, or the document ready state. The poll already read it to decide whether it was satisfied and then discarded it, so verifying a submit cost a second round trip to learn what had just been confirmed. Bounded at 512 characters on a character boundary. Chromium reports it for text, value, URL, and document conditions; Firefox for URL. Absent on element and network-quiet conditions, which match on presence and counts rather than a value.
- `recovery_status` accepts `sessionId` instead of `workflowId` and answers with that session's recoverable workflows, newest first, capped at 32. `recovery_status` and `workflow_recover` were keyed by `workflowId` alone and the checkpoint store had no index, so an agent that was compacted or restarted could not name — and therefore could not reach — its own in-flight workflow. Exactly one of the two keys is required. Ownership is enforced against the session-ownership registry, and a corrupt entry is skipped rather than failing the listing.
- The context-store privacy canary scans the store instead of a directory that cannot exist. It read `<context dir>/<profile>` literally, but the store hex-encodes the profile component, so the scan found nothing and the test failed on its own precondition — the property it exists to prove was never actually checked. It now walks every `.json` under the store root at any depth.
- `intent_submit_and_verify`, `intent_follow`, and boundary `click` accept `autoCheckpoint`, which **defaults to `true`**. A Boundary command took three calls — pin `commandId`/`attemptId`, `checkpoint_save` naming those exact ids, then submit — because a `WorkflowCheckpoint` needs `restartUrl` and `currentUrl` and nothing on the runtime interface exposes live page state. The runtime now mints it and returns its `checkpointId`. Pass `autoCheckpoint: false` only to author `invariants` or `replayableInputs`. `Executor::validate` is unchanged and still matches on all five fields; a checkpoint that fails to save fails the submit.
- `bobby install --host acp` and `bobby acp-stdio` mirror the MCP stdio entrypoint for ACP hosts (credential load + exec of `acp-gateway`).
- Release packages and `Formula/bobby-browser.rb` ship the gateway trio (`bobby`, `mcp-gateway`, `acp-gateway`). `bobby doctor` checks sibling gateway presence on PATH. README documents `brew install --formula ./Formula/bobby-browser.rb` and the three-binary / Explore size tip.
- `bobby init --preset agent` mints a bootstrap without `authority:admin`; heal respects the preset marker and never widens an agent floor to unrestricted. (Superseded for the default floor by the Unreleased breaking bootstrap note above.) `bobby doctor` reports `bootstrap-preset`.
- `bobby doctor` reminds that `vision:assist` still needs session `executionPolicy.visionAssist=true`, and that `javascript:evaluate` still needs `executionPolicy.javascriptEvaluation=true` (cap alone is not enough).
- Vision HTTP endpoints resolve only through `NodeRegistry` (sdk-core extract + vision child spawn). `bobby doctor` warns when both `[nodes]` and `[vision].endpoint_url` are set (`[nodes]` wins).
- `bobby vision connect` (direct) writes the loopback HTTP endpoint under `[nodes.vision]` (`kind = "vision"`) and keeps provider profiles under `[vision.providers.*]`. It no longer persists a dual-truth `[vision].endpoint_url`.
- ACP `session/prompt` freeform-text parse errors include a concrete structured JSON example in the error data. MCP prompt list descriptions carry one-line recovery tips.
- MCP gateway `tools/call` argument structs and capability/operation/description tables live in `tool_args` / `tool_meta`; the name-matched dispatch match lives in `server/tool_dispatch.rs` (behavior unchanged).

## 0.6.0 - 2026-08-05
- Add outbound ACP vision delegation: a workflow harness performs the vision work over ACP and bobby stores no provider credentials. Bounded packet and result validation, isolated child-session lifecycle, image capability negotiation, and harness-advertised authentication. `bobby vision connect --backend acp` writes the config and `bobby doctor` covers it; direct providers are unchanged.
- An isolated ACP vision harness that requests interactive permission fails closed — the request is cancelled and the child session closed, so it cannot produce an accepted result after asking for authority the parent session did not grant.
- The optional session `visionNode` selector is exposed through the MCP `session_create` input/output schemas and the TypeScript SDK contract validator. MCP agents could configure ACP vision but `session_create` rejected the selector, leaving the configured route unreachable through that interface. The versioned Rust session contract is unchanged.
- A rejected advertised ACP authentication is classified as `AcpClientError::Authentication` and fails before the isolated child session is created, instead of surfacing as transport loss.
- Firefox executes vision-assisted intents: bounded accessibility snapshots become semantic intent candidates, and vision-selected coordinates execute through native BiDi pointer actions. Unsupported candidate scopes are rejected.
- The Firefox companion popup pairs and re-pairs the profile through the native host's `enrollProfile` control path, so the credential never leaves the host. First-time enroll bootstraps the companion; a day-2 enroll reuses the live `bobby serve` descriptor. The install persists `browser-selection.json` and the enroll defaults, sharing one selection builder with the CLI. Adds the toolbar icons. The guide prefers popup Pair; `bobby enroll-firefox-profile` remains for CI.
- `bobby install --companion` and `make firefox` upgrade a bobby-managed native host instead of failing with `native-host installation destination already exists` whenever the installed wrapper or manifest differed by bytes — JSON key order, or a different `bobby` path. Operator-owned files still refuse. Manifest keys serialize alphabetically so a repeat install is idempotent.
- `scripts/check-version-agreement.py` covers `packages/firefox-companion/manifest.json`. It had drifted to 0.3.1, which is the version `about:addons` showed.
- The Chromium worker never holds the `pages` mutex across browser I/O. The guard was held across every CDP round trip — navigate (up to 300s), inspect, click, humanized typing, screenshot, HAR dump — so one hung page call serialized every other page on the session, and close/terminate blocked on the same mutex, making a single stuck call unrecoverable. All 30 lookup sites clone the Arc-backed page handle under the lock and drop the guard before I/O; the remaining guard uses are map mutations.
- The envelope deadline is enforced mid-flight, not only at admission. `inspect`, `click`, `screenshot`, and the cookie operations carry no timeout of their own, so a hung call stalled its command forever. Execution now races the deadline at the single dispatch site and fails retryable `DeadlineExceeded`; callers declare the budget via `timeoutMs`, clamped to the 300s ceiling. An aborted call may still execute browser-side, as with any timeout.
- `cookie_set` and `cookie_delete` held the `pages` mutex across a recursive `get_cookies` acquisition of the same non-reentrant lock. Every cookie command hung its session permanently.
- `download_url` is advertised, required, parsed, and threaded through the MCP gateway. The gateway always sent `pageId: None` while the executor requires one, so every call failed `invalidRequest`.
- Firefox `page_close` returns `Evidence::Page` captured before teardown. Without it the executor recorded every successful close as a retryable failure, so agents retried a destructive operation.
- Extraction and the Firefox JavaScript-result path truncate on a character boundary. Byte-index truncation panicked inside a multi-byte codepoint, so any non-ASCII page could kill the command.
- Two check-then-act races are single-flighted: concurrent leases on one session launched two browsers against one profile directory (`SingletonLock`), and concurrent `network_log` calls spawned duplicate HAR collectors that split entries into a flaky `verificationFailed`.
- **Security:** `get_job` and `cancel_job` checked the capability but never ownership, so any principal holding `job:read` could read another principal's job payload and result, and `job:cancel` could cancel their work. Jobs record their submitting owner (serde-compatible with pre-ownership journals; a `None` owner stays readable) and cross-principal access answers as absence.
- **Security:** the `cookie_get` URL filter matched by substring, so `example.com` matched `notexample.com` and `path=/` matched every URL — agents received other origins' cookies as correct results and re-injected them with `cookie_set`. Matching is now dot-boundary host suffix plus real path prefix.
- **Security:** an idempotency permit dropped between reserve and finish — a cancel or panic mid-request — wedged its key until deadline for every retry, and enough wedges exhausted the principal's capacity. Permits abandon on `Drop`, disarmed by finish or explicit abandon.
- **Security:** companion reconnect credentials are stored as a SHA-256 digest and compared in constant time. They were stored in plaintext and compared with an early-exit `!=`.
- **Security:** a live MCP SSE stream re-evaluates its `SubscribeEvents` guard on every poll, so a token rotation pauses event delivery instead of the stream running on the capability set it opened with. Revoked or expired principals still close the channel.
- **Security:** the SSRF deny path covers IPv4-compatible IPv6 (`::127.0.0.1`), the 6to4 and Teredo prefixes, IPv4 broadcast, and CGNAT `100.64.0.0/10`.
- **Security:** vision endpoint responses check `Content-Length` up front and enforce the size bound while reading chunked, instead of buffering the whole body before the check.
- `vision-proxy`'s `validate_extract` enforces a 64 KiB serialized bound. It was a no-op stub, so unbounded upstream extraction JSON reached the runtime unchecked.
- Checkpoint files are written 0600 on Unix; they previously took the process umask.
- `authority.json` syncs the file and its directory before the rename — `flush()` is userspace-only, so a crash could leave a torn file — and an unparsable file is quarantined to `<path>.corrupt` instead of failing boot with every token stranded.
- A corrupt artifact-ownership record is quarantined instead of permanently rejecting all future registrations.
- `ArtifactStore` construction sweeps crash-orphaned staging directories, which each leaked up to `max_bytes` for the life of the installation.
- The session manager releases the worker before unregistering the session. The old order dropped the API handle first, so a failed release leaked the browser with nothing left to retry through; on failure the session now stays registered.
- `events_read` no longer journals a receipt per poll into the 64-event ring, where its own traffic evicted real events and poisoned resume cursors.
- `Page.getFrameTree` awaits the page lock instead of fabricating an `about:blank` frame tree from a `try_lock` fallback under contention.
- The task scheduler re-checks terminal state when inserting an abort handle; a job that finished faster than the insert left the handle in place forever.
- `bobby doctor`'s 15s MCP handshake deadline applies to the read. The blocking `read_line` ignored it, so a mute gateway hung; the read runs on a thread with `recv_timeout`.
- The checkpoint store prunes uncontended entries from its per-workflow lock map, which otherwise held one `Arc<Mutex>` per workflow id for the life of the process.
- The page registry URL update after navigate (a stale `page_list`) and the expected-URL settle wait before click verification are logged instead of silently discarded.
- The Firefox companion removes pending prompts when their context is destroyed, and serializes cookie names and values as JSON into the `document.cookie` statement — the hand-rolled escape let a `;` or newline break out of or inject into it.
- A `bootstrap.env` parse error reports the line number instead of the offending line, which is the bearer token.
- The MCP stdio read loop shares the 64-deep in-flight bound the notification branch already had. A client pipelining thousands of `tools/call` frames without reading responses could exhaust memory and browser processes.
- `replace_session`'s timeout error states its contract: the timeout reports while the owned cleanup finishes in the background, and the swap lands exactly once afterwards.
- The MCP gateway tests run against a real `RuntimeService` — journal, worker pool, recovery coordinator — over an evidence-producing fake worker, and assert terminal outcomes: `intentKind`, `resolutionPath`, candidates, verification evidence, and the resolved evidence inside a checkpoint read back through a real `RecoveryCoordinator`. The `RuntimeService::default()` fixture behind ~50 of them had no journal, workers, or recovery, so every dispatched command failed downstream and the assertions (`assert_ne!(code, -32602)`, dispatch counts) passed under total downstream failure.
- Twelve of the fourteen security release cases run without a browser. They prove auth, framing, quota, store, and lifecycle boundaries and never lease a worker, but ran only behind the installed-Chromium ignore gate, so CI proved none of them. The release matrix still runs all fourteen; canary leakage and principal isolation open real pages and stay Chrome-gated.
- `page-runtime` pins that a typed value which never lands fails `verificationFailed`. The positive `type_text` test passed whether or not the post-type comparison ran, because the fake echoed the expected value after any write.
- The default fingerprint profile and the behavioral benchmark are pinned to reviewed literals — UA, platform, locale, timezone, WebGL vendor and renderer, screen and client-hint fields, plus the seeded overall, category, and 19 per-dimension scores — in addition to matching generated output. Both previously compared against values the code under test produced, so regeneration blessed any drift.

## 0.5.1 - 2026-08-04
- **Breaking (MCP):** a command whose outcome status is not `completed` now returns `isError: true`. Failed commands previously reported `isError: false`, so hosts checking `isError` treated every failure as success. `restarted`/`resumed` recovery decisions remain success.
- Boundary commands are usable over the flat MCP tools: `intent_*` tools and `click` accept optional `commandId`/`attemptId` (threaded through unchanged), and every outcome echoes `attemptId` alongside `workflowId`/`commandId`. The Boundary gate requires a pre-saved checkpoint naming those exact ids, but the server minted them internally and never surfaced them, so `intent_submit_and_verify` and boundary `click` could never pass it over MCP. The `fill_and_submit_form` prompt is rewritten to the only working order (snapshot, fill, pin ids, checkpoint, submit) and states the exact `CompleteFormField`/`ExtractField` shapes it previously omitted.
- The static `bobby://` resources (capabilities, failure-taxonomy, intents, primitives) are readable by any authenticated principal; only live `artifact://` entries require `artifact:read`. An agent that hit `missingCapability` could not read the repair documentation for it. Revoked/expired principals are still denied.
- `bobby://failure-taxonomy` documents the RPC-layer `InterfaceErrorCode` vocabulary (14 codes, each with a repair action), and the advertised `errorCode` enum adds `targetObscured`/`targetOutOfBounds` (29/29 variants, pinned by a schemars parity test).
- Tool annotations corrected: `readOnlyHint` on `wait_for`, `intent_locate`, `intent_wait_for_state`, `intent_extract`; `openWorldHint` on `page_open`, `click`, `intent_follow`, `intent_submit_and_verify`. `network_log`'s description names both real failure codes.
- `tools/list` advertises shared `Id` schemas by `$ref`, keeping the full surface at ~125 KB with ~5.8 KB of headroom inside the 128 KiB connect budget after the new `commandId`/`attemptId` fields.
- A tag publishes every artifact. `release-binaries` creates the GitHub Release before uploading assets — nothing created it, so `gh release upload` answered "release not found" and v0.5.0 built five binaries and shipped none. The body is the CHANGELOG section for the version via `scripts/changelog-section.py`, so a version with no section fails the tag instead of publishing empty notes.
- `release-binaries` calls `publish-docs` directly, and `publish-docs` gains a `workflow_call` trigger. A release created with `GITHUB_TOKEN` does not fire `on: release`, so the documentation artifact never built.
- npm publishes through the OIDC trusted publisher instead of a stored token: the account requires 2FA on publish, so a token fails with `EOTP` and CI cannot hold a one-time password. `publish-npm.yml` is renamed `publish.yml` and the job runs in the `production` environment, matching the org, repository, workflow filename, and environment the trusted publisher matches against.

## 0.5.0 - 2026-08-04
- `bobby install` can put `bobby` (+ sibling `mcp-gateway`) on PATH (`~/.cargo/bin` when already on PATH, else `~/.local/bin`). On by default in the interactive checklist and via `--cli` / `make cli`.
- `make` help lists every target in sections (Setup / Service / Quality / dogfood); `install` was missing from the menu. `make firefox` builds and installs the Firefox companion only. `bobby install --companion` no longer also wires Claude or regenerates the bootstrap credential.
- `bobby install` interactive checklist uses ↑/↓ + space (esc quits) instead of number-key toggles, so arrow keys actually select options.
- **Breaking (MCP):** `session_list` `structuredContent` is `{"sessions": [...]}`, was a bare array. MCP types `structuredContent` as an object; a conforming client rejects the entire `tools/list` on a non-object `outputSchema`, so all 43 tools failed to load. `outputSchema` is object-shaped to match and the `ARRAY_SHAPED_OUTPUT` exception is removed. `GET /v1/sessions` is unchanged.
- Add `bobby vision-proxy`: loopback adapter that speaks bobby’s propose/extract HTTP contract and forwards to an OpenAI-compatible chat/completions upstream (no model SDK in the runtime).
- Add named `[vision.providers.<name>]` profiles plus `[vision].provider` selection (OpenAI / Ollama / LM Studio presets; any OpenAI-compatible custom `base_url`). Secrets stay in env via `token_env` / `api_key_env` — never in TOML.
- Add `bobby vision connect` to write loopback `[vision]` + a provider profile (interactive or `--yes`).
- `bobby serve` and `bobby mcp-stdio` gain `--vision` / `--no-vision`: when the resolved vision endpoint is loopback and a provider is selected, the parent auto-spawns `vision-proxy` and tears it down on exit; non-loopback never spawns. `mcp-stdio` sets `BOBBY_BROWSER_CONFIG` and stays resident when holding the sidecar.
- `bobby doctor` warns on missing `vision.provider` profile and missing upstream `api_key_env` (skipped for local profiles without a key), and distinguishes loopback vs external `vision-endpoint` hints.
- Docs: configuration / troubleshooting / intents cover connect → `--vision`, LM Studio (MLX), and custom OpenAI-compatible providers.
- Browser selection resolves through one function for `bobby serve`, the stdio gateway, and `bobby doctor`: `AUTOMATION_RUNTIME_BROWSER_SELECTION`, then the persisted enrollment at `<config-dir>/bobby-browser/browser-selection.json`, then the built-in default; a present-but-malformed source fails closed. The gateway composed its factory eagerly, so an install with no env var hit the fail-closed Firefox default and exited at startup, which every MCP host reported as a dead server. `bobby enroll-firefox-profile` persists the selection atomically (0600 on Unix) instead of instructing an env export, and `doctor` reports which source resolved.
- `run_doctor` is a structured `DoctorReport` with bootstrap-expiry and handshake-error classification extracted as pure functions, covered by tests for malformed config, malformed selection, and the satisfiable path.
- The stdio gateway loads `AppConfig` (`BOBBY_BROWSER_CONFIG` or `./config.toml`) and composes its worker factory the way `bobby serve` does. It ran on `AppConfig::default()` with a hardcoded Chromium factory, so MCP sessions ignored `headless = false` and enrolled Firefox profiles while `serve` and `doctor` validated a different path. Factory composition moved from `cli` into `firefox-companion::selection`; `cli` re-exports it unchanged.
- `POST /v1/commands` emits `Retry-After` on a 503 `retryableFailure` (1s when the outcome carries no explicit backoff), matching `resourceExhausted`.
- Publish the `/v1` OpenAPI description in the docs artifact, stamped with the product and interface versions. Nullable schemas use OpenAPI 3.1 type arrays instead of the 3.0 `nullable` keyword, and `OpenPageRequest` drops `additionalProperties: false`.
- The MCP stdio server serializes handshake frames and all traffic before `Ready`. Back-to-back `notifications/initialized` + `tools/call` could observe `AwaitingInitializedNotification` and return `-32002`, which surfaced as a missing `structuredContent.id` on `session_create`.
- Add the Firefox companion operator popup: connection state, session policy (fingerprint owner, humanize), and a fingerprint toggle rendered checked and disabled when the host owns the setting.
- `skill/SKILL.md` documents the runtime wiring an agent needs: that the gateway loads `config.toml`, the engine resolution order, `doctor`'s source reporting, and that Chromium profiles are disposable while the Firefox companion attaches to a real profile where logins persist.
- `bobby` builds on Windows. `exec_mcp_stdio` was `#[cfg(unix)]` but called unconditionally, `GATEWAY_COMMAND` never matched `mcp-gateway.exe`, and the Unix-only artifact boundary in `interface-core` failed `-D warnings` as dead code. CI gains a windows-latest job building the crate the release ships; the break first appeared in `release-binaries` on the `v0.4.0` tag.
- The npm publish step sets `NODE_AUTH_TOKEN` from `NPM_TOKEN`. Without it the token was empty and npm answered 404 on PUT, so publish had never succeeded.
- Land the pending Dependabot bumps across Rust, JS, and Actions, with the `rand` 0.10 fixes (`RngExt`, and `SessionRandom` cloned from its seed after `StdRng` lost `Clone`).
- Comments across 54 files are compressed to facts — invariants, units, bounds, safety constraints — dropping build-history narration and references to internal planning documents. `docs/superpowers/` is removed from the published tree.

## 0.4.0 - 2026-08-03
- **Naming:** one scope, one prefix, one tag. Internal npm packages move off the unowned `@bobby-browser` scope to `@cavi-ai/bobby-gauntlet`, `@cavi-ai/bobby-firefox-companion`, `@cavi-ai/bobby-interface-conformance`; `@cavi-ai/bobby-browser` is unchanged, so nothing published breaks. 25 internal crates are `publish = false` — only `bobby-browser-client` and `bobby-browser` are products, and names like `types`, `config`, and `broker` are not claimed on crates.io. `sdk-v*` and `crate-v*` collapse into `v*`: one tag ships binaries, npm, and the crate.
- `publish-crates.yml` publishes. It was named "Publish crates (dry-run)" and only ever ran `cargo publish --dry-run`, which is why crates.io is empty. The dry run stays as a pre-flight on every trigger; the real publish is gated on a `v*` tag, so `workflow_dispatch` remains a safe rehearsal.
- Add `scripts/check-version-agreement.py`, run in CI: every crate, every `package.json`, and every path-dependency pin must carry the workspace version, npm packages must be under `@cavi-ai`, and only the two product crates may publish. npm reached 0.3.1 while the last `sdk-v*` tag was 0.3.0 because nothing checked.
- Dogfooding the Chromium humanized stream caught three cadence bugs a detector would flag: paste bursts went out as sub-millisecond key storms (now `Input.insertText`, which is how a real paste presents), and clear-first backspaces fired at CDP speed (now paced 30–90ms apart). A biometrics dogfood test pools four typing rounds and asserts detector-relevant invariants: no sub-10ms key intervals, human variance in cadence, and a non-collinear mouse approach.
- Remove the collector probe's `chromeRuntime` check. It failed whenever `window.chrome` existed without `chrome.runtime` — the state of stock Chrome on an ordinary page, and the state the injection deliberately produces, since CreepJS's `hasBadChromeRuntime` fingerprints the TypeError shape of a faked `chrome.runtime.sendMessage`. The probe asserted the opposite of the design it was probing. The real invariant is locked as a unit test instead, and all six `fingerprint_conformance` tests now run unskipped in CI.
- `executionPolicy.humanize` now works on Chromium, not just Firefox: the Chromium worker synthesizes typing and pointer input through `behavioral-engine` (paced key events, curved approach paths, hover dwell) and emits `Evidence::Humanization` with action count and synthesized milliseconds, matching the Firefox contract. Two engine quirks are handled explicitly: headless pages get an activation before input so clicks can focus, and clear-first backspaces over the field instead of chording Ctrl/Cmd+A, which loops Chrome's command pipeline.
- Capability parsing is now a single `FromStr` table on `types::Capability`, replacing five hand-maintained per-binary parse tables that drifted twice in a week (a gateway rejected `job:*`, then bootstrap rejected `browser:*` — both times against credentials `bobby init` itself wrote). A round-trip test fails if a new variant misses the table.
- `bobby install` gains a Browser companions item: installs the Firefox companion (extension copied into the bobby config dir, native-host wrapper + manifest into Mozilla's per-platform directory) and prints the one remaining step (start Firefox with `--remote-debugging-port`, run `bobby enroll-firefox-profile`). On by default when a Firefox binary is found; `--companion`/`--extension` for non-interactive use. `make install` builds the extension first.
- Add `bobby mcp-stdio`: the MCP entrypoint agent hosts point at — it loads the bootstrap credential from `bootstrap.env` itself and execs the stdio gateway, so host configs carry no secrets and no env wiring.
- Add `bobby install` (and `make install`): one-command agent setup — bootstrap credential, MCP config merge into Claude Code / Zed / VS Code (preserving existing entries), and agent-skill installation. Interactive checklist with toggles by default; `--host`/`--skill`/`--yes` for non-interactive use.
- `adaptive_http_capacity` no longer asserts wall-clock latency. The envelope deadline the runtime already enforces is the real bound, so a `Completed` outcome proves the work fit inside it; the second, tighter assertion added no coverage and failed whenever the suite ran alongside other test binaries. Capacity and routing assertions are unchanged.
- Add the two mixed rows of the vision double gate to `crates/intent-engine/tests/vision_escalation.rs`: an open session policy does not substitute for `vision:assist`, and holding `vision:assist` does not substitute for the session grant. Both assert by provider call count, so "never consulted" is a fact rather than an inference from an error code.
- The context graph records which command ids produced evidence against a page (`record_command`, `commands_for`), bounded at 64 per page. It stores ids, not evidence, so the journal stays the one authority on what happened. History survives target invalidation and is dropped on session close.
- CI runs four previously-uncovered ignored suites: `default_profile_golden` in the browser-free job, and `mcp_live`, `fingerprint_conformance`, `orphan_reap` in the chromium job.
- `fingerprint_conformance` resolves Chrome from `BOBBY_CHROME_EXECUTABLE` before `CHROME_PATH`. It read only `CHROME_PATH`, which CI does not set, so it launched nothing.
- Add `acp-gateway`, the fourth adapter: ACP schema v1 over stdio (`initialize`, `session/new`, `session/prompt`, `session/cancel`, `session/update`, `session/request_permission`). Prompts are structured (optional `url` plus one `types::IntentCommand` in the `command_execute` wire shape) — no planner, no freeform text. Permission prompts cover vision escalation only and can lift a session gate, never mint a capability.
- Set `[profile.dev]` and `[profile.test]` to `debug = "line-tables-only"` and `incremental = false`. A clean workspace build drops from 16 GB to 9.0 GB and from 24,138 to 2,479 files in `target/debug/deps`, the 6.2 GB per-build incremental cache goes away, and the build runs in 117s instead of 202s. Backtraces keep file, line, and column. `incremental = false` is also what lets `sccache` work at all — it does not cache incrementally-compiled crates, so it sat at a 0% hit rate before and reaches 2,158 hits of 2,808 on a rebuild after this. Use `RUSTFLAGS="-C debuginfo=2"` for a session that needs full DWARF.
- **Breaking (idempotency digests):** `canonical_sha256` sorts JSON object keys recursively before hashing. It previously inherited ordering from `serde_json`'s default `BTreeMap` backend, so any dependency enabling `serde_json/preserve_order` switched the whole workspace to insertion order and two equivalent requests hashed differently — a retry would execute instead of replaying the retained result. Array order is preserved, since it carries meaning. Digests change once; in-flight idempotency records will not match across the upgrade.
- Add the node-locality proof test: a session naming a loopback node sends its escalation traffic only to that node; a second listener standing in for a remote provider records zero hits.
- **Breaking (Rust):** the `/v1` wire types moved from the `types` crate into `bobby-browser-client`, which is now the single published Rust crate (`cargo publish` dry-run verified; `types` remains in the workspace as a `publish = false` re-export shim over the moved modules). crates.io publishing is now the one `bobby-browser-client` crate instead of the 25-crate ordered closure.
- TypeScript SDK source now carries JSDoc on the public surface (client, contracts, errors, events, intents, validators).
- Add `bobby init --emit <claude|zed|vscode|json>`: prints the MCP client config fragment for the host with `${VAR}` credential placeholders, never the secret. Add `skill/SKILL.md`, the agent skill package for driving the runtime.
- `bobby doctor` now runs a live MCP handshake (`initialize` + `tools/list`) against the stdio gateway and reports tool count and catalog bytes against the 128 KiB budget; a missing gateway is a warning, a failed handshake a failure.
- Fix `mcp-gateway` startup rejecting bootstrap credentials that carry `job:*` capabilities (the parse table predated the jobs API, so the stdio gateway could not start with a current `bobby init` file).
- Add MCP `toolset_select`: narrows `tools/list` to `explore`, `act`, `intent`, `verify`, or `full`. Default stays `full`, byte-for-byte the previous surface.
- Narrow phases cut the connect payload from ~130 KB to 42–74 KB; selecting a phase emits `notifications/tools/list_changed`.
- A phase changes what is advertised, never what is permitted: a hidden tool stays callable and capability gates remain the only authority.
- Add MCP `context_ask` (`page:read`): asks the retained page context where a described control is, returning a bound target and confidence, or nothing.
- **Breaking (idempotency digests):** `canonical_sha256` now sorts JSON object keys recursively before hashing, so a digest no longer depends on the order a client serialized keys in. It previously inherited ordering from `serde_json`'s default `BTreeMap` backend; any dependency enabling `serde_json/preserve_order` switched that to insertion order workspace-wide and two equivalent requests hashed differently, so a retry executed instead of replaying. Digests change once; in-flight idempotency records will not match across the upgrade.
- Add `crates/acp-gateway` with the ACP permission-escalation gate: a `session/request_permission` prompt is only sent for a capability the principal already holds but session policy gates, and approval never mints a capability.
- Add `crates/interface-conformance/tests/acp_permission.rs`, joining ACP to the conformance suite as a fourth adapter.
- **Breaking:** `executionPolicy.fingerprint` and `executionPolicy.humanize` now require the new `browser:fingerprint` and `browser:humanize` capabilities at session creation; a principal without them gets `missingCapability` and no session is created. `bobby init` bootstrap credentials include both, matching the `vision:assist` double-gate precedent.
- Document the MCP surface depth shipped in v0.3.1: per-tool `outputSchema`, `title` + `annotations`, the four `bobby://` resources, `artifact://` capture resources, the three working-loop prompts, and `notifications/bobby/event` + `notifications/tools/list_changed` push channels. Document `job:*` capabilities and the `browser:fingerprint` / `browser:humanize` gates in the capabilities concept page.
- Add a per-session context graph: `a11y_snapshot` results are retained per page and answer "where is the control described as X" with a bound target plus a confidence score.
- The graph invalidates on any command not on an explicit read-only allowlist, including `navigate` and `emulate`, which are `CommandClass::Replayable` yet change the page.
- A failed non-read-only command invalidates too, since a command that failed is not a command that did nothing.
- Truncated accessibility snapshots are not recorded, so the graph never reports a control absent when it was cut off.
- Ambiguous, partial, and below-floor matches answer nothing rather than guessing.
- Retained page context is dropped when its session is deleted, and bounded at 256 pages so an unclosed page cannot leak page text for the life of the process.
- Add a `[nodes.<name>]` config table: named, separately addressable nodes with `kind` (`vision`), `endpoint_url`, optional `token_env`, and `timeout_ms`. An unknown kind fails config load.
- Add `executionPolicy.visionNode`, naming which registered node a session escalates to.
- A named node that is not configured declines the escalation and never falls back to another node or to a process-wide provider.
- A `[vision]` endpoint with no `[nodes]` table is reachable as a node named `vision`; when both are set `[nodes]` wins and `[vision]` is ignored.
- Node locality is derived from the node's address, so a session bound to a loopback node keeps page material on the machine.
- **Breaking (HTTP):** `POST /v1/checkpoints` takes `evidenceRefs` (command ids, max 128) instead of `evidence`. The runtime resolves each id against its own journal and checks session ownership, so a caller can no longer author evidence for work it did not perform. Matches the MCP `checkpoint_save` contract. TypeScript SDK `CheckpointRequest.evidence` is replaced by `CheckpointRequest.evidenceRefs`.
- Add `crates/interface-conformance/tests/checkpoint_evidence.rs`: asserts no adapter accepts caller-authored checkpoint evidence, and that `evidenceRefs` is accepted on each.
- Add `executionPolicy.fingerprint` and `executionPolicy.humanize`, both deny-by-default. Fingerprint spoofing was a process-wide worker-factory setting; it is now per session. Humanized input timing was unconditional on the Firefox path; it is now per session.
- `PageRuntime` writes both flags to the worker on every lease, so a pooled worker never carries one session's opt-in into another's.
- Add `Evidence::Humanization` (`engine`, `actions`, `synthesizedMs`), emitted only when the session opted into `humanize`.
- Add `crates/mcp-gateway/tests/crate_boundary.rs`: fails if a schema names a type from `behavioral-engine`, `fingerprinting`, or `task-scheduler`, none of which carry `JsonSchema` derives.
- Add `networkLog` (MCP `network_log`): always-on bounded per-page network capture (512 entries) on Chromium (CDP Network events) and Firefox (BiDi network events), dumped as a HAR 1.2 artifact.
- Add broker job API `POST|GET|DELETE /v1/jobs` (`job:submit|read|cancel`) with in-process scheduler + optional `scheduler_journal_path`, and CLI `bobby jobs submit|status|cancel`. New bootstrap credentials include `job:*`; `bobby doctor` ensures the scheduler journal dir and warns when bootstrap lacks `job:submit`. Builtin handlers: `echo`, `sleep`.


## 0.3.1 - 2026-08-01
### Documentation
- Publish a new immutable docs artifact that names `@cavi-ai/bobby-browser`
  throughout (the `v0.3.0` release asset still referenced `@bobby-browser/sdk`).
- Carry forward post-`0.3.0` doc coverage already on main (`recovery_status`,
  MCP agent-surface catalog fixes, truncation ordinal notes) into `v0.3.1`.
### Browser primitives
- Add cookie primitives (`getCookies`, `setCookies`, `deleteCookies`) on Chromium (CDP Network) and Firefox (BiDi storage), exposed as MCP `cookie_get`/`cookie_set`/`cookie_delete` with `cookieState` evidence.
- Add `printToPdf` (MCP `pdf`) on Chromium (CDP `Page.printToPDF`) and Firefox (BiDi `browsingContext.print`), producing a verified `application/pdf` artifact.
- Add `handleDialog` (MCP `dialog`): waits for a JavaScript dialog with a bounded timeout and accepts or dismisses it, returning dialog type/message/action evidence. Chromium via CDP dialog events, Firefox via BiDi user prompts.
- Add `emulate` (MCP `emulate`): viewport size and geolocation overrides. Chromium via CDP Emulation, Firefox via BiDi viewport and geolocation override.

## 0.3.0 - 2026-08-01

### MCP surface

- Emit only the `$defs` each tool's arguments can reach. A principal holding the default `bobby init` capability set previously produced a `tools/list` past the 1 MiB frame cap, so the gateway answered `resultTooLarge` and no client could enumerate the surface.
- Expose one MCP tool per intent (`intent_locate`, `intent_fill`, `intent_complete_form`, `intent_submit_and_verify`, `intent_wait_for_state`, `intent_follow`, `intent_dismiss_obstruction`, `intent_extract`), each building its own command envelope. `command_execute` still accepts nested intent envelopes.
- Accept an optional `workflowId` on every envelope-minting tool and return it on the outcome, so `checkpoint_save` and `workflow_recover` are reachable without hand-built envelopes.
- Report rejected arguments as `data.pointer` (JSON Pointer) plus `data.constraint`, or as `malformedArguments` / `deadlineOutOfRange` / `invalidIdempotencyKey`, instead of an indistinguishable `"Invalid params"`.
- Add `credentialExpiresAt` to `runtime_info` and a `bootstrap-expiry` check to `bobby doctor` that warns under 7 days and fails once expired.
- Allow MCP `click`, `type_text`, and `upload_files` to consume accessibility-snapshot targets without also requiring a legacy CSS selector.
- Add MCP `recovery_status` (`recovery:read`) alongside `checkpoint_save` / `workflow_recover`.
- Guard MCP schema parity with schemars: `JsonSchema` derives on the wire types and tests that fail when the hand-bounded MCP tool schemas drift from the Rust command/evidence variants.

### Sessions, pages, and events

- Add `DELETE /v1/sessions/{id}`, MCP `session_close`, and TypeScript SDK `deleteSession` for session teardown.
- Add the `activatePage` primitive (MCP `page_activate`) to bring a page to the front on Chromium and Firefox.
- Add `GET /v1/events?stream=1` server-sent-event streaming with cursor frame ids and terminal gap frames.
- `GET /v1/mcp` now opens the streamable-HTTP SSE channel (keep-alive) instead of 405.
- Add `GET /v1/recovery/{workflow}`, MCP `recovery_status`, and TypeScript SDK `recoveryStatus` to inspect a workflow checkpoint and recovery receipts (`recovery:read`).
- Honor idempotency keys on session creation and checkpoint save, replaying retained results.
- Scope CDP-originated interface events to the authenticated principal.
- Report real uptime and in-flight command counts in runtime info.
- Add `listSessions` to the TypeScript SDK and stop rejecting checkpoints with recovery receipts.

### Packages

- Publish the TypeScript SDK as `@cavi-ai/bobby-browser` (replacing `@bobby-browser/sdk`).

### Semantic automation

- Add the `accessibilitySnapshot` primitive (MCP `a11y_snapshot`): a compact tree capped at 2048 nodes, from Chrome's full AX tree on Chromium and the companion extension's DOM walker on Firefox. Form controls include current value, description, required/disabled/read-only/invalid/checked state, autocomplete, and numeric bounds; sensitive values are redacted.
- Add command-ready semantic targets to actionable accessibility-snapshot nodes; duplicate role/name pairs receive deterministic tree-order ordinals without exposing DOM or browser IDs. Duplicate ordinals are computed on the full accessibility tree before `maxNodes` truncation, so retained targets keep globally correct ordinals.
- Carry snapshot targets into intents via `IntentHints.ordinal` and `intentHintsFromAccessibilityTarget`.
- Add verified `completeForm` intent (ordered uniquely named fill fields; no implicit submit).
- Add `FillValue` kind `checked` for reliable checkbox/radio semantic fills on Chromium and Firefox.
- Fill / completeForm verification fails closed on native HTML constraint validity (`required`, `pattern`, length, range, …) and retains the browser validation message in evidence.
- Add `expectedUrl` to `typeText` (all surfaces): typing fails before mutation when the page URL does not match, so agents cannot type into the wrong page.

### Extraction and vision

- Add the `extractStructured` primitive (MCP `extract_structured`): bounded page text plus the caller's JSON schema go to the configured provider, and the result is schema-validated and size-bounded before becoming `structuredExtraction` evidence. Gated on `browser:mutate`, `vision:assist`, session policy, and a configured provider.
- Plumb real screenshot bytes into vision escalation (`screenshot_bytes` on Chromium and Firefox workers); empty frames no longer reach providers.
- Add an HTTP vision-assist provider (`[vision]` config: https or loopback endpoint, bearer via env var) with response validation and fail-closed escalation.

### Firefox companion

- The Firefox native host treats a companion server silent for 45s as dead and reconnects, recovering from half-open connections left by killed processes.
- Recover stale Firefox companion attachments: a cycled companion connection now re-grants and retries once instead of failing every later action with `ConnectionClosed`; lease renewal re-grants dead attachments.
- Share one BiDi connection across runtime sessions on a Firefox profile (Firefox RemoteAgent accepts a single WebDriver session per browser).
- Keep prior attachment grants when issuing new ones, and renew attachment leases before expiry so sessions outlive the attachment TTL.
- Companion extension: merge attachment grants instead of replacing them, and retry terminal native-auth states after a bounded cooldown instead of stopping until a browser restart.
- Recover native-host descriptor publication from descriptor files leaked by killed processes.
- Log Firefox companion launch, pairing, and discovery failures as warnings.
- Add `bobby enroll-firefox-profile` for one-time Firefox companion pairing and selection output.
- Document Firefox companion setup and operations.

### CLI and startup

- Add `bobby doctor` setup checks and clap-based CLI help.
- Fail startup when the configured engine preference has no satisfiable worker registration.

## 0.2.1 - 2026-07-30

- Scope command outcome events to the authenticated principal across HTTP and MCP transports.
- Require session ownership for checkpoint creation and workflow recovery.
- Prevent workflow checkpoints from being rebound to a different session.
- Revalidate checkpoint session identity while holding the recovery lock.
- Default browser selection to exact Firefox without Chromium fallback.
- Bootstrap installed Firefox and its companion for championship runs.
- Support Playwright 1.62 bootstraps and repeated warmed client conformance runs.
