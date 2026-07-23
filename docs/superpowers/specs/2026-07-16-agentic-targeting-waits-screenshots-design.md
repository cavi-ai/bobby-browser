# Agentic Targeting, Waits, and Screenshots Design

## Objective

Implement Phase 6 of the browser automation runtime as the shared interaction foundation for reliable operation on real sites. The phase adds semantic target descriptions, deterministic command-scoped resolution, frame and open-shadow traversal, composable waits, and bounded screenshot evidence without weakening existing CSS-selector compatibility.

The runtime must fail closed when a target is missing or ambiguous. It may rank candidates for evidence, but it cannot silently guess unless a caller explicitly enables best-match selection for that command.

## Scope

Phase 6 includes:

- A stable `TargetSpec` contract for CSS, test IDs, labels, roles, accessible names, visible text, attributes, frame paths, and open shadow roots.
- One native target-resolution pipeline shared by all target-bearing browser commands.
- Deterministic candidate filtering, scoring, confidence, and ambiguity evidence.
- Command-scoped element handles with fingerprints used only for drift detection.
- A composable `WaitFor` primitive for element, content, URL, readiness, and network conditions.
- Explicit viewport, full-page, element, and clipped screenshots.
- Configurable cropped screenshots and compact DOM/accessibility fragments on ambiguity or action failure.
- Deterministic and real-Chrome proof across dynamic DOMs, frames, shadows, waits, and evidence containment.

Phase 6 does not include closed-shadow-root piercing, OCR or vision-only targeting, arbitrary JavaScript predicates, adaptive direct HTTP, CAPTCHA solving, stealth fingerprint manipulation, or broad network-policy enforcement.

## Architecture

The phase introduces a shared interaction engine between page command execution and Chromium:

- `TargetSpec` describes the target the caller means.
- `TargetResolver` evaluates a target specification against current browser state and returns exactly one command-scoped target or a typed failure.
- `CandidateRanker` produces deterministic scores and explanations without converting uncertainty into success.
- `WaitEngine` observes browser and DOM state and reuses the resolver for target-dependent conditions.
- `EvidenceCapture` produces bounded resolution traces, redacted candidate summaries, fingerprints, DOM/accessibility fragments, and screenshot artifacts.

Existing click, type, inspect, upload, popup, and download commands route through this interaction engine. The broker remains free of browser logic, Chromium does not own durable workflow state, and target handles never cross command boundaries.

The current selector field remains a compatibility fast-path. New clients use `target`. Supplying both is rejected unless the selector is exactly represented by the target's CSS constraint.

## Target contract

`TargetSpec` supports composable declarative constraints:

- `css`: exact CSS selector fast-path.
- `test_id`: configured test-ID attribute value.
- `role`: accessibility role.
- `accessible_name`: exact or normalized accessible name.
- `label`: associated form label.
- `text`: visible-text predicate with exact, contains, or bounded regular-expression matching.
- `attributes`: bounded equality constraints for stable attributes.
- `frame_path`: ordered frame specifications from the current page to the target document.
- `shadow_path`: ordered host specifications for open shadow-root traversal.
- `ordinal`: explicit zero-based candidate selection after deterministic filtering.
- `allow_best_match`: opt-in permission to choose the top candidate when uniqueness rules do not pass.

The contract does not accept executable predicates. Regular expressions have bounded length and compile limits. Attribute names and values, path depth, candidate counts, and evidence payloads have explicit limits.

An explicit ordinal is deterministic caller intent and may disambiguate otherwise equivalent candidates. `allow_best_match` is different: it permits scored selection and therefore must be visible in command evidence.

## Resolution pipeline

Every target-bearing command uses the following pipeline:

1. Validate the command, deadline, page ownership, target structure, and resource limits.
2. Traverse the explicit frame path. Same-origin and cross-origin frames remain separate contexts; the resolver never merges their DOMs.
3. Traverse requested open shadow roots. Closed roots return a typed failure.
4. Collect candidates using the cheapest selective signals first: CSS and test ID, then label, role/name, text, and attributes.
5. Filter candidates by attachment and the state required by the consuming command, such as visibility, enablement, or editability.
6. Rank candidates deterministically and retain the reasons for each score.
7. Apply ordinal, uniqueness, confidence, and best-match rules.
8. Return one command-scoped target or a typed failure with bounded candidate evidence.
9. Execute the browser action.
10. Re-observe the target or resulting page state and emit verification evidence.

Resolution always evaluates current browser state. Navigation, document replacement, frame replacement, and DOM detachment invalidate handles. A command may retry resolution within its deadline when the operation is replayable or reconciliable, but it cannot reuse a stale element handle.

## Candidate ranking and fail-closed behavior

Candidate ranking uses stable, explainable signals. Exact CSS/test-ID matches, exact accessible name, explicit label association, role compatibility, exact text, attribute equality, visibility, and interactability contribute independently. Partial text and structural proximity are weaker signals.

The resolver succeeds automatically only when one candidate remains after filtering or when the top candidate passes both the confidence floor and uniqueness margin. Otherwise it returns `TargetAmbiguous` with ranked summaries. Default thresholds are configuration values with conservative defaults and are recorded in evidence.

Candidate evidence contains page and frame identity, role, redacted accessible name or text, selected stable attributes, visibility and interaction state, score, and score reasons. Full HTML and unrestricted text are excluded by default.

When `allow_best_match` is true, the resolver may choose the highest-ranked candidate above the confidence floor even when the uniqueness margin fails. Evidence must mark the selection as caller-authorized best-match behavior.

## Command-scoped identity and drift

Resolved targets contain a live CDP handle plus a fingerprint derived from observable identity signals:

- Page and frame identity.
- Role and normalized accessible name.
- Stable attributes.
- Label association.
- Bounded DOM ancestry.
- Shadow-host path where applicable.

Fingerprints are evidence, not reusable authorization. A later command always resolves again from its `TargetSpec`. If a command re-observes a materially different fingerprint during execution or verification, it returns `TargetDetached` or `VerificationFailed` according to whether the original target disappeared or changed unexpectedly.

## Frames and shadow DOM

Frame traversal is explicit. Each frame-path component is itself a restricted target specification suitable for identifying an iframe. The resolver enters frames one component at a time and records each resolved frame in evidence.

Cross-origin frames are addressed through their CDP execution context. Cross-origin access does not grant DOM access outside what Chromium exposes to the automation session, and page/session ownership checks still apply.

Shadow traversal supports open roots only. Each shadow-path component identifies a host within the current document or open root. Missing hosts return `TargetNotFound`; a host with no accessible open root returns `ShadowRootUnavailable`. The runtime does not patch page prototypes, force roots open, or retain shadow handles across commands.

## Wait model

`WaitFor` is replayable and has a required bounded timeout. It supports:

- Element attached, detached, visible, hidden, enabled, or disabled.
- Text or value equals, contains, or matches a bounded regular expression.
- URL equals, contains, or matches a bounded regular expression.
- Document readiness at commit, DOM-content-loaded, interactive, or complete.
- Network quiet with an idle duration and maximum in-flight request threshold.

Target-dependent waits accept `TargetSpec` and invoke fresh resolution on each meaningful observation. The wait engine uses DOM mutation signals, lifecycle events, target events, and network events where available. It uses bounded adaptive polling only as a fallback and never uses an unbounded sleep loop.

Network quiet ignores configured long-lived channels and records which request classes were excluded. Navigation resets document-dependent wait state. Wait success includes the satisfied condition, elapsed time, observation count, and final browser evidence.

## Screenshots and artifacts

`CaptureScreenshot` is replayable and supports:

- Current viewport.
- Full page.
- Resolved element bounds.
- Explicit page-coordinate clip.

Element screenshots route through `TargetResolver`. Screenshot bytes are written beneath the session-private artifact root and returned as typed metadata containing artifact ID, media type, dimensions, byte length, SHA-256, capture mode, page ID, and timestamp. Callers do not receive arbitrary filesystem paths.

Normal screenshot capture is explicit. Ambiguity and browser-action failures may trigger configurable diagnostic capture. Diagnostic screenshots are cropped to the target region or bounded candidate union when available. DOM and accessibility fragments are compact and redacted before persistence.

Configuration defines retention tier, maximum screenshots per command, maximum dimensions, maximum encoded bytes, fragment limits, and whether automatic failure capture is enabled. Cross-session artifact access is denied.

## Errors and outcomes

Phase 6 adds stable public error codes:

- `TargetNotFound`
- `TargetAmbiguous`
- `FrameNotFound`
- `ShadowRootUnavailable`
- `TargetDetached`
- `WaitConditionTimedOut`
- `ScreenshotCaptureFailed`

Errors preserve their existing layer and retryability fields. Missing or ambiguous targets are non-retryable after the command deadline is exhausted, but the resolver may continue observing within the deadline for commands whose required target state has not yet appeared. Frame and shadow failures include the path component that failed. Screenshot failure does not convert a successful browser action into failure when the screenshot was diagnostic-only; explicit screenshot commands fail normally.

Boundary actions retain the existing checkpoint requirement. Target ambiguity occurs before the boundary action and therefore does not create an uncertain side effect. A driver failure after dispatch continues to return `NeedsReconciliation` under the existing recovery model.

## Security and resource containment

Targeting and evidence are declarative, bounded, and session-scoped. The runtime enforces:

- Maximum target, frame, and shadow path depth.
- Maximum candidates collected and ranked.
- Maximum regex length and evaluation work.
- Maximum wait duration and polling/event counts.
- Maximum screenshot dimensions, bytes, and artifacts per command.
- Redaction of configured secrets and sensitive attributes before observations or artifacts are exposed.
- Session ownership for pages, frames, screenshots, and artifact retrieval.

The runtime does not inject arbitrary caller JavaScript as part of resolution. Internal isolated-world helpers, if required for DOM observation, are fixed runtime code with bounded inputs and outputs.

## Testing strategy

Unit tests cover:

- `TargetSpec` schema validation and compatibility behavior.
- Candidate filtering, ranking, confidence floors, uniqueness margins, ordinals, and authorized best-match selection.
- Fingerprint stability and drift detection.
- Regex, depth, candidate, wait, and screenshot limits.
- Evidence redaction and artifact ownership.
- Every wait condition and typed failure mapping.

Deterministic browser fixtures cover:

- Duplicate labels and ambiguous buttons.
- Hidden, disabled, and detached decoys.
- Delayed rendering and DOM replacement.
- Nested same-origin and cross-origin frames.
- Open shadow roots and unavailable closed roots.
- URL and document lifecycle transitions.
- Short requests, long-lived channels, and network quiet.
- Element, viewport, full-page, and clip screenshots.

Real-Chrome tests prove semantic role/name and label targeting, frame and shadow traversal, fail-closed ambiguity, command-scoped re-resolution after DOM replacement, every wait category, screenshot hashing and containment, and cross-session denial.

The integrated phase workflow intentionally changes DOM structure between steps while preserving semantic identity. A CSS-only locator from the initial state must no longer identify the final control, while semantic targeting completes the workflow. The workflow also enters a frame and open shadow root, waits for dynamic state without fixed sleeps, captures an element screenshot, and verifies the resulting artifact metadata and session boundary.

## Completion gate

Phase 6 is complete only when:

1. Existing selector-only clients remain compatible.
2. All existing target-bearing commands route through the shared resolver.
3. Ambiguous targets fail closed by default with ranked, redacted evidence.
4. Explicit best-match and ordinal behavior are deterministic and auditable.
5. Handles are command-scoped and DOM replacement triggers fresh resolution.
6. Nested frames and open shadow roots work in real Chrome.
7. Every wait category passes deterministic and real-browser tests without fixed workflow sleeps.
8. Screenshot artifacts are hashed, bounded, session-private, and cross-session inaccessible.
9. Formatting, lint, workspace tests, graph impact review, and serial real-Chrome tests pass from a clean branch.
10. The integrated semantic-drift workflow passes and provides visible artifact proof.
