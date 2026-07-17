# Workflow I/O, Tabs, and Popups Design

## Purpose

Implement phase 5 of the browser automation runtime as a production vertical slice for workflow-heavy browsing. The phase adds uploads, downloads, tabs, popups, and richer form workflows to the existing command, journal, checkpoint, recovery, worker, and SDK architecture.

The phase is primitive-first: each browser capability receives a stable internal command, typed evidence, deterministic verification, and focused tests. One integrated live Chromium workflow is the release gate and must prove that the primitives compose into real agent operation.

## Scope

Phase 5 includes:

- Single-file and multi-file uploads through browser file inputs.
- Atomic click-and-wait download capture.
- Opening, listing, inspecting, and closing ordinary tabs.
- Atomic click-and-wait popup capture.
- Dynamic multi-step form operation using the new primitives and the existing command-bound checkpoint requirement for boundary submission.
- Session-private upload authorization and download storage.
- Stable page and artifact identities, typed evidence, durable command lifecycle records, and recovery-aware error classification.

Phase 5 does not add intent targeting, adaptive direct HTTP execution, SDK/MCP/CDP conformance, distributed scheduling, or broad security-policy completion. Those remain later phases in the approved runtime design.

## Architectural Approach

All capabilities extend the existing shared internal command and evidence protocol. No parallel workflow engine or browser-specific public API is introduced.

1. `types` defines commands, recovery classes, results, and evidence.
2. `PageRuntime` validates inputs, journals lifecycle transitions, leases the session worker, dispatches one command, and verifies observable postconditions.
3. `BrowserWorker` exposes engine-neutral methods. `ChromiumWorker` implements them using Chromium targets, browser events, and session directories.
4. `SessionManager` and `PageRuntime` retain stable `PageId` ownership for ordinary pages and discovered popups.
5. The configured artifact/download boundary owns session-private storage and produces metadata suitable for durable evidence.
6. `RuntimeService` exposes the same primitives without bypassing validation, journaling, or verification.

The broker remains free of browser logic. Chromium does not own durable workflow state. The calling agent continues to own strategy, while the runtime owns browser event correlation, waits, verification, typed outcomes, and evidence.

## Command Contracts

### Upload files

`UploadFiles` contains:

- `selector`: target file-input selector.
- `files`: one or more explicit source paths.

It is `Reconciliable`. Before browser execution, every source is canonicalized and checked by the file-policy boundary. The default phase-5 policy permits only configured upload roots and rejects missing files, directories, traversal, and symlink escape. Chromium receives canonical paths only.

Success requires browser-visible file input state to report the expected file names and count. Evidence contains the selector, canonical source references suitable for internal audit, redacted display names, sizes, and browser-observed count. File content is not copied into logs or observations.

### Click and wait for download

`ClickAndWaitForDownload` contains:

- `selector`: download trigger selector.
- `timeout_ms`: bounded wait.
- Optional expected filename predicate.

It is `Reconciliable`. Download observation is armed before the click so fast events cannot be missed. Chrome writes only beneath the session-private download directory. The runtime waits for completion, rejects path escape, verifies the final file exists, and computes its byte length and content hash.

Success evidence contains a stable `ArtifactId`, final filename, session-private path reference, media type when known, byte length, hash, source page, and download timing. Partial files never produce `Completed`.

### Page and tab operations

`OpenPage` creates a blank or caller-provided URL in the current session and returns a stable `PageId`. `ListPages` returns all live pages with URL, title, opener identity, and active/closed state. `ClosePage` closes one specified page and verifies its target disappeared.

Commands continue to target pages explicitly through `CommandEnvelope.page_id`; no mutable global "selected tab" is added. This avoids cross-agent races and makes parallel page work explicit. Closing a stale page, a page from another session, or the last live page when no replacement is requested fails with a typed validation error.

`OpenPage` and `ClosePage` are `Reconciliable`; `ListPages` is `Replayable`.

### Click and wait for popup

`ClickAndWaitForPopup` contains:

- `selector`: popup trigger selector.
- `timeout_ms`: bounded wait.
- Optional expected URL predicate.

It is `Reconciliable`. Target observation is armed before the click. Only a new target whose opener is the command page is accepted. The runtime assigns the popup a new stable `PageId`, registers it once in the page/session model, and returns its URL, title, browser target identity, and opener `PageId`.

## Browser Event Correlation

Compound event primitives follow one ordering rule:

1. Validate command and durable context.
2. Arm the relevant target or download observer.
3. Journal `Prepared` and `Executing` before the triggering click.
4. Trigger the browser action.
5. Correlate only events belonging to the session and opener page.
6. Wait for completion or timeout.
7. Verify the resulting page or file state.
8. Journal the typed terminal outcome and evidence.

Observers are bounded and removed on every success, error, cancellation, or timeout path. Events that predate the command are never accepted as its result.

## Page Identity and State

`PageId` remains the durable runtime identity; Chromium target IDs remain engine diagnostics. Each live worker maintains a bidirectional association between runtime page identity and browser page/target. Popup discovery registers through `PageRuntime`, not directly into caller-owned state.

Page listing is authoritative for the current worker. Recovery may recreate a checkpoint page with its prior `PageId`; newly discovered targets receive new IDs. A popup is never silently substituted for its opener.

Several workflows may share a session, but mutations on the same page stay serialized by the existing command path. Independent pages are explicit branches and may be operated independently once the scheduler phase supports parallel dispatch.

## File Containment

The browser configuration gains explicit upload roots and a download root. Each session receives a private download directory derived from its `SessionId`.

Upload validation:

- Canonicalize the requested source and each configured root.
- Require a regular file beneath an allowed root.
- Reject traversal, symlink escape, directories, missing files, and empty file lists.
- Pass only canonical paths to Chromium.

Download validation:

- Configure Chrome download behavior for the session-private directory.
- Sanitize suggested filenames and ignore site-provided path components.
- Resolve collisions deterministically without overwriting an existing artifact.
- Reject any completed path outside the session directory.
- Hash the completed file before returning evidence.

Session A cannot address Session B's upload authorization, download directory, artifacts, or pages.

## Verification and Outcomes

A driver response alone never constitutes success.

- Upload verification reads the file input's browser-visible file names and count.
- Download verification checks completion, containment, size, and hash.
- Open-page verification reads the created page URL and title.
- Close-page verification confirms the target no longer appears in the live page set.
- Popup verification confirms a new target, correct opener, optional URL predicate, and inspectable page state.
- Dynamic form submission continues to require a verified checkpoint bound to the exact boundary command and explicit postcondition evidence.

If a popup or download times out before a trigger effect is observable, the result is retryable. If the click may have triggered an externally meaningful effect but the runtime cannot correlate its result, the outcome is `NeedsReconciliation`. Invalid paths, stale page identities, cross-session handles, and containment violations fail before browser mutation.

## Deterministic Fixtures

The test site adds routes and browser behavior for:

- Single and multiple file inputs with visible file-name/count echoes.
- A dynamic second form step unlocked only after upload verification.
- A popup whose opener and content can be verified.
- An independently opened ordinary tab.
- A generated attachment with known filename, bytes, media type, length, and hash.
- Delayed popup and download triggers for observer-ordering and timeout tests.
- A boundary form submit with a stable result page.

Fixtures remain deterministic and local. Tests do not depend on third-party public sites.

## Testing Strategy

### Contract tests

Cover stable camel-case JSON, required fields, recovery classifications, page/artifact evidence, and backward compatibility for existing commands.

### Worker and page-runtime tests

Fake workers prove:

- Observers are armed before clicks.
- Fast events are not missed.
- Timed-out observers are removed.
- Popup page identities are registered exactly once.
- Stale and cross-session page IDs fail closed.
- Upload path rejection occurs before browser access.
- Download containment, filename normalization, byte count, and hash are verified before completion.
- Lifecycle journaling precedes browser mutation and terminal evidence is durable.

### Live Chromium tests

Focused ignored tests prove each primitive against installed Chrome. The final integrated live workflow must:

1. Create two isolated sessions.
2. Upload a known file and verify its visible name/count.
3. Complete a dynamic form step.
4. Open a popup, inspect it through its returned `PageId`, and verify its opener.
5. Open an independent tab, list it, inspect it, and close it without disturbing the form page.
6. Trigger a generated download and verify its filename, bytes, length, hash, and session-private location.
7. Create a command-bound checkpoint and submit the form.
8. Verify the final result while retaining correct page identities.
9. Prove the second session cannot access the first session's pages or downloaded artifact path.

## Quality Gates and Delivery

Each coherent task is implemented test-first and ends with:

- `cargo fmt --all -- --check`
- Clippy with warnings denied for the affected crates, or the full workspace when shared contracts change.
- Focused unit/integration tests.
- Relevant live Chromium proof when browser behavior changes.
- A clean commit containing no internal plan artifacts.

The phase completion gate requires:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- All focused live Chromium primitive tests.
- The integrated live workflow with explicit proof output.
- A graph-backed scope and impact audit.
- A clean branch pushed to a proper pull request against `main`.

Smoke tests alone do not satisfy completion.

## Implementation Order

1. Extend command, evidence, page, and artifact contracts.
2. Add session file paths and upload/download containment.
3. Implement upload execution and verification.
4. Implement page listing, opening, and closing.
5. Implement popup event correlation and registration.
6. Implement download event correlation, completion, and hashing.
7. Compose the integrated workflow fixture and live release proof.
8. Run the completion audit and publish the phase PR.
