# Changelog

## Unreleased

### Fixed

- On Firefox, a command that misses its deadline fails alone; other sessions and runtime restarts keep the running Firefox.
- Restarting Firefox quits it with `browser.close` when possible, so site logins survive.
- Settling ignores animations, counters and carousels that never stop changing the page.
- Settling waits for the page to use a response that lands during the quiet window.
- On Firefox, network tracking, HAR and dialogs keep every event on pages that fire thousands of requests at once.
- Settling keeps waiting for in-flight loads on pages with thousands of requests or very long URLs.
- On Chromium, settling ignores requests started by a document the navigation replaced.
- Same-document navigations settle after 1 s without DOM changes.
- Snapshots show sign-in field labels and targets; only entered secrets are redacted.
- Runtime owner logs to `owner.log`, rotated past 1 MiB.
- A session keeps its pages when Firefox restarts onto a new companion build.
- `type_text` ending in Enter reports the landed page's full URL, query included, and its settled title.
- `type_text` ending in Enter detects a navigation made during the keypress without waiting 1 s.
- `intent_follow` completes once the destination page stops changing, so `postState` shows the rendered page.
- Settling after a navigation waits for the page's in-flight fetch and XHR requests, so `intent_follow` `postState` shows content the page fetches.
- A page that does not settle within 5 s reports the URL, query included, and title it shows at that point.
- Actions wait up to 5 s, and at most half the time left in the call, for a target that is not on the page yet before failing with `targetNotFound`.
- `navigate` and `workflow_start` return once the document has loaded, its scripts have run, and the page has stopped changing.
- A page title that discloses a credential is withheld in `navigate`, `page_list`, popups and settled-page evidence on both engines.
- A session start brings the enrolled Firefox to the companion build installed in its profile, and fails with a non-retryable error naming both builds when the mismatch survives one attempt.
- A page opened or bound by the runtime keeps its Firefox companion lease while the tab is blank or frame discovery fails.
- Firefox accessibility snapshots are sanitized and size-bounded; a rejected result reports `resultRejected` with its reason.
- Firefox page text is hidden only when it contains secret material or would be rejected by the extension channel.
- A shared-runtime gateway connection ends within 5 s of its peer leaving.
- An attached MCP or ACP gateway connection holds no per-principal in-flight permit; agents sharing one credential are bounded by `interface.max_connections` (default 64).
- A gateway refused by the shared runtime with a retryable error retries for up to 10 s, honoring `retryAfterMs`.
- A gateway still refused answers the host's `initialize` request with a JSON-RPC error carrying the runtime's reason before exiting 1.
- `session_close` removes the session from the registry even when its browser is dead, waiting at most 10 s on teardown.
- A form snapshot or page open on a session whose browser is gone returns `engineUnreachable` telling the caller to create a new session.
- A command on a Firefox session whose worker is closed tells the caller to create a new session.
- A wrongly shaped `kind` union is rejected with the allowed kinds and the chosen kind's required properties; `intent_follow` without a wait shows a valid example.
- An MCP connection that ends closes the sessions it opened and had not closed.
- `a11y_snapshot`, `workflow_observe` and `postState` carry each page text once.

### Added

- `bobby install --restart-runtime` stops this scope's running runtime owner after the install; `bobby install` ends with one line naming a still-running owner when it replaced the PATH `bobby`.
- `bobby runtime restart [--force]` and `make restart` stop this scope's runtime owner and start a new one, showing what is attached, asking before disconnecting agents, and saving the impact report under `runtime/restart-snapshots/`.

### Changed

- A settle that ends at the 5 s cap logs the page changes that kept it busy.
- `bobby runtime stop` shows what is attached, asks before disconnecting agents, refuses without a terminal unless `--disconnect-agents` is given, and saves the impact report first.
- `make install` runs `bobby install --yes` when stdin or stdout is not a terminal; `make install RESTART=1` passes `--restart-runtime`.
- `Formula/bobby-browser.rb` carries the v0.19.1 asset digests.

## 0.19.1 - 2026-10-02

### Fixed

- `bobby mcp-stdio` starts on a machine with no paired Firefox profile and runs on managed Chromium.
- The working directory does not select the config or the storage; the CLI and both gateways load the scope's `config.toml`, resolve its relative paths next to it, and the owner runs in the scope directory.
- A running owner takes the agent after the scope's files change; `bobby runtime status` reports the pending change and `bobby runtime stop` applies it.
- `mcp-gateway` and `acp-gateway` launched with the scope's bootstrap credential attach to the scope's running owner; a gateway with its own credential stays a standalone runtime.
- Unreadable data in a durable store does not stop the runtime from starting; skipped journal lines are counted by `bobby doctor` and an unreadable idempotency ledger is moved aside to `*.unreadable-<time>`.
- The Firefox companion binds an OS-picked port at every start and publishes it in the native-host descriptor; enrollment never fails on a taken port and stale `.pending-*` descriptor files are removed.
- `bobby mcp-stdio` and `bobby acp-stdio` report why the shared runtime refused a connection.
- Remembered site structure is written to disk when each outcome is verified.
- A completed `intent_complete_form` or `intent_extract` remembers every field it resolved, and a failed one counts the failure against the field that failed.
- Context flushes of one site run one at a time.
- Each runtime starts its own vision proxy on an OS-picked loopback port, which exits with the runtime.
- `bobby doctor` reports no `engine-satisfiability` failure when the scope's own runtime holds the enrolled Firefox profile, and no `vision-service` warning when the runtime runs its own proxy.
- `bobby doctor` warns `vision-service` for a loopback vision URL with no selected provider and nothing listening.
- `make install`, `make cli`, and `bobby install --cli` install `bobby`, `mcp-gateway`, and `acp-gateway` from one build and refuse a build missing either gateway.
- The repository carries no generated `openshell/` pack; `bobby openshell install` writes it into a project.
- The vision docs name the `vision-service` doctor check.
- The Firefox companion's native-host wrapper runs the installed `bobby`, so rebuilding the checkout does not affect pairing.
- The Firefox companion's native host keeps its pairing when the descriptor file changes without naming a new endpoint or owner.

### Changed

- `Formula/bobby-browser.rb` carries the v0.19.0 asset digests.
- The release version check accepts a Homebrew formula at an earlier release of the current or previous minor line.
- `bobby doctor` `companion-port` reports that the runtime binds a free loopback port at every start.

## 0.19.0 - 2026-10-01

### Fixed

- `intent_submit_and_verify` and a boundary `intent_follow` honor their `idempotencyKey` under `autoCheckpoint`; a retry replays the first outcome and saves no checkpoint.
- A request under an idempotency key whose earlier outcome is unknown is refused as unresolved with `reconciliationRequired: true`, including from a new session.
- MCP errors with `reconciliationRequired: true` carry a repair saying not to retry or mint a new key and to check for the effect first.
- ACP `contextAsk` and `contextNeighbors` return the MCP and HTTP shape with `hit`, `reason`, `nextStep`, and `pageDerived`; MCP `context_ask` and `context_neighbors` hits carry `hit: true`.
- Docs list the Python SDK (`bobby-browser` on PyPI) beside the TypeScript and Rust clients; only `bobby-browser-client` is on crates.io.

### Added

- Zed over ACP guide covering `agent_servers` setup for `bobby acp-stdio`.
- `bobby audit export --workflow <id>` writes a tar of one workflow's journal lines, checkpoint, and artifacts with a manifest of SHA-256 digests signed by a local Ed25519 key.
- `bobby audit verify` checks every digest and the signature, optionally pinned with `--public-key`; `bobby audit key` prints the public key.
- `bobby audit replay <bundle>` verifies a bundle and writes a self-contained HTML replay of each command with its phases, outcome, evidence, and screenshots.
- `bobby init --preset claude`, `codex`, and `openshell` mint host credentials at the preset's capability floor; `bobby doctor` warns when a credential holds a capability outside its preset.
- The capabilities page carries a generated preset matrix of every preset against every capability.
- Release builds sign the Firefox companion through addons.mozilla.org and ship it as `firefox-companion/bobby-firefox-companion.xpi` in every platform archive and as a release asset.
- `bobby install --companion` installs the signed build for the default scope; team and project scopes and checkout builds keep the unpacked sideload, which needs Firefox Developer Edition, Nightly, or ESR.

### Changed

- `tools/list` is smaller: `explore` is 26,776 bytes and `full` is 66,380, with ceilings of 28 KiB and 68 KiB; `tools/call` still validates against the full schemas.
- The companion manifest declares `data_collection_permissions` (`websiteContent`, `browsingActivity`).
- Managed Chromium remembers site structure across sessions and runtime restarts under the shared `managed-chromium` identity; `bobby context list --profile managed-chromium` shows the sites.
- Docker image: the context store lives at `/var/lib/bobby/data/context` on the data volume.
- `Formula/bobby-browser.rb` carries the v0.18.0 asset digests.

## 0.18.0 - 2026-09-30

### Added

- Local team and project scopes with one shared runtime owner for MCP and ACP clients, covering installation, Firefox pairing, profiles, credentials, context, storage, and jobs.
- `bobby --team <name> --project <name> runtime start`, `status`, and `stop`, plus `bobby runtime list`.
- `bobby firefox-start` opens the selected installed profile on an automatically assigned BiDi port for first-run pairing.

### Fixed

- Concurrent agent startup reuses the scope owner; disconnecting one client leaves the runtime available to others.
- Owner shutdown closes active gateway connections, drains their sessions, and releases ownership leases before the CLI reports stopped.
- Firefox recovery rejects missing or inaccessible profiles before launching, follows the profile's current BiDi endpoint, and terminates only processes that own the enrolled profile.
- Firefox companion startup and enrollment select a free loopback port when the configured port is occupied, and the native relay follows descriptor changes without restarting Firefox or pairing again.

## 0.17.0 - 2026-09-28

### Fixed

- Interrupted scheduler jobs require reconciliation after restart, and durable idempotency reservations survive runtime replacement.
- A dropped durable idempotency store releases its writer lock.
- Homebrew formula license metadata matches the repository's MIT license.
- Firefox workflow targeting and recovery handle detached controls, frame targeting, and companion reconnects.
- Vision readiness honors the configured proxy for remote providers and recognizes Ollama's full `/v1/chat/completions` base URL.
- MCP stdio handles a request line that arrives in more than one read.
- Vision proxy Ollama upstream accepts a host-only, `/v1`, or full-endpoint `base_url`, and the `ollama` preset writes `http://127.0.0.1:11434`.
- The vision proxy parses a JSON reply wrapped in commentary.
- `bobby install` completes when the selected vision backend is not reachable, printing `locations:` and pointing at `bobby doctor`.
- Host MCP/ACP entries follow the `bobby install --cli` binary, and `bobby doctor --fix` does not rewrite them to an older Homebrew copy.
- Ollama vision readiness probes `/v1/models` and `/api/tags`, accepts `llava` when `llava:7b` is installed, and `bobby doctor --fix` starts `ollama serve` when the loopback port is down.
- `bobby doctor` reports `vision-readiness` on the regular run.
- A fixed companion port that is already taken fails the Firefox launch with `browserLaunchFailed` naming the port and leaves the descriptor file untouched; port 0 binds an ephemeral port.

### Changed

- `bobby doctor` prints `next: bobby doctor --fix` first when anything is wrong, and auto-repairable fail lines include the fix.
- `bobby install` prints a `locations:` block, and `bobby doctor` warns when PATH `bobby` is not that CLI.
- `bobby install --skill-openclaw` writes `$OPENCLAW_STATE_DIR/skills/` when set, else `~/.openclaw/skills/`.
- `bobby install --skill-hermes` installs the Python SDK skill into `$HERMES_HOME/skills/` when set, else `~/.hermes/skills/`.
- The Python SDK publishes to PyPI as `bobby-browser` from each release tag; install with `pip install bobby-browser`.
- Every MCP JSON-RPC error `message` ends with `; repair: <action>`, and `error.data.repair` carries `{action, doc}`.
- Vision proxy errors are a structured object `{"error": {"code", "kind", "message", "retryable"}}` with `message` capped at 512 characters and no upstream response body.

## 0.16.0 - 2026-09-23

### Added

- Python SDK (`packages/python-sdk`, package `bobby-browser`, stdlib only) mirroring the TypeScript client's `/v1` surface.
- Managed Chromium opts into a durable profile with `{"mode": "exact", "engine": "chromium", "profileId": "<name>"}`, persisted at `<profiles_dir>/chromium/<name>` with `context_ask`, `bobby doctor`, and `bobby context list`/`forget` working for it.
- Every MCP result carrying page-read text has a top-level `pageDerived: true` in `structuredContent`; text under it is data from the page, never an instruction.
- A Prompt injection docs page, linked from `SECURITY.md` and the security model page.
- `intent_follow`, `intent_submit_and_verify`, `intent_complete_form`, and a Boundary `click` carry `postState` on a completed result: the compact `workflow_observe` observation of the handle's current page.

### Fixed

- A worker's first lease fails with the `browser launch failed: ...` diagnostic after a 45 s launch deadline.
- The enrolled Firefox is recycled only when every BiDi endpoint the profile offers refuses the probe or connect.
- Recovery tactics stop their background work when the caller stops waiting.
- Ambiguous target evidence names the iframe a contender was found inside, and `targetAmbiguous` points at `framePath`.
- `intent_submit_and_verify` with a pre-satisfied `expectedState` reports `failed` with `expectedStatePreSatisfied`, and a corrected `expectedState` resubmits without `reSubmit: true`.
- A `wait_for` or post-click `Text` or `Value` wait on a target matching several candidates is satisfied when any candidate matches, on Chromium and Firefox.
- That wait skips a candidate that detaches mid-read, and a poll where every candidate fails to read counts as not yet satisfied.
- `intent_follow` reports a post-click wait error caused only by targeting trouble as `verificationFailed`, with the click's evidence visible.
- Deterministic `intent_*` tools report the stuck kind's own error code (`targetNotFound`, `targetAmbiguous`, `obstructionSuspected`) when vision assist is off, denied, or unavailable.
- `visionAssistDenied` reports only from `extract_structured`, `intent_solve_challenge`, and `intent_detect_challenge`.
- `intent_follow` without `expectedDestination` or `expectedState` fails with a message naming the fix.

### Changed

- The default `explore` `tools/list` payload is under 32,768 bytes and `full` under 81,920; `cookie_get`, `dialog`, `download_url`, `screenshot`, `form_snapshot`, `inspect`, `wait_for`, `network_log`, `click_and_wait_for_download`, `context_ask`, `context_neighbors`, and `intent_detect_challenge` advertise in the `act`/`intent`/`verify` phases but stay callable from `explore`.
- Every tool description is one to two sentences.
- `intent_complete_form` fields accept an optional `revealedBy` hint naming a control to click before the field is resolved, so a sign-in gate with a revealed MFA field completes in three calls.
- `intent_complete_form`'s advertised schema names `revealedBy` on each field.

## 0.15.0 - 2026-09-19

### Added

- Provider health enforcement: `[vision].health_failure_threshold` (default 3) classifies a provider `unhealthy` or `degraded`, and `/v1/runtime` and `runtime_info` report `providerHealth`.
- `bobby doctor` evaluates `provider-health`, `slo-vision-latency-budget`, and the `[observability.slo]` objectives `vision_max_failure_rate` and `vision_min_acceptance_rate`.
- MCP `click_and_wait_for_download` clicks, waits for the download, and returns digest-verified artifact evidence; it needs `browser:mutate` and `file:download` and is advertised in `explore` and `act`.
- A tool result admitting a text-like download carries `downloadPreviews` with `{filename, text, truncated}` per file, `text` capped at 4 KiB.
- Screenshot evidence is also returned as an MCP `image` content block of up to 768 KiB base64.
- `intent_follow` accepts `expectedState` as an alias of `expectedDestination`; exactly one is required.
- Page-scoped commands report `Evidence::PageGeneration { pageId, generation }`.
- ACP `session/prompt` accepts `contextAsk`, `contextNeighbors`, `contextSite`, `checkpointSave`, `recoveryStatus`, and `workflowRecover`, and automation requests accept an optional stable `workflowId`.
- Vision escalation executes candidate-grounded `selectOne`, `selectMany`, `setChecked`, and `clear` control actions, and `spinbutton` and `listbox` are vision candidate roles.
- A file fill whose input does not resolve escalates to vision and uploads through `upload_files`.
- Vision escalation shows up to 10 near-miss candidates ordered by retained page context.
- `contextRankedVision` operational metrics appear in `runtime_info.operationalMetrics`.
- `InterfaceOperation::ALL` and `InterfaceOperation::as_str()` in `bobby-browser-client`.

### Changed

- Successful `intent_follow`, `intent_submit_and_verify`, and `intent_complete_form` results default to compact evidence; `evidenceDetail: "full"` returns everything and failures are never compacted.
- ACP automation replies are one JSON `session/update` chunk with `operation`, `sessionId`, `pageId`, `workflowId`, `attemptId`, and the full `CommandOutcome`.
- `bobby openshell install` takes `--agent codex|claude` (default `codex`), and `--agent-binary` is optional and replaces the preset.
- `intent_follow` is advertised in the default `explore` toolset.
- `screenshot`'s advertised `mode` is the `oneOf` of viewport, full-page, element, and clip shapes.
- MCP `initialize` instructions route verified link and control activation through `intent_follow` with `expectedState`.
- The HTTP vision backend rejects a `clickCandidate` for `fill`, `type`, and `extract` intents.
- TypeScript SDK contracts match runtime outcomes, including error codes `targetObscured` and `targetOutOfBounds`, reason `pageMutated`, and the newer evidence kinds.
- Firefox companion and registry failures map to distinct error codes: attachment, profile, pairing-code, revoked, expired-attachment, and credential failures are non-retryable `policyDenied`, and an unknown profile is `notFound`.

### Fixed

- TypeScript SDK `isRuntimeInfo` accepts `visionProposeBudgetMs`, `operationalMetrics`, and `providerHealth`.
- `workflow_observe` with `includeForms` reads the handle's current page, so it reads the opener after a followed popup closes.
- `recovery_status` and `workflow_recover` return `notFound` for a missing or unowned workflow.
- The MCP and CDP gateways revalidate connection authorization before resolving a method.

## 0.14.0 - 2026-09-10

### Added

- Docker image (`Dockerfile`, `docker-compose.yml`) and `deploy/docker/` config running `bobby serve` with managed headless Chromium as a non-root user, generating the bootstrap credential on first run.
- `bobby mcp-stdio` promotes verified intent outcomes into the shared context store when the engine selection carries a durable Firefox profile.
- `[vision].propose_budget_ms` sets a budget for one vision round-trip; `bobby doctor` warns when exceeded and `/v1/runtime` advertises `visionProposeBudgetMs`.
- `bobby mcp-stdio` dumps the operational metrics snapshot (counters and histograms only) to `BOBBY_METRICS_SNAPSHOT_PATH` when the host closes the session.
- A successful `click_and_wait_for_popup` through a `workflowHandle` rebinds the handle onto the popup; once it closes, a read-only call replays on the opener with `Evidence::PopupClosed` and a mutating call fails with that evidence and a repair naming the opener.
- `page_close {workflowHandle}` on the followed popup returns the handle to the opener.
- `runtime_info`'s output schema advertises `operationalMetrics` and `visionProposeBudgetMs`.
- `workflow_observe` takes `workflowHandle`, with the single-live-handle default and `workflowHandleDefaulted` evidence.
- `page_activate {workflowHandle, pageId}` activates the named page in the same session and rebinds the handle to it.
- `context_ask` and `context_neighbors` report a miss as `hit: false`, `reason: "notRemembered"`, `nextStep: "a11y_snapshot"`, on MCP and HTTP `/v1/context/ask`.
- An `intent_extract` field hinted at an accessibility-tree-only role misses with an `a11yOnlyRole` configuration marker and runs no vision escalation.
- A `targetAmbiguous` rejection names its contenders like `button "Submit" score=80` and ends with the narrowing repair.
- A schema violation on `oneOf`/`anyOf`/`enum`/`const` extends the repair with the variant-list fix.
- `intent_complete_form`'s field `name` and `intent_fill`'s `accessibleName` accept a `controlId` from `workflow_observe` (`includeForms: true`) or `form_snapshot`.
- `intent_complete_form` accepts a top-level `hints` when `fields` has exactly one entry with empty hints; otherwise it is rejected with `hintsPerField`.
- A handle-capable call naming no `workflowHandle` or scope ID resolves against the connection's one live handle, with `workflowHandleDefaulted` evidence; zero or several live handles get no default.

### Changed

- The public agent skill passes `workflowHandle` on later calls, with explicit ids as the repair path when the handle dies.
- Explore-loop tool descriptions name `workflowHandle` as the call scope.

### Fixed

- `intent_extract`'s advertised schema offers no top-level `hints` property.
- A page that closes while a command is in flight fails `notFound` immediately.
- A closed-session-shaped CDP error on a page command is probed first: a Replayable command retries once on the same lease, a mutating command fails `targetDetached` (retryable) with `transientTargetLoss` evidence, and an absent page fails permanently.
- Chromium clicks use one press/release sequence and retry once from target resolution when the error precedes the press acknowledgement.
- A click resolved inside an iframe or shadow root lands at the right point, with the target scrolled into view first.
- JSON-RPC `-32602` responses carry the rejection reason, pointer, constraint, and repair action in `error.message`.
- `intent_fill` and `intent_complete_form` on a file control fail with `intentActionMismatch` naming `upload_files` as the repair.
- A browser revive that could not reattach states that the transport was lost, and attaches `browserRevived` evidence naming the probe result, the reattach failure, and the original error.
- Starting a bobby instance reaps only browsers whose registered owner is confirmed dead, leaving a live bobby's browser untouched.
- A scope-less call to a handle-capable tool with zero or several live handles gets a repair naming the live count and pointing at `workflow_start`.

## 0.13.0 - 2026-09-04

### Added

- `bobby doctor` runs read-only health checks with `--json` output and a `next-action` recommendation over the command journal, scheduler journal, sidecar version, and store health.
- `bobby doctor --fix` rotates an expired bootstrap token.
- Firefox `wait_for` supports `networkQuiet` with the same idle, max-in-flight, and ignore filters as Chromium.
- Firefox live-process reattach reconnects the BiDi websocket without a new session, so typed values survive a transport drop.
- Rust SDK intent envelope builders (`locate_envelope`, `fill_envelope`, `submit_and_verify_envelope`, and others) mirror the TypeScript SDK.
- Rust SDK client methods `form_snapshot`, `checkpoint`, `recovery_status`, `recover`, and `artifact`, with artifact verification of media type, Content-Length, byte cap, and SHA-256 digest.
- MCP `boundary-once` guard refuses a second `intent_submit_and_verify` against a workflow whose Boundary submit already completed; `reSubmit: true` acknowledges it.

### Changed

- `RuntimeService::navigate` reports the command error message.
- `element_at_point` defaults to unsupported (`browserCommandFailed`).
- MCP `extract` default value and handle examples are formatted correctly in tool schemas.
- Advertised command-outcome schemas show field-level detail.
- MCP workflow handles are initialized before the first tool dispatch.

### Fixed

- The `ExpectedStatePreSatisfied` pre-check allows 2 s.
- The `boundary-once` ledger keys per control identity, so a workflow with two distinct Boundary submits works.
- Hints-less boundary submits are fail-open with no ledger entry.
- Candidate census handles elements serving non-string `innerText` or `value`.
- Firefox recycles a leaked BiDi session when `session.new` hits "Maximum number of active sessions" and retries.
- Firefox returns correct typed-value evidence and validates click bounds.
- Firefox CDP `inspect` with a text-only target resolves by page text.
- Firefox companion attach falls back for tab selection when `targetsDiscovered` is absent.
- Firefox native transport cleans up on disconnect.
- Firefox reads the document title correctly.
- CDP same-document hash-link clicks dispatch without waiting for a cross-document navigation.
- CDP `/json/list` provisions an auto-session when the gateway has none.
- CDP page title updates after navigate.
- CDP `Runtime.evaluate` is refused after navigate when the execution context is stale.
- CDP `Target.createBrowserContext` is refused as unsupported.
- `Formula/bobby-browser.rb` carries the v0.12.0 asset digests.

## 0.12.0 - 2026-08-31

### Added

- MCP `intent_detect_challenge` (read-only, advertised in `explore`) classifies a challenge without acting, and `intent_solve_challenge` drives the vision solve loop until cleared or `timeoutMs`; both require `browser:mutate`, `intent:execute`, and `vision:assist`.
- `bobby://intents` documents ten intents, including the captcha path.
- The `DetectChallenge` intent returns a typed `challengeDetection` with type, confidence, blocking, and optional region, or a clean-page answer; `bobby vision detect` runs it.
- A `SolveChallenge` tactic at rung 4 of the recovery ladder runs the vision solve loop in place and re-checks the original postcondition; a session without vision assist declines the rung.
- `session_create` and `workflow_start` accept `zigzagzig: true`, forcing fingerprint and humanize and routing page-bound commands through the recovery ladder; it requires `browser:fingerprint` and `browser:humanize`.
- The solve rung runs detection first, skipping the solve budget on a clean page and narrowing the prompt with a typed detection.
- TypeScript SDK `detectChallengeRuntimeCommand`, `solveChallengeRuntimeCommand`, `detectChallengeEnvelope`, `solveChallengeEnvelope`, and their intent contracts and default timeouts (15 s and 30 s).
- A worker reattaches to a live browser process when the CDP transport dies, preserving typed values, scroll, and cookies.
- `intent_submit_and_verify` with a `networkQuiet` postcondition returns a `submitSettlement` classification and value-free `formValidation` evidence.

### Changed

- The bundled `config.toml` sets the MLX vision provider to `mlx-community/Qwen3.5-27B-4bit` and includes a commented `[vision.providers.ollama]` example.
- `form_snapshot` output omits default-valued fields.
- `targetDetached` separates whole-browser transport loss from a stale element, and `notFound` documents the "browser page is not open" shape.
- The default MCP `explore` phase includes `intent_complete_form` and `intent_submit_and_verify`.
- `Evidence::ExecutionPath.path` names the strategy, `browser` or `browserFallback`.
- Successful `intent_complete_form` responses default to compact evidence; `evidenceDetail: "full"` returns the full per-field evidence.
- Download `savedTo` evidence echoes the caller-supplied destination, and an invalid `maxBytes` is rejected as `invalidRequest` with the configured range.

### Fixed

- A transport reset that reattached and replayed reports success.
- `submit_and_verify` with a descriptive `purpose` and no hints resolves the submit control by purpose, and an unresolved purpose stays ambiguous.
- A `fill` intent that cannot find its target escalates with a real candidate window and abstains cleanly.
- `completeForm` resolves every ordered field against current page state, so a field placed after its revealer completes in the same intent.
- A visible `aria-invalid="true"` control rejects network-quiet settlement.
- A Firefox profile relaunched on a different BiDi port is reachable again.
- The stdio gateway ends the shared Firefox BiDi session on exit.
- Browser-launch failures expose their allowlisted cause with an environment-shaped repair.
- `scripts/dev/firefox-start.sh` accepts pretty-printed endpoint files and a macOS-relaunched Firefox with a different pid.
- `Formula/bobby-browser.rb` carries the v0.11.1 asset digests.

## 0.11.1 - 2026-08-22

### Fixed

- Product documentation navigation includes every shipped page.
- `Formula/bobby-browser.rb` carries the v0.11.0 asset digests.

## 0.11.0 - 2026-08-20

### Breaking

- `FillValue` is removed; `fill` and `completeForm` intents use the `ControlAction` enum shared with `control_action`.
- Fill kinds are `setText` (`value`, `clearFirst` default true), `selectOne` (`value`), `setChecked` (`checked`), and `setFiles` (`paths`), plus new `selectMany` and `clear`.
- `activate` is rejected in fill and stays `control_action`-only.
- `control_action` `setText` accepts `clearFirst` (default true); `type_text` `clearFirst` defaults false.
- Interface version is `2026-08-19`; HTTP clients send the new `x-interface-version`.

### Added

- Qwen3.5-27B-4bit (`mlx-community/Qwen3.5-27B-4bit`) is the MLX vision default in `vision-proxy`, `bobby vision connect`, and the `bobby install` model list.
- `VISION_COORD_SPACE=normalized|absolute` overrides the mlx-vlm provider's coordinate-space detection.
- `network_log` reports `networkRecordingStarted` on the call that attaches the collector, and its description states that recording begins at the first call on a page.
- `bobby doctor`'s companion-port check names the listening process's pid and command on unix.
- `click` carries `dialogOpened` evidence when `alert`, `confirm`, or `prompt` opens during the click.
- `type_text` carries `typedControlKind` evidence.
- `form_snapshot` collects `[role=button]` elements and names a button from its own text.
- The DOM candidate collector maps implicit roles for headings, lists, list items, images, tables, rows, and cells.

### Changed

- Interface error messages carry the allowlisted diagnostic and the repair action, capped at 1024 bytes with the diagnostic truncated first.
- The candidate limit applies to the matching set after filtering, and an explicit `ordinal` skips the bound.
- `type_text` keys what the US keyboard layout can press and inserts the rest at the caret, so Unicode and newlines are typed.
- The browser-launch repair and `engineUnreachable` diagnostic name the Firefox companion bind (default `127.0.0.1:9876`) and the other runtime that may hold it.
- `visionAssistDenied` leads with the deterministic stuck reason and then names the closed gate.
- A duplicate in-flight JSON-RPC request id is rejected with a diagnostic and a repair.
- The advertised `executionPath.reason` enum carries a description.
- Every failing command outcome is logged at WARN with command, session, page, outcome, code, retryable, and message.
- The `targetNotFound` re-collect loop backs off from 25 ms to a 500 ms cap.
- A page opened by `page_open` or an agent workflow reports its navigated URL and an `interactive` ready state and is listed on its session.

### Fixed

- A `click` that opens `alert`, `confirm`, or `prompt` returns with `dialogOpened`, and `dialog` consumes a dialog that opened before it was called.
- `click` and keyboard dispatch bring the target page to front first.
- `type_text` against a non-editable target fails with `invalidRequest`.
- A screenshot clip with a negative origin, a non-finite value, or a dimension over `max_screenshot_dimension` is refused with `invalidRequest`.
- `a11y_snapshot` on a deep subtree returns nodes within its budget.
- One page's slow or hung call does not stall other pages in the session.
- A dead browser target triggers the revive path.
- A browser-aborted navigation (`net::ERR_ABORTED`) fails non-retryable and points at `download_url` and `click_and_wait_for_download`.
- Driver failures are retryable by code: malformed requests, missing targets, and policy refusals are not; transport, launch, internal, and timeout codes are.
- `httpResponseTooLarge` is a plain `failed`.
- An iframe hop resolves by DOM identity, so unnamed sibling iframes are told apart and a snapshot target passes back verbatim.
- Role matching treats `img` and `image` as one role.
- Accessible names are trimmed before comparison.
- `type_text` verification accepts an append, a checkable's checked state, and a select's committed option.
- The mlx-vlm provider rescales normalized `[0, 1000)` click coordinates onto the screenshot frame and unwraps list-typed coordinates.
- The mlx-vlm provider builds a system+user chat template when the processor supports it.
- `Formula/bobby-browser.rb` carries the v0.10.0 asset digests.

## 0.10.0 - 2026-08-18

### Added

- `Emulation.setDeviceMetricsOverride` and `Emulation.setTouchEmulationEnabled` are allowlisted over CDP; a scale factor other than 1, a non-portrait orientation, and `hasTouch` are refused with the reason.
- `[cdp].auto_session` (default true) gives a connecting CDP client with `session:write` and `page:write` and no session a session with a blank page.
- `[cdp]` section in the sample `config.toml`.
- `bobby doctor` reports a `cdp-port` check: serving authenticated discovery, free, or owned by another process.
- `cdp.listener.ready` startup log carries the CDP discovery endpoint and WebSocket base.
- `bobby token` prints the enrolled bootstrap bearer and refuses a redirected stdout without `--stdout`.
- `engineUnreachable` interface error code (HTTP 503) means the configured browser engine did not answer; it carries the browser-launch repair.
- `bobby doctor` reports `firefox-bidi-port-mismatch` as a failure when the CDP port is held by another service while an enrolled BiDi endpoint accepts nothing.
- Playwright 1.62.1 clients are supported over CDP.
- `click.modifiers` is an optional array of at most one each of `shift`, `ctrl`, `alt`, and `meta`, on Chromium and the Firefox companion; a duplicate modifier is refused, and a modified click that enters automatic download capture fails with `invalidRequest`.
- `SolveChallenge` intent, opt-in: it loops screenshot, vision proposal, act, reassess on a 750 ms poll until the model returns `challengeSolved` or the hint deadline elapses, failing closed on provider error, below-floor confidence, a disallowed action, and a closed vision gate.
- `bobby vision solve` submits a `SolveChallenge` over `/v1` in a vision-enabled session or `--session`/`--page`, with `--zigzagzig` adding humanized input timing and fingerprint spoofing.
- A solve outcome promotes to `SiteContext.challenges` as success and failure counters.
- `BrowserFlavor` fingerprint axis (Chrome default, Firefox); the companion sends a Gecko user agent on a Gecko engine.
- `ScreenResolution` carries `window_width` and `window_height`, so a profile presents a window smaller than the screen.
- `bobby doctor` reports a `companion-port` check: whether the enrolled profile's `companionBind` is free, serving the Firefox companion, or held by another service.

### Changed

- `scripts/dev/firefox-start.sh` puts the companion profile's remote-debugging endpoint on 9224; `BOBBY_FIREFOX_DEBUG_PORT` overrides it.
- CDP bind failures name the address, the Firefox 9222 overlap, and `--cdp-port`.
- `Target.createTarget` with no runtime session names the routes that open one (`POST /v1/sessions`, `POST /v1/pages`, MCP `session_create`/`page_open`, SDK).
- `bobby doctor` reports `cdp-listen` when CDP is disabled, naming the address `bobby cdp` would bind.
- The authenticated-CDP guide documents the session-and-page prerequisite, managed Chromium as the pairing-free engine, the surface's scope, and a per-client operation table.
- `/json/list` reports the URL and title the gateway last verified for each page.
- A rejected JSON request body names the offending field and position, never request values.
- `bobby doctor` distinguishes a refused BiDi connection from a live socket speaking another protocol.
- `v1_request_with_limits` lets a caller raise the `/v1` request timeout.
- `propose` prompts spell out the exact action JSON shapes and the `solveChallenge` guidance.

### Fixed

- Chrome launches with `--window-size` matching the spoofed screen and each page applies a matching device-metrics override; the init script reports `pdfViewerEnabled`, `navigator.share`/`canShare`, and `Notification.permission` `default`.
- A transient vision failure costs one solve attempt, only the deadline is terminal, and the deadline error reports the last transient failure.
- `solveChallenge` is click-only.
- The Python vision adapter drops non-action response fields and canonicalizes snake_case action kinds.
- `SolveChallengeHints::default()` keeps the 30 s timeout.
- Release binary builds work with `pnpm/action-setup@v6` and on Windows.
- `scripts/dev/firefox-start.sh` compares the endpoint's `ws_port` after stripping whitespace.
- A companion-server bind failure names the address and the operating system error.
- A solve screenshot that fails while the page is alive costs one attempt; a dead page fails every attempt and the deadline reports it.
- `Formula/bobby-browser.rb` carries the v0.9.0 asset digests.

## 0.9.0 - 2026-08-12

### Added

- `runtime_info` reports operational metrics: bounded intent-resolution, context-lookup, prefill, vision-provider, verification, retry, reconciliation, and admitted MCP-call counters with no request content or typed values.
- `control_action` reports the controls a form revealed, with kind, accessible name, and a verbatim-passable target, on managed Chromium and the Firefox companion.
- `intent_submit_and_verify` refuses a pre-satisfied text, element, or value `expectedState` with `expectedStatePreSatisfied` before clicking; url, document, and `networkQuiet` conditions skip the check.
- Candidate-grounded fill and extraction actions are index-only: typed values stay inside the runtime.
- `Evidence::Download` carries `savedTo`, the file's name below the configured downloads root, on the direct-HTTP, Chromium, and Firefox paths.
- `BOBBY-VISION/1` prompts carry a `HINT: role=<role>` row.
- The mlx-vlm provider prefills the JSON skeleton so completions carry coordinates.
- Persisted context recall breaks match-ladder ties by a control's validation record; an exact name match always outranks a fuzzy one.
- `bobby doctor` reports the configured vision timeout.

### Changed

- `initialize` negotiates the MCP protocol revision (2025-11-25, 2025-06-18, 2025-03-26, 2024-11-05), answering with the newest when the client's is unknown.
- The Firefox companion server binds and publishes its descriptor at `bobby serve` startup, and serve shutdown ends the shared BiDi connection.
- `workflow_observe` accepts a target, so an observation reads one region.
- The DOM candidate collector roles `ARTICLE` elements, so `a11y_snapshot` scoping resolves article subtrees.
- Runtime error detail reaches the operator interface.

### Fixed

- `a11y_snapshot` scoped to an iframe element returns the frame's content with hop-stamped targets.
- A command that fails because the browser process died revives once: a fresh browser launches, the page reopens at its last URL, replayable commands retry, and mutating commands fail with a revival note.
- `RUST_LOG` output from the stdio gateway goes to stderr.
- The Firefox companion's accessibility walk skips a hostile DOM node.
- Vision abstention fails closed.
- Firefox install finishes before vision readiness is reported.

## 0.8.0 - 2026-08-10

### Added

- Vision assist: `bobby install` configures a vision provider, `bobby vision connect --provider {openai,ollama,mlx}` writes the provider config, and `vision-proxy` takes `--upstream`, `--vision-base-url`, `--model`, and `--spawn-server`.
- `BOBBY-VISION/1` wire contract for `propose` and `extract` with local mlx-vlm, Ollama, and LM Studio backends, normalizing responses across coordinate shapes and degrading to a valid click.
- Vision escalation corpus collection behind `[vision] corpusDir`.
- `element_at_point` on managed Chromium reads the internal DOM channel, not the policy-gated `evaluate_javascript` path.
- NVIDIA OpenShell host: `bobby install --host openshell` and `bobby openshell install` write an `openshell/` pack with MCP client config, a policy sample, `policy-network.yaml`, a skill, and a README.
- `bobby openshell provision|revoke --sandbox <id>` mints or revokes one agent-scoped principal per sandbox and writes a 0600 injection env.
- `bobby init --emit openshell` prints the MCP fragment.
- `bobby openshell list|status|rotate` with non-secret `.status.json` sidecars.
- `bobby doctor` reports `openshell-pack`, `openshell-admin`, `openshell-companion`, `openshell-mcp-url`, and `openshell-sandboxes`, and warns on two or more sandboxes sharing one Firefox companion or a non-loopback cleartext MCP URL.
- `BOBBY_OPENSHELL_SECRETS_DIR` overrides the OpenShell secrets root.
- `a11y_snapshot` accepts an optional target and returns just that subtree, resolving through frame hops.
- The DOM candidate collector roles forms, dialogs, `main`, `nav`, and labelled regions as scope roots.
- Firefox companion `wait_for` supports Text, Value, and Document conditions.

### Changed

- The default `explore` phase advertises `click`, `click_and_wait_for_popup`, `type_text`, `control_action`, `upload_files`, `dialog`, and `download_url` with full schemas.
- `control_action` targets require only `role` and `accessibleName`; role matching is case-insensitive and `ordinal: 0` matches an omitted ordinal.
- `control_action` `selectOne`/`selectMany` and select fills accept an option's visible label as well as its value.
- Intent resolution and `a11y_snapshot` descend one level into same-process iframes on managed Chromium, up to 8 frames per gather.
- Whole-page `inspect` after a mutating command reads the live DOM, with `executionPath.reason: pageMutated`.
- Page-scoped text waits read live `document.body.innerText`.
- `click_and_wait_for_popup` defaults `autoCheckpoint=true`, accepts pinned `commandId`/`attemptId`, and registers `window.open` targets; `page_list` syncs untracked page targets into the session.
- A plain `click` on an anchor with a `download` attribute routes through download capture on managed Chromium and returns `Download` evidence.
- `workflow_start` failures carry `detail` with the error code and message.
- The advertised `WaitCondition` schema names every `kind` tag, required field, and enum.
- `a11y_snapshot` omits `InlineTextBox` leaves, and its description points at `toolset_select`.
- OpenShell `provision` revokes any prior principal for the sandbox id, rolls back on a failed env write, and defaults to the narrow `openshell` capability floor (`--capabilities-preset agent` for the full agent floor).
- The OpenShell sample policy denies `evaluate_javascript` and `job_*` and raises MCP `max_body_bytes` to 262 KiB.
- `bobby://intents` documents the `framePath` step shape with an example and the Firefox exact-CSS/test-id hop requirement.

### Fixed

- `page_open` on a session whose browser died invalidates the dead worker and retries once on a fresh one.
- `session_close` completes on a dead browser.
- Managed Chromium re-attaches a dead page handle on the next command, and a destroyed target unregisters the page so callers get `notFound`.
- CDP `oneshot canceled` and dead-target loss map to retryable `targetDetached`, and stale node ids map to `targetNotFound` with a fresh-snapshot repair.
- Boundary commands failing with `waitConditionTimedOut` or `verificationFailed`, or before reaching the browser, report `failed`; `needsReconciliation` is reserved for effects that may have landed.
- An intent post-state wait that times out after the boundary click landed reports a non-retryable `verificationFailed` stating the click landed.
- `intent_submit_and_verify` with a `networkQuiet`-only wait fails when `[aria-invalid=true]` markers remain.
- Intent `action_target` preserves `framePath` and `shadowPath`, and `locate` `NotFound` attaches the near-miss candidate set.
- Whole-page `inspect` over direct HTTP treats an empty-`<body>` SPA shell as `javascriptRequired` and falls back to the live browser.
- `inspect` denied by network policy degrades to the browser that has the page open; `download_url` keeps the hard denial.
- `[http]` accepts partial overrides.
- `bobby doctor` passes `BOBBY_BROWSER_CONFIG` into the MCP handshake child.
- `networkPolicyDenied` guidance names the loopback and private-destination cause and the `http.allow_loopback` / `http.allow_private_network` switches.
- `upload_files` policy errors name the resolved absolute roots and the gateway working directory.
- Empty-string target fields are rejected as `invalidRequest` on both engines.
- Protocol-layer `-32602` rejections carry `error.data.repair`.
- `scripts/dev/firefox-start.sh` launches Firefox directly and verifies it owns the process it started.

## 0.7.0 - 2026-08-07

- **Breaking (MCP surface):** `tools/list` defaults to the `explore` phase; `[mcp] startup_toolset` or `BOBBY_MCP_TOOLSET` selects `explore`, `act`, `intent`, `verify`, or `full` at connect, and hidden tools stay callable.
- **Breaking (bootstrap):** `bobby init`, `bobby install`, and loopback auto-init mint the agent preset without `authority:admin`; `--preset unrestricted` mints the operator floor.
- `bobby doctor` reports `bootstrap-preset`.
- MCP adds `workflow_start` and `workflow_observe` in every toolset phase, and `checkpoint_save` in Intent.
- Workflow handles are bounded to 64 bindings plus 64 concurrent reservations and expire on reinitialize or server-generation change.
- Streamable HTTP clients with the same authenticated principal share one MCP server lifecycle; distinct principals stay isolated.
- MCP `initialize` returns short `instructions` covering the explore phase, `toolset_select`, `error.repair`, `autoCheckpoint`, and `bobby://` recovery docs.
- MCP failures carry a machine-readable repair `{action, doc}` pointing into `bobby://failure-taxonomy`, and `needsReconciliation` always carries the never-retry repair.
- `http_wait` accepts `contains` and `maxBodyBytes`.
- `runtime_info`'s `capabilities` list reports `vision-assist` and `vision-provider` when configured.
- MCP `job_submit`, `job_status`, and `job_cancel` mirror HTTP `/v1/jobs`, with built-in handlers `echo`, `sleep`, `http_probe`, `http_wait`, and `http_fetch`; `bobby://job-handlers` documents payloads and `bobby doctor` reports `job-handlers`.
- Ollama joins the direct vision backends through `bobby vision-proxy --ollama --ollama-base-url` and `bobby vision connect --provider ollama`.
- `bobby vision collect` gathers vision proposals as JSONL training data.
- `bobby context forget` works against a store it just released, and an unusable lockfile reports its real cause.
- README and install docs state the Homebrew-core status; Unix release binaries are stripped, installation docs cover curl download of release assets, and `scripts/install.sh` is the one-liner installer (`BOBBY_VERSION`, `INSTALL_DIR`).
- Docs distinguish the public agent skill (`bobby install --skill`) from the internal Ghost / ZigZagZig recovery runtime.
- `context_ask` falls back to the persisted per-profile store, with `source` of `observed`, `persisted`, or `visionPromoted` on every answer.
- `context_neighbors` returns remembered form structure around a control.
- `context:read` capability over MCP and `/v1`, included in the bootstrap floors.
- `bobby context list` and `bobby context forget <site>`; `bobby doctor` reports store size and retention sweeps run on open.
- `IntentHints.accessibleName` takes an `a11y_snapshot` node's `target` verbatim in any `intent_*` tool; conflicting `nearText` is refused as `intentCompileFailed`.
- Idempotent retry works over MCP: identity is the command (schema version, session, page, command, vision consent), same key with a different command conflicts, and a replayed outcome carries no stamped `workflowId`/`attemptId`.
- `Evidence::Wait` carries `observed`, the value the condition matched on, bounded at 512 characters, for text, value, URL, and document conditions on Chromium and URL on Firefox.
- `recovery_status` accepts `sessionId` and answers with that session's recoverable workflows, newest first, capped at 32; exactly one of `workflowId` and `sessionId` is required.
- `intent_submit_and_verify`, `intent_follow`, and boundary `click` accept `autoCheckpoint`, default `true`, returning the minted `checkpointId`; pass `false` to author `invariants` or `replayableInputs`.
- `bobby install --host acp` and `bobby acp-stdio` mirror the MCP stdio entrypoint for ACP hosts.
- Release packages and `Formula/bobby-browser.rb` ship `bobby`, `mcp-gateway`, and `acp-gateway`, and `bobby doctor` checks sibling gateway presence on PATH.
- `bobby init --preset agent` mints a bootstrap without `authority:admin`, and heal never widens it.
- `bobby doctor` reminds that `vision:assist` needs `executionPolicy.visionAssist=true` and `javascript:evaluate` needs `executionPolicy.javascriptEvaluation=true`.
- Vision HTTP endpoints resolve through the node registry, and `bobby doctor` warns when both `[nodes]` and `[vision].endpoint_url` are set (`[nodes]` wins).
- `bobby vision connect` writes the loopback endpoint under `[nodes.vision]` and provider profiles under `[vision.providers.*]`.
- ACP `session/prompt` freeform-text parse errors include a structured JSON example, and MCP prompt descriptions carry one-line recovery tips.

## 0.6.0 - 2026-08-05
- Outbound ACP vision delegation: a workflow harness performs the vision work over ACP and bobby stores no provider credentials; `bobby vision connect --backend acp` writes the config and `bobby doctor` covers it.
- An isolated ACP vision harness that requests interactive permission fails closed.
- The session `visionNode` selector is accepted by MCP `session_create` and the TypeScript SDK validator.
- A rejected ACP authentication fails before the isolated child session is created.
- Firefox executes vision-assisted intents, with vision-selected coordinates executed through native BiDi pointer actions.
- The Firefox companion popup pairs and re-pairs the profile through the native host without the credential leaving the host; `bobby enroll-firefox-profile` remains for CI.
- `bobby install --companion` and `make firefox` upgrade a bobby-managed native host; operator-owned files still refuse.
- Version agreement checks cover `packages/firefox-companion/manifest.json`.
- One hung page call does not serialize other pages on a Chromium session or block close and terminate.
- The envelope deadline is enforced mid-flight; a hung call fails retryable `DeadlineExceeded`, with `timeoutMs` clamped to 300 s.
- `cookie_set` and `cookie_delete` complete without blocking the session.
- `download_url` works through the MCP gateway.
- Firefox `page_close` returns page evidence, so a successful close is not retried.
- Extraction and Firefox JavaScript-result truncation respect multi-byte characters.
- Concurrent leases on one session launch one browser, and concurrent `network_log` calls share one HAR collector.
- **Security:** `get_job` and `cancel_job` enforce job ownership; cross-principal access answers as absence.
- **Security:** `cookie_get` URL filtering matches on dot-boundary host suffix and real path prefix.
- **Security:** an abandoned idempotency permit releases its key.
- **Security:** companion reconnect credentials are stored as a SHA-256 digest and compared in constant time.
- **Security:** a live MCP SSE stream re-evaluates its `SubscribeEvents` guard on every poll, so a token rotation pauses event delivery.
- **Security:** the SSRF deny path covers IPv4-compatible IPv6, 6to4 and Teredo prefixes, IPv4 broadcast, and CGNAT `100.64.0.0/10`.
- **Security:** vision endpoint responses are size-checked up front and while streaming.
- `vision-proxy` bounds extraction JSON at 64 KiB.
- Checkpoint files are written 0600 on Unix.
- `authority.json` is synced before rename and an unparsable file is quarantined to `<path>.corrupt`.
- A corrupt artifact-ownership record is quarantined.
- `ArtifactStore` construction sweeps crash-orphaned staging directories.
- The session manager releases the worker before unregistering the session, and a failed release leaves the session registered.
- `events_read` does not journal a receipt per poll.
- `Page.getFrameTree` waits for the page lock.
- A job that finishes quickly leaves no abort handle behind.
- `bobby doctor`'s 15 s MCP handshake deadline applies to the read.
- The checkpoint store prunes uncontended per-workflow locks.
- The Firefox companion removes pending prompts when their context is destroyed and serializes cookie names and values safely.
- A `bootstrap.env` parse error reports the line number.
- The MCP stdio read loop bounds in-flight requests at 64.
- `replace_session`'s timeout error states that cleanup finishes in the background and the swap lands once.

## 0.5.1 - 2026-08-04
- **Breaking (MCP):** a command whose outcome status is not `completed` returns `isError: true`; `restarted` and `resumed` recovery decisions remain success.
- Boundary commands work over the flat MCP tools: `intent_*` tools and `click` accept optional `commandId`/`attemptId`, and every outcome echoes `attemptId`.
- The `fill_and_submit_form` prompt states the working order (snapshot, fill, pin ids, checkpoint, submit) and the exact `CompleteFormField`/`ExtractField` shapes.
- The static `bobby://` resources are readable by any authenticated principal; only live `artifact://` entries require `artifact:read`.
- `bobby://failure-taxonomy` documents the RPC-layer `InterfaceErrorCode` vocabulary, and the `errorCode` enum includes `targetObscured` and `targetOutOfBounds`.
- Tool annotations: `readOnlyHint` on `wait_for`, `intent_locate`, `intent_wait_for_state`, `intent_extract`; `openWorldHint` on `page_open`, `click`, `intent_follow`, `intent_submit_and_verify`.
- `tools/list` advertises shared `Id` schemas by `$ref`.
- A tag creates the GitHub Release before uploading assets, with the CHANGELOG section as its body; a version with no section fails the tag.
- `release-binaries` builds the documentation artifact on each release.
- npm publishes through the OIDC trusted publisher from `publish.yml` in the `production` environment.

## 0.5.0 - 2026-08-04
- `bobby install` can put `bobby` and sibling `mcp-gateway` on PATH (`~/.cargo/bin` when on PATH, else `~/.local/bin`), via the checklist, `--cli`, or `make cli`.
- `make` help lists every target in sections; `make firefox` builds and installs the Firefox companion only, and `bobby install --companion` neither wires Claude nor regenerates the bootstrap credential.
- The `bobby install` checklist uses up/down arrows and space.
- **Breaking (MCP):** `session_list` `structuredContent` is `{"sessions": [...]}`; `GET /v1/sessions` is unchanged.
- `bobby vision-proxy` is a loopback adapter that forwards to an OpenAI-compatible chat/completions upstream.
- Named `[vision.providers.<name>]` profiles and `[vision].provider` selection with OpenAI, Ollama, and LM Studio presets and custom OpenAI-compatible `base_url`; secrets stay in env via `token_env` / `api_key_env`.
- `bobby vision connect` writes loopback `[vision]` and a provider profile, interactively or with `--yes`.
- `bobby serve` and `bobby mcp-stdio` accept `--vision` / `--no-vision`, auto-spawning `vision-proxy` for a loopback endpoint with a selected provider and tearing it down on exit.
- `bobby doctor` warns on a missing `vision.provider` profile or upstream `api_key_env` and distinguishes loopback from external `vision-endpoint` hints.
- Docs cover connect, `--vision`, LM Studio (MLX), and custom OpenAI-compatible providers.
- Browser selection resolves through one function for `bobby serve`, the stdio gateway, and `bobby doctor`: `AUTOMATION_RUNTIME_BROWSER_SELECTION`, then the persisted enrollment, then the built-in default; a malformed source fails closed and `doctor` reports which resolved.
- `bobby enroll-firefox-profile` persists the selection atomically (0600 on Unix).
- `run_doctor` returns a structured `DoctorReport`.
- The stdio gateway loads `BOBBY_BROWSER_CONFIG` or `./config.toml` and composes its worker factory like `bobby serve`.
- `POST /v1/commands` emits `Retry-After` on a 503 `retryableFailure`.
- The `/v1` OpenAPI description is published in the docs artifact, stamped with the product and interface versions.
- The MCP stdio server handles `notifications/initialized` immediately followed by `tools/call`.
- Firefox companion operator popup: connection state, session policy, and a fingerprint toggle shown checked and disabled when the host owns the setting.
- `skill/SKILL.md` documents the gateway's config loading, engine resolution order, `doctor` source reporting, and that Chromium profiles are disposable while the Firefox companion attaches to a real profile.
- `bobby` builds on Windows.
- The npm publish step sets `NODE_AUTH_TOKEN` from `NPM_TOKEN`.
- Dependabot bumps across Rust, JS, and Actions are landed.

## 0.4.0 - 2026-08-03
- Internal npm packages move to the `@cavi-ai` scope (`@cavi-ai/bobby-firefox-companion`, `@cavi-ai/bobby-interface-conformance`); `@cavi-ai/bobby-browser` is unchanged.
- Only `bobby-browser-client` and `bobby-browser` are published crates, and one `v*` tag ships binaries, npm, and the crate.
- `publish-crates.yml` publishes on a `v*` tag, with a dry run as a pre-flight on every trigger.
- `scripts/check-version-agreement.py` runs in CI, requiring every crate, `package.json`, and path-dependency pin to carry the workspace version.
- Chromium humanized typing pastes through `Input.insertText` and paces clear-first backspaces 30 to 90 ms apart.
- `executionPolicy.humanize` works on Chromium: typing and pointer input are synthesized with paced key events, curved approach paths, and hover dwell, with `Evidence::Humanization` reporting action count and synthesized milliseconds.
- Capability parsing is one `FromStr` table on `types::Capability`.
- `bobby install` gains a Browser companions item that installs the Firefox companion and native-host wrapper and prints the remaining step; `--companion` and `--extension` are the non-interactive flags.
- `bobby mcp-stdio` is the MCP entrypoint agent hosts point at, loading the bootstrap credential itself so host configs carry no secrets.
- `bobby install` and `make install` set up an agent in one command: bootstrap credential, MCP config merge into Claude Code, Zed, and VS Code, and agent-skill installation, with `--host`, `--skill`, and `--yes` for non-interactive use.
- A session policy does not substitute for `vision:assist`, and `vision:assist` does not substitute for the session grant.
- The context graph records, per page, the command ids that produced evidence (bounded at 64), dropped on session close.
- `fingerprint_conformance` resolves Chrome from `BOBBY_CHROME_EXECUTABLE` before `CHROME_PATH`.
- `acp-gateway` speaks ACP schema v1 over stdio (`initialize`, `session/new`, `session/prompt`, `session/cancel`, `session/update`, `session/request_permission`) with structured prompts and permission prompts covering vision escalation only.
- **Breaking (idempotency digests):** `canonical_sha256` sorts JSON object keys recursively; digests change once and in-flight idempotency records do not match across the upgrade.
- **Breaking (Rust):** the `/v1` wire types live in `bobby-browser-client`, the single published Rust crate.
- TypeScript SDK source carries JSDoc on the public surface.
- `bobby init --emit <claude|zed|vscode|json>` prints the MCP client config fragment with `${VAR}` credential placeholders, and `skill/SKILL.md` is the agent skill package.
- `bobby doctor` runs a live MCP handshake against the stdio gateway and reports tool count and catalog bytes against the 128 KiB budget.
- `mcp-gateway` starts with bootstrap credentials carrying `job:*` capabilities.
- MCP `toolset_select` narrows `tools/list` to `explore`, `act`, `intent`, `verify`, or `full`, emitting `notifications/tools/list_changed`; narrow phases cut the connect payload from about 130 KB to 42 to 74 KB.
- A phase changes what is advertised, never what is permitted.
- MCP `context_ask` (`page:read`) asks the retained page context where a described control is and returns a bound target and confidence, or nothing.
- `crates/acp-gateway` sends a `session/request_permission` prompt only for a capability the principal holds but session policy gates, and approval never mints a capability.
- **Breaking:** `executionPolicy.fingerprint` and `executionPolicy.humanize` require the `browser:fingerprint` and `browser:humanize` capabilities at session creation; `bobby init` credentials include both.
- A per-session context graph retains `a11y_snapshot` results per page and answers "where is the control described as X" with a bound target and confidence.
- The graph invalidates on any command outside a read-only allowlist, including `navigate`, `emulate`, and failed commands.
- Truncated accessibility snapshots are not recorded.
- Ambiguous, partial, and below-floor matches answer nothing.
- Retained page context is dropped when its session is deleted and bounded at 256 pages.
- A `[nodes.<name>]` config table defines named nodes with `kind` (`vision`), `endpoint_url`, optional `token_env`, and `timeout_ms`; an unknown kind fails config load.
- `executionPolicy.visionNode` names the registered node a session escalates to; a node that is not configured declines the escalation without falling back.
- A `[vision]` endpoint with no `[nodes]` table is a node named `vision`, and `[nodes]` wins when both are set.
- A session bound to a loopback node keeps page material on the machine.
- **Breaking (HTTP):** `POST /v1/checkpoints` takes `evidenceRefs` (command ids, max 128) resolved against the runtime's journal with session ownership checked; TypeScript SDK `CheckpointRequest.evidence` is replaced by `CheckpointRequest.evidenceRefs`.
- `executionPolicy.fingerprint` and `executionPolicy.humanize` are per session and deny-by-default, and a pooled worker never carries one session's opt-in into another.
- `Evidence::Humanization` (`engine`, `actions`, `synthesizedMs`) is emitted when the session opted into `humanize`.
- MCP `network_log` captures bounded per-page network activity (512 entries) on Chromium and Firefox and dumps it as a HAR 1.2 artifact.
- Broker job API `POST|GET|DELETE /v1/jobs` (`job:submit|read|cancel`) with an in-process scheduler and optional `scheduler_journal_path`, and CLI `bobby jobs submit|status|cancel`; new bootstrap credentials include `job:*`, `bobby doctor` warns when bootstrap lacks `job:submit`, and built-in handlers are `echo` and `sleep`.

## 0.3.1 - 2026-08-01
### Documentation
- A new docs artifact names `@cavi-ai/bobby-browser` throughout.
- Docs cover `recovery_status`, MCP agent-surface catalog fixes, and truncation ordinal notes.
### Browser primitives
- Cookie primitives (`getCookies`, `setCookies`, `deleteCookies`) on Chromium and Firefox, exposed as MCP `cookie_get`/`cookie_set`/`cookie_delete` with `cookieState` evidence.
- `printToPdf` (MCP `pdf`) on Chromium and Firefox produces a verified `application/pdf` artifact.
- `handleDialog` (MCP `dialog`) waits for a JavaScript dialog with a bounded timeout and accepts or dismisses it, returning type, message, and action evidence.
- `emulate` (MCP `emulate`) overrides viewport size and geolocation.

## 0.3.0 - 2026-08-01

### MCP surface

- `tools/list` emits only the `$defs` each tool's arguments can reach, so the default `bobby init` capability set fits the 1 MiB frame cap.
- One MCP tool per intent: `intent_locate`, `intent_fill`, `intent_complete_form`, `intent_submit_and_verify`, `intent_wait_for_state`, `intent_follow`, `intent_dismiss_obstruction`, `intent_extract`; `command_execute` still accepts nested intent envelopes.
- Every envelope-minting tool accepts an optional `workflowId` and returns it on the outcome.
- Rejected arguments report `data.pointer` (JSON Pointer) and `data.constraint`, or `malformedArguments`, `deadlineOutOfRange`, or `invalidIdempotencyKey`.
- `runtime_info` reports `credentialExpiresAt`, and `bobby doctor` has a `bootstrap-expiry` check that warns under 7 days and fails once expired.
- MCP `click`, `type_text`, and `upload_files` accept accessibility-snapshot targets without a CSS selector.
- MCP `recovery_status` (`recovery:read`) sits beside `checkpoint_save` and `workflow_recover`.

### Sessions, pages, and events

- `DELETE /v1/sessions/{id}`, MCP `session_close`, and TypeScript SDK `deleteSession` tear down a session.
- `activatePage` (MCP `page_activate`) brings a page to the front on Chromium and Firefox.
- `GET /v1/events?stream=1` streams server-sent events with cursor frame ids and terminal gap frames.
- `GET /v1/mcp` opens the streamable-HTTP SSE channel.
- `GET /v1/recovery/{workflow}`, MCP `recovery_status`, and TypeScript SDK `recoveryStatus` inspect a workflow checkpoint and recovery receipts (`recovery:read`).
- Session creation and checkpoint save honor idempotency keys and replay retained results.
- CDP-originated interface events are scoped to the authenticated principal.
- Runtime info reports real uptime and in-flight command counts.
- TypeScript SDK `listSessions`, and checkpoints with recovery receipts are accepted.

### Packages

- The TypeScript SDK publishes as `@cavi-ai/bobby-browser`.

### Semantic automation

- `accessibilitySnapshot` (MCP `a11y_snapshot`) returns a compact tree capped at 2048 nodes with form control value, description, and state; sensitive values are redacted.
- Actionable snapshot nodes carry command-ready targets, with deterministic tree-order ordinals for duplicate role/name pairs computed before truncation.
- Snapshot targets carry into intents via `IntentHints.ordinal` and `intentHintsFromAccessibilityTarget`.
- `completeForm` intent fills ordered, uniquely named fields with no implicit submit.
- `FillValue` kind `checked` fills checkboxes and radios on Chromium and Firefox.
- Fill and completeForm verification fails closed on native HTML constraint validity and retains the browser validation message.
- `expectedUrl` on `typeText` fails the command before mutation when the page URL does not match.

### Extraction and vision

- `extractStructured` (MCP `extract_structured`) sends bounded page text and the caller's JSON schema to the configured provider and returns schema-validated, size-bounded `structuredExtraction` evidence; it needs `browser:mutate`, `vision:assist`, session policy, and a configured provider.
- Vision escalation receives real screenshot bytes on Chromium and Firefox; empty frames never reach providers.
- An HTTP vision-assist provider (`[vision]` config with an https or loopback endpoint and a bearer from an env var) validates responses and fails closed.

### Firefox companion

- The Firefox native host treats a companion server silent for 45 s as dead and reconnects.
- A cycled companion connection re-grants and retries once, and lease renewal re-grants dead attachments.
- Runtime sessions on a Firefox profile share one BiDi connection.
- Attachment grants are kept when new ones are issued and renewed before expiry.
- The companion extension merges attachment grants and retries terminal native-auth states after a bounded cooldown.
- The native host recovers descriptor publication from files leaked by killed processes.
- Firefox companion launch, pairing, and discovery failures are logged as warnings.
- `bobby enroll-firefox-profile` performs one-time Firefox companion pairing and prints the selection.
- Firefox companion setup and operations are documented.

### CLI and startup

- `bobby doctor` setup checks and clap-based CLI help.
- Startup fails when the configured engine preference has no satisfiable worker registration.

## 0.2.1 - 2026-07-30

- Command outcome events are scoped to the authenticated principal across HTTP and MCP.
- Checkpoint creation and workflow recovery require session ownership.
- Workflow checkpoints cannot be rebound to a different session.
- Checkpoint session identity is revalidated while holding the recovery lock.
- Browser selection defaults to exact Firefox without Chromium fallback.
- Installed Firefox and its companion are bootstrapped.
- Playwright 1.62 bootstraps are supported.
