# Adaptive HTTP Path Design

## Purpose

Phase 7 adds a direct HTTP execution path for safe document inspection and ordinary downloads. The runtime will use it only when it can prove that the result is equivalent to the Chromium path. Chromium remains authoritative for rendered state, JavaScript-dependent behavior, mutations, browser-visible transitions, and every ambiguous case.

This phase is the first modular step toward broader adaptive execution. It must deliver measurable latency and resource improvements without changing command semantics, weakening session isolation, or making callers coordinate two engines.

## Scope

Direct HTTP is initially eligible for:

- Read-only document inspection whose requested result can be derived from the current page's bounded HTTP response.
- A new explicit URL-based download primitive that does not require a browser-visible mutation or JavaScript-triggered flow.

Direct HTTP is not eligible for:

- Uploads, form submissions, clicks, typing, selections, JavaScript evaluation, screenshots, layout, accessibility-tree, or rendered-state queries.
- Requests whose observable result depends on client-side rendering, service workers, browser permissions, extensions, dialogs, or browser-only authentication state that cannot be synchronized safely.
- Boundary or mutating commands, unsupported URL schemes, policy-ambiguous destinations, or operations whose equivalence cannot be demonstrated before execution.

Later phases may expand eligibility through new policy rules and conformance tests. They must not require callers to change command contracts.

## Public Contract Additions

The phase adds `DownloadUrlCommand { url, expected_content_type, max_bytes }` as a replayable primitive. It performs a safe read of an explicit URL and returns existing download artifact evidence plus adaptive execution-path evidence. Content-addressed persistence makes repeated successful transfers converge on the same durable bytes.

`ClickAndWaitForDownloadCommand` remains a Chromium boundary operation. It is not reinterpreted or routed through direct HTTP.

`InspectCommand` retains its public shape. Direct HTTP eligibility is limited to full-document text or HTML and explicit CSS-based selection against the static response document. Semantic targets, rendered visibility, values produced by script, layout, accessibility state, and browser-mutated DOM require Chromium. This narrow rule can expand only with differential conformance proof.

Execution-path evidence adds a stable enum with `directHttp`, `chromium`, and `chromiumFallback`, plus a non-sensitive reason code and synchronization snapshot version. Existing evidence remains valid for older consumers.

## Architecture

`PageRuntime` continues to own command lifecycle, journaling, recovery classification, and postcondition verification. It delegates eligible page work through a new `AdaptivePageEngine`:

```text
PageRuntime
  -> AdaptivePageEngine
       -> EligibilityPolicy
       -> SessionHttpState
       -> DirectHttpExecutor
       -> Chromium BrowserWorker fallback
```

The broker remains free of browser and session-state logic. Browser workers remain authoritative for browser execution. The adaptive engine owns transport selection but no workflow strategy or capability authorization.

### EligibilityPolicy

`EligibilityPolicy` is a pure deterministic classifier. It receives the typed command, recovery class, requested evidence, URL, session policy, and known page state. It returns one of:

- `DirectHttp`: the operation is safe and its equivalence requirements are explicit.
- `Chromium`: the operation is ineligible; route normally without reporting an error.
- `Denied`: policy or resource rules prohibit either path.

Eligibility is allowlisted. Unknown commands, content requirements, schemes, or policy states route to Chromium or fail closed; they never become HTTP-eligible by default.

### SessionHttpState

`SessionHttpState` produces an immutable, versioned request snapshot containing only the state needed for one eligible operation:

- Applicable cookies.
- User agent and accepted languages.
- Referrer and current URL where appropriate.
- Cache validators.
- Explicitly scoped authorization material when session policy permits it.

The snapshot contains no unrelated session data. Secret values are injected into the request at execution time and never appear in evidence, journals, metrics, errors, or artifact metadata.

State synchronization is bidirectional only where equivalence is explicit. Safe response cookies and cache validators may update the session state through a version-checked commit. Browser-visible navigation or DOM state is never fabricated from an HTTP response. If synchronized state changes concurrently, the result is rejected and the command is reclassified rather than merged optimistically.

### DirectHttpExecutor

`DirectHttpExecutor` performs bounded HTTP requests and returns a typed candidate result. It provides:

- `http` and `https` only.
- DNS resolution and destination-policy checks for every connection attempt.
- Redirect revalidation with a bounded redirect count.
- Connection, header, body, decompressed-body, duration, and download-size limits.
- Streaming decompression, charset decoding, document parsing, and downloads.
- Atomic writes into the existing session-private artifact store.
- Content hashes and normalized response metadata.

The executor does not decide whether a candidate result is equivalent and does not mutate browser state.

### AdaptivePageEngine

`AdaptivePageEngine` coordinates classification, state snapshots, execution, validation, state commit, fallback, and metrics. It returns the same public command outcomes and domain evidence used by Chromium, augmented with execution-path evidence.

Callers submit one command and receive one outcome. They do not select or reconcile engines themselves.

## Command Data Flow

1. `PageRuntime` validates and durably prepares the command using the existing lifecycle.
2. `AdaptivePageEngine` classifies the operation.
3. `Chromium` classifications dispatch directly to the existing browser worker.
4. `DirectHttp` classifications obtain a versioned session request snapshot.
5. `DirectHttpExecutor` performs the bounded request while rechecking DNS results and redirects.
6. The engine validates status, final URL, content type, decoded size, parsed result, state version, and command postconditions.
7. A proven-equivalent result commits permitted state updates and returns typed evidence marked `directHttp`.
8. An uncertain result discards partial observations and artifacts, records a fallback reason, and dispatches to Chromium only when the command remains safe to replay.
9. Terminal policy, resource, or unsafe-replay conditions return the existing typed denial, exhaustion, reconciliation, or failure outcome.

## Inspection Semantics

HTTP inspection operates on the decoded response document at the page's last committed URL, not a simulated rendered DOM. Eligibility requires either full-document text/HTML or an explicit CSS selector against the static document. It must not require semantic targeting, layout, accessibility, computed style, runtime mutations, input values produced by script, or client-side rendering.

The result is normalized into the existing inspection evidence shape. Additional execution evidence records that the source was `directHttp`, the final response URL, normalized content type, bytes processed, timing, state snapshot version, and content hash.

JavaScript shells, unsupported markup, misleading content types, incomplete decoding, or a missing requested target require Chromium fallback. A successful status code alone never proves equivalence.

## Download Semantics

`DownloadUrlCommand` is eligible when it is a safe read request to its explicit URL and does not depend on a click, browser event, service worker, rendered state, or browser-only permission flow.

Bytes stream to a temporary file inside the session-private artifact boundary. The artifact becomes visible only after size validation, completion, hash calculation, atomic persistence, and session ownership verification. Interrupted, oversized, redirected-to-denied, or invalid transfers remove all partial files.

Click-triggered and browser-event-correlated downloads remain Chromium operations through `ClickAndWaitForDownloadCommand`.

## Security and Containment

- Every resolved address and redirect destination is checked against `NetworkPolicy`. Loopback, link-local, private-network, cloud-metadata, daemon-control, and other denied destinations fail closed unless the session has an explicit grant.
- DNS changes are re-evaluated on each connection. A hostname allow decision never authorizes a later denied address.
- Request headers, cookies, redirects, bodies, decompressed bytes, duration, and output files are bounded.
- Cross-session cookies, caches, authorization material, temporary files, artifacts, and state snapshots are inaccessible.
- Authorization and secret values are redacted before any observation or diagnostic boundary.
- Partial HTTP results cannot update session state or become durable evidence.
- The adaptive engine cannot expand session capabilities or override execution, file, network, secret, or resource policy.

## Failure and Fallback Model

Adaptive execution distinguishes:

- `Ineligible`: route directly to Chromium. This is expected selection behavior, not a failure.
- `FallbackRequired`: HTTP began, but equivalence was not proven. Discard partial state and use Chromium only if replay is safe.
- `Terminal`: policy denial, resource exhaustion, corrupt transfer, state-version conflict that cannot be replayed safely, or an uncertain boundary effect.

Fallback is observable. Evidence records the eligibility decision, selected path, state snapshot version, timing, redirect chain without sensitive query material, bytes, content hash, and fallback reason. Callers still receive one stable `CommandOutcome`.

## Testing

### Unit and Contract Tests

- Eligibility allowlists and default-deny behavior.
- Recovery-class and requested-evidence classification.
- Cookie, header, referrer, cache, and secret scoping.
- DNS, redirect, byte, decompression, duration, and artifact bounds.
- State snapshot version conflicts and permitted commits.
- Fallback and terminal-error classification.
- Redaction and stable evidence serialization.

### Deterministic Fixtures

- Static HTML and selector inspection.
- Gzip and Brotli responses.
- Supported charset variants.
- Same-origin and cross-origin redirects.
- Cookie and cache-validator updates.
- Oversized headers, bodies, downloads, and decompression bombs.
- Interrupted and corrupt transfers.
- Misleading content types and JavaScript-dependent shells.
- Redirect and DNS transitions to denied networks.

### Differential Conformance

Eligible scenarios execute through both direct HTTP and Chromium. Tests compare normalized URL, selected content, decoded text, hashes, cookies, and relevant metadata. Any unexplained difference makes the scenario ineligible until equivalence is defined and tested.

### Live Runtime Proof

One workflow alternates between direct HTTP inspection, Chromium interaction, direct HTTP download, checkpointing, worker replacement, and verified continuation. It proves coherent session state, typed execution-path evidence, artifact ownership, and conservative fallback.

Existing real-Chrome vertical-slice, checkpoint-recovery, and agentic-interaction suites remain mandatory regression gates.

## Performance and Capacity Gates

The phase records direct-HTTP latency, Chromium fallback rate and reasons, transferred and decoded bytes, connection reuse, and active-request counts.

Eight concurrent eligible reads must remain within configured queue, memory, response, and time bounds. Against the same deterministic workload, the direct path must demonstrate lower median latency and lower Chromium activity than Chromium-only execution. Performance measurements are reported separately from correctness tests and cannot relax correctness or policy gates.

## Completion Criteria

Phase 7 is complete when:

- Eligible document inspections and ordinary downloads use direct HTTP and return equivalent typed evidence.
- Ineligible and uncertain operations route visibly and safely to Chromium.
- Mutating, rendered-state, and browser-event-dependent commands never enter the HTTP path.
- Cookies and permitted request state remain coherent across both engines without fabricating browser state.
- DNS and redirects cannot bypass destination policy.
- Partial transfers, state conflicts, secrets, and cross-session data fail closed.
- Differential, security, capacity, workspace, and live-Chromium regression gates pass.
