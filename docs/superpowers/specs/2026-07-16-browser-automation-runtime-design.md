# Browser Automation Runtime Design

## Purpose

Build a production local browser automation runtime that lets agents operate public websites with ordinary browser capabilities. The runtime prioritizes workflow-heavy browsing, speed, containment, durable recovery, and agent-oriented operation.

Version 1 targets Chromium and runs as a persistent local Rust daemon. It provides stable browser primitives and higher-level intent operations through native SDK, MCP, and CDP-compatible control surfaces.

## Version 1 Outcomes

The first production milestone must execute long-lived, multi-step workflows involving dynamic forms, uploads, downloads, popups, tabs, and navigation transitions. It must preserve progress through verified checkpoints, resume when current state can be reconciled, and restart from the workflow entry point when safe recovery cannot be proven.

The target developer machine supports eight concurrently active workflows and approximately 32 warm or resumable sessions with bounded resources and explicit backpressure.

## Architecture

The runtime is organized around a single internal command and event protocol:

1. SDK, MCP, and CDP adapters translate external requests into internal commands.
2. The broker authenticates local clients, validates commands, applies quotas, and routes work.
3. The durable workflow coordinator tracks actions, checkpoints, retries, recovery, and evidence without deciding the calling agent's goals.
4. The adaptive page engine selects a direct HTTP document path or Chromium while preserving coherent session state.
5. The worker pool leases an isolated Chromium process to each active session.
6. The journal and artifact store persist workflow events, checkpoints, screenshots, downloads, and traces.

Browser behavior is accessed through a replaceable driver adapter. A mature Chromium driver may provide initial compatibility, while direct CDP implementations may replace performance-sensitive paths later without changing public APIs or workflow semantics.

The broker contains no browser logic. The workflow coordinator contains no model-specific planning logic. Browser drivers do not own durable workflow state. These boundaries keep components independently testable and replaceable.

## Public Control Surfaces

All control surfaces map to the same internal commands and receive equivalent behavior and evidence subject to client capabilities.

### Primitive operations

- Navigate and reload.
- Inspect DOM, accessibility, layout, and page metadata.
- Query and resolve elements.
- Click, hover, focus, type, press keys, select, and scroll.
- Upload and download files.
- Evaluate JavaScript under execution policy.
- Capture screenshots and traces.
- Create, select, close, and observe tabs, frames, popups, dialogs, and workers.
- Wait on explicit page, element, network, download, or workflow conditions.

### Intent operations

- Locate a target described by purpose or visible meaning.
- Fill a field by its semantic role.
- Submit a form and verify the resulting state.
- Follow a described link or control.
- Dismiss an obstruction.
- Wait for a task-relevant state.
- Extract validated structured results.

Intent operations compile into primitives and return their execution record. Target resolution prefers deterministic browser evidence: accessibility roles and names, labels, DOM relationships, visible text, geometry, and recent mutations. Heavier inference is optional and never required for primitive use.

## Session and Workflow Model

A session is the durable identity and isolation boundary. It owns browser storage, cookies, cache, permissions, proxy configuration, private filesystem workspace, pages, browser targets, action journal, and recovery metadata.

An active version 1 session owns a dedicated Chromium process. The public session API depends on `IsolationPolicy` and `WorkerPlacement` abstractions so later releases can add incognito-context density and configurable risk-tiered placement without changing callers.

A workflow is an agent-visible unit of progress within a session. It contains:

- A caller-provided goal and restart entry point.
- Current action and verified postconditions.
- Retry and recovery budgets.
- Verified checkpoints.
- Attempt lineage and completion state.
- References to retained evidence and artifacts.

Several workflows may share a session. Mutations within a shared page are serialized unless the caller explicitly creates independent branches.

## Adaptive Page Execution

The page engine chooses the cheapest path that can preserve correct, observable behavior.

Direct HTTP is eligible for safe document reads and downloads. It may use pooled connections, DNS caching, compression, conditional requests, and streaming. Before changing execution paths, the runtime synchronizes relevant cookies, headers, cache validators, redirects, and URL state.

Chromium remains authoritative for rendered state. Mutating interactions, JavaScript-dependent behavior, uploads, downloads with browser-visible state, popups, dialogs, ambiguous operations, and any operation whose equivalence cannot be demonstrated execute in Chromium.

## Command Lifecycle

Commands transition through `accepted`, `prepared`, `executing`, `verifying`, and a terminal or recovery state. Before execution, the coordinator records expected effects, postconditions, timeout, idempotency classification, and recovery strategy. A driver response is not sufficient for success; the coordinator verifies observable postconditions.

Actions have three recovery classes:

- **Replayable:** inspection, screenshots, scrolling, waits, and idempotent navigation.
- **Reconciliable:** typing, selections, tab creation, uploads, and form steps whose current effect can be inspected.
- **Boundary:** submissions, externally visible transitions, and other actions whose duplicate execution may produce a second effect.

Boundary actions require a pre-action checkpoint and explicit postcondition evidence. The runtime never silently retries an uncertain boundary action.

## Checkpoints, Resumption, and Restart

A checkpoint contains the durable workflow cursor, browser profile state, page and target identities, relevant invariants, inputs permitted for replay, and evidence required for reconciliation.

After interruption, the runtime restores the session and compares current state with checkpoint invariants. It resumes only when it can prove that continuation is safe. If state cannot be reconciled, the workflow restarts from its declared entry point and records lineage to the abandoned attempt.

Restart carries forward approved durable inputs and artifacts, but not assumptions about prior page state. If the effect of a boundary action is uncertain, the runtime emits `NeedsReconciliation` with evidence and waits for a caller decision rather than guessing.

Callers may supply idempotency keys and site-specific reconciliation probes when supported by a workflow.

## Security and Containment

Public web content is treated as hostile without reducing ordinary browser capability.

Each active session uses a dedicated Chromium process under the browser sandbox and an OS-level worker boundary. It receives a private profile, filesystem workspace, download area, IPC channel, and resource budget. It cannot access daemon credentials, journals, other sessions, or arbitrary host files.

Policy interfaces include:

- `NetworkPolicy`: public internet access with loopback, link-local, private-network, cloud-metadata, and daemon-control destinations denied by default.
- `FilePolicy`: explicit read-only mounts for uploads and session-private download destinations.
- `ExecutionPolicy`: JavaScript evaluation, permissions, external protocols, extensions, and native dialogs.
- `ResourcePolicy`: CPU, memory, process, storage, network, and time budgets.
- `SecretPolicy`: scoped delivery of sensitive values without exposing them in observations, logs, screenshots, or DOM snapshots.

Redirects, DNS changes, popups, service workers, WebSockets, and downloads are re-evaluated under policy. Policies are configurable per session, allowing deliberate grants for local networks, files, permissions, or broader execution without weakening global defaults.

The local command protocol uses authenticated transport, capability-scoped handles, strict schemas, bounded payloads, and redacted structured logging.

## Scheduling and Performance

The worker pool maintains pre-launched Chromium processes with no user state. Session assignment atomically attaches a fresh private profile and policy bundle. Idle sessions may be suspended by checkpointing durable state and terminating their process; resumption leases a clean worker and restores the profile.

Separate queues serve interactive actions, background reads, downloads, and maintenance. Interactive work receives latency priority. Per-session ordering prevents races, while global and per-origin backpressure prevents unbounded work.

Performance mechanisms include:

- Adaptive HTTP and Chromium execution.
- Warm process leasing.
- Incremental DOM and accessibility observations.
- Mutation-driven waits instead of fixed sleeps.
- Streaming observations and downloads.
- Batched commands for tightly coupled primitives.
- Content-addressed artifacts with bounded retention.
- Bounded queues and explicit overload outcomes.

Benchmarks separately measure daemon overhead, cold and warm process acquisition, navigation, interaction, extraction, checkpointing, recovery, download throughput, steady-state memory, and eight-workflow concurrency.

## Errors and Evidence

Every command produces a typed outcome:

- `Completed`: verified postconditions and evidence.
- `RetryableFailure`: classified failure with a safe retry strategy.
- `NeedsReconciliation`: execution may have occurred but its effect is uncertain.
- `PolicyDenied`: the required capability and policy decision.
- `ResourceExhausted`: constrained resource and retry timing.
- `Restarted`: unsafe resumption caused a new attempt.
- `Failed`: terminal classified failure.

Errors retain causal context across interface, broker, workflow, page engine, driver, browser, network, and site layers. Stable public codes are separate from implementation diagnostics.

Evidence may include before-and-after URLs, navigation chains, relevant DOM and accessibility fragments, target screenshots, network metadata, artifact hashes, browser-target events, timing, retries, target-resolution decisions, recovery decisions, and trace links.

Metrics and structured events are inexpensive defaults. Detailed screenshots, DOM snapshots, network bodies, and traces use configurable retention tiers. Sensitive values and authorization data are redacted before logs or agent observations are produced.

## Testing and Release Gates

Testing is layered:

- Unit tests cover schemas, policies, selector scoring, state transitions, recovery classification, redaction, scheduling, and retry budgets.
- Deterministic browser fixtures cover SPAs, redirects, frames, shadow DOM, popups, dialogs, downloads, uploads, service workers, WebSockets, dynamic forms, and navigation races.
- Fault injection terminates renderers, browser processes, workers, and the daemon at every lifecycle phase.
- Security tests cover cross-session access, host filesystem reads, private-network navigation, redirect and DNS policy bypasses, traversal, oversized payloads, secret leakage, and resource exhaustion.
- Chromium compatibility tests pin supported revisions and gate upgrades.
- Public-site canaries exercise representative workflows and detect real-world drift.
- Interface conformance tests run common scenarios through SDK, MCP, and CDP adapters.
- Performance tests cover cold and warm startup, eight active workflows, approximately 32 warm or resumable sessions, intent resolution, observation cost, adaptive escalation, downloads, checkpoints, and bounded memory.

The milestone passes only with live Chromium proof for dynamic multi-step forms, uploads, downloads, tabs, popups, checkpoints, forced crashes, verified resumption, and forced restart when recovery cannot be proven. Smoke tests alone do not satisfy the release gate.

## Explicit Version 1 Boundaries

- Chromium is the only production browser engine, but browser-specific behavior remains behind an engine interface.
- The runtime supports both primitives and intent operations.
- The calling agent owns goals and strategic decisions; the runtime owns understanding, targeting, execution, waits, retries, recovery, and evidence.
- Dedicated browser processes are the default and only production isolation placement in version 1.
- Distributed fleet scheduling, multi-host tenancy, Firefox, WebKit, and context-density placement are future additions, not version 1 deliverables.

## Implementation Order

The implementation plan should proceed as a thin vertical slice rather than completing crates in isolation:

1. Internal command, event, outcome, and evidence contracts.
2. Durable journal and workflow lifecycle.
3. Dedicated Chromium worker lease and primitive navigation/inspection/action loop.
4. Verification, checkpoint, crash recovery, and restart lineage.
5. Workflow-heavy uploads, downloads, tabs, popups, and forms.
6. Intent targeting over the primitive layer.
7. Adaptive HTTP path with state synchronization and conservative fallback.
8. SDK, MCP, and CDP conformance.
9. Security, compatibility, public-canary, and performance release gates.
