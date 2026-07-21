# Native Browser Companion Design

## Purpose

Extend the browser automation runtime beyond its managed Chromium worker so agents can complete full workflows in a user's installed, authenticated browser. The companion must operate public sites and the user's authenticated sites with the same genuine browser profile, network, certificates, extensions, password managers, passkeys, and permissions that the user already relies on.

Success means completing the workflow, not merely issuing equivalent commands. The supported workflow surface includes account signup, login, logout, session restoration, navigation, semantic targeting, clicking, typing, complex form filling, selection, submission, file attachment, downloads, tabs, popups, frames, dialogs, permissions, and recovery after interruption.

The design begins with Firefox and preserves a modular engine contract for Chrome, Edge, Safari, and later genuine installed browsers. It does not depend on disguising Chromium as another engine. Instead, it uses the user's real installed engine and profile so browser identity remains internally coherent.

## Outcomes

This phase delivers:

1. A cross-browser companion protocol between the durable runtime and browser-specific adapters.
2. A Firefox-first WebExtension and local native bridge using WebDriver BiDi where available.
3. A complete interaction ladder spanning page content, browser APIs, native browser UI, and operating-system dialogs.
4. Durable attachment to real browser profiles, windows, tabs, frames, and popups without copying credential stores into the runtime.
5. Verified recovery across extension reloads, bridge failures, browser restarts, tab replacement, navigation, and engine switching.
6. Compatibility learning based on full workflow outcomes for each origin, browser, profile, and action family.
7. Release proof from real installed browsers and complete headed workflows, including authenticated flows.

The existing managed Chromium path remains available as one execution adapter. Callers use the same workflow, command, evidence, and recovery contracts regardless of the selected browser.

## Architecture

The runtime remains the durable coordinator and source of truth for workflow progress. The native-browser companion adds replaceable execution adapters below the existing command contract:

1. The runtime selects a compatible installed browser and requests an attachment through the local companion service.
2. The companion service authenticates the runtime, resolves the paired browser profile, and establishes an engine-specific control channel.
3. A browser extension observes page state and executes content-level operations through browser and extension APIs.
4. An engine adapter uses native automation protocols such as WebDriver BiDi for browser-level discovery, input, navigation, prompts, downloads, and lifecycle events.
5. A host adapter handles browser chrome and native UI through operating-system accessibility, keyboard, pointer, and dialog facilities when the browser protocol or extension cannot reach them.
6. The runtime verifies postconditions, journals evidence and recovery state, and decides whether to continue, reconcile, or restart.

The layers have explicit responsibilities:

- `CompanionCoordinator`: pairing, discovery, attachment leases, routing, capability negotiation, and connection health.
- `CompanionProtocol`: versioned commands, events, observations, evidence references, and typed failures shared by every browser implementation.
- `ExtensionAdapter`: DOM and accessibility observations, semantic target resolution support, content-script actions, page lifecycle events, and isolated-world execution.
- `EngineAdapter`: browser-specific native protocol behavior. Firefox uses WebDriver BiDi first; Chrome and Edge use their supported native protocols; Safari uses its supported automation and Safari Web Extension bridge.
- `HostAdapter`: operating-system interaction for browser chrome, native file choosers, permission surfaces, password-manager UI, passkey prompts, and other controls outside page content.
- `CompatibilityRegistry`: observed capabilities and workflow outcomes keyed by origin, browser identity, profile identity, action family, and runtime version.

Browser adapters do not own durable workflow state. The extension does not decide retry or release policy. The host adapter does not inspect or persist credentials. Each layer may be replaced without changing caller-facing workflow semantics.

## Pairing and Trust

The user pairs each installed extension with the local runtime once. Pairing creates a revocable, profile-scoped companion identity and an authenticated local channel. Browser discovery reports browser family, version, profile identity, windows, tabs, and negotiated capabilities without exporting passwords, cookies, passkeys, or raw browser storage.

The local bridge accepts only authenticated runtime connections. Commands are bound to a paired profile, attachment lease, workflow, and capability set. Message schemas are strict and versioned; payloads, rates, and artifact references are bounded. Revocation immediately prevents new commands and invalidates outstanding leases.

Extension code runs in an isolated world and exposes only the minimum bridge required by the protocol. Page content is untrusted input. Observations and errors are redacted before they enter the journal or agent context.

## Browser and Profile Selection

The runtime exposes selection preferences rather than hard-coding one engine. A request may name an exact browser/profile, prefer an engine family, continue in the current attached profile, or allow the runtime to choose from paired profiles.

Selection considers:

- Required workflow capabilities.
- Existing authenticated state and the user's chosen profile.
- Browser and operating-system support.
- Recent successful workflow outcomes for the origin and action family.
- Current browser availability and attachment health.
- Whether continuation requires the exact current profile or can restart in another genuine browser.

The selected installed profile remains authoritative for cookies, authentication, extensions, certificates, password managers, passkeys, browser permissions, and profile history. The runtime never creates a contradictory synthetic identity by partially copying profile state between engines.

An agent may attach to an existing tab or open a new tab in the selected profile. User-owned tabs outside the attachment lease remain untouched. The runtime tracks user interleaving and reconciles page state before its next mutation.

## Companion Protocol

The companion protocol maps the runtime's existing lifecycle—`accepted`, `prepared`, `executing`, `verifying`, and a terminal or recovery state—onto native browser execution.

Every command includes:

- Workflow, attempt, attachment, page, frame, and command identities.
- Expected effects and observable postconditions.
- Required capabilities and preferred interaction mode.
- Deadline, idempotency class, and recovery strategy.
- Opaque handles for approved secrets and files.

Every response includes:

- The adapter and browser identities that executed the operation.
- Lifecycle events and the final typed outcome.
- Before-and-after page, target, and navigation identities.
- Structured observations needed to verify the postcondition.
- Redacted evidence references and timing.
- Any challenge, browser UI, user interleaving, or compatibility signal encountered.

Commands and events are safe to correlate across reconnects. An adapter reconnect never implies that an uncertain command failed; the coordinator reconciles its observable effect before deciding whether to issue another mutation.

## Complete Interaction Surface

The companion must support the runtime's complete browser workflow contract:

- Inspect DOM, accessibility, layout, visible content, page metadata, and recent mutations.
- Resolve targets by role, accessible name, label, text, DOM relationship, geometry, and stable site-specific evidence.
- Click, double-click, hover, focus, type, clear, press keys, select, drag, scroll, and submit.
- Fill text, email, telephone, date, time, numeric, rich-text, checkbox, radio, select, combobox, and dynamically generated controls.
- Attach one or more approved files through page inputs, drag-and-drop surfaces, or native file dialogs.
- Navigate, reload, follow redirects, and observe history transitions.
- Create, select, close, and observe windows, tabs, popups, frames, and nested frames.
- Handle JavaScript dialogs, browser permissions, downloads, native prompts, password-manager surfaces, passkeys, and MFA flows available in the local profile.
- Wait on page, element, navigation, network, download, dialog, or workflow-relevant conditions without fixed sleeps as the default.
- Capture screenshots, target evidence, downloads, traces, and verified checkpoints.

Intent operations continue to compile into these primitives. The runtime reports success only after the declared workflow postcondition is observed.

## Interaction Ladder

For each action, the adapter chooses the highest-fidelity available interaction path:

1. Native engine input and lifecycle control, such as Firefox WebDriver BiDi actions and events.
2. Browser extension APIs and isolated content scripts for observations or operations the native protocol cannot express.
3. Operating-system accessibility, keyboard, pointer, and native dialog control for browser chrome and host UI.

Native engine input is preferred for ordinary user interaction. Direct page-JavaScript activation such as calling an element's `click()` method is a compatibility fallback, not the default, because it can skip hit testing, focus, pointer sequences, browser behavior, and site handlers that expect genuine input.

The adapter records which path was used so compatibility decisions and failures remain explainable. Moving down the ladder does not relax postcondition verification.

## Authentic Browser Identity and Compatibility

The companion preserves the browser's genuine, coherent identity rather than attempting to mask one engine as another. Engine, browser version, operating system, graphics behavior, fonts, codecs, locale, timezone, screen characteristics, media capabilities, request headers, TLS behavior, installed extensions, and profile history remain those of the real selected browser and host.

The runtime avoids page-visible shims that create contradictory signals. It also avoids replacing ordinary native interaction with synthetic script events when a genuine input path is available.

Compatibility is measured at workflow level. The runtime records success and classified failure for each origin, browser identity, profile, workflow shape, and action family. It uses those observations to:

- Reuse a previously successful adapter and interaction path.
- Select Firefox, Chrome, Edge, Safari, or another paired installed browser when the current engine is incompatible.
- Detect challenge pages, blocked navigation, unexpected redirects, disabled controls, missing capabilities, and behavioral incompatibilities.
- Compare complete workflow outcomes against the managed Chromium adapter.

A compatibility record is evidence, not a permanent assumption. Browser upgrades, site changes, profile changes, runtime changes, and stale observations trigger revalidation.

## Authentication, Secrets, and Files

The browser profile is authoritative for authenticated state. Existing cookies, password-manager sessions, client certificates, passkeys, and browser permissions remain inside the browser and their owning system services.

When a workflow supplies a secret, the runtime passes an opaque, capability-scoped handle. The resolved value is delivered only to the intended input path and is excluded from DOM snapshots, logs, screenshots, traces, errors, and agent-visible observations. The runtime does not journal raw cookie databases, password vault contents, authentication headers, or plaintext credentials.

Files use opaque handles bound to the workflow and attachment lease. The host adapter grants only the selected files to the browser input or native chooser. Evidence records metadata and content hashes according to retention policy, not unrelated host paths or directory contents.

Password-manager, passkey, MFA, and permission experiences vary by browser and host. The engine and host adapters expose their state as typed workflow events, interact when supported, and allow user completion without losing the workflow cursor. After user completion, the runtime observes and verifies the resulting authenticated state before proceeding.

## Recovery and Engine Switching

Recovery covers extension reload, local bridge restart, WebDriver BiDi disconnect, runtime restart, browser crash or update, browser restart, tab replacement, popup creation, navigation, frame replacement, and user interleaving.

Each verified checkpoint records:

- Workflow cursor and attempt lineage.
- Browser, profile, window, tab, frame, and attachment identities.
- Current URL and observable invariants.
- Approved inputs, secret handles, and file handles permitted for replay.
- The last verified boundary and retained evidence.
- The adapter capabilities and interaction path used.

After interruption, the coordinator re-discovers the browser and live targets, then compares them with the checkpoint. It chooses one of three outcomes:

- `Resume`: the same live state satisfies checkpoint invariants.
- `Reconcile`: the state changed, but the effect of the last action can be determined safely.
- `Restart`: continuation cannot be proven safe, so the workflow starts again from its declared entry point with a new attempt lineage.

Duplicate-sensitive actions—including signup completion, final form submission, purchases, messages, uploads that create remote records, and other externally visible transitions—are never repeated merely because the connection failed. The runtime first inspects the expected result, account state, confirmation view, receipt, server-visible identifier, or workflow-provided reconciliation probe.

If the selected engine cannot complete the workflow and continuation cannot be reconciled, the runtime starts the workflow over from its declared entry point in another compatible paired genuine browser when one is available. It carries forward the goal and approved inputs, secret handles, file handles, evidence, and attempt lineage, but it does not assume that page state or authentication from the previous engine exists in the new profile. When no compatible paired browser is available, it returns a typed terminal failure with the evidence needed to add or pair one.

## Errors and Evidence

Companion operations use the runtime's stable outcomes and add precise diagnostics for native-browser failure domains. Relevant classifications include:

- Companion not paired, revoked, unavailable, or version-incompatible.
- Browser, profile, window, tab, or frame unavailable.
- Required engine, extension, host, or operating-system capability unavailable.
- Native input rejected or target obscured.
- Browser chrome or native dialog not reachable.
- Authentication, permission, MFA, or passkey interaction awaiting user completion.
- Challenge, rate limit, site drift, or engine-specific incompatibility detected.
- User interleaving changed the expected state.
- Command effect uncertain and reconciliation required.
- Recovery failed and workflow restarted with lineage.

Evidence may include redacted DOM and accessibility fragments, screenshots, URLs and navigation chains, target geometry, adapter and browser identity, interaction path, lifecycle events, native-dialog state, downloads, timing, retries, reconciliation decisions, and artifact hashes.

Evidence collection must not expose passwords, passkeys, authentication tokens, cookie stores, unrelated tabs, or unrelated host files. A failure to obtain detailed evidence does not authorize a duplicate-sensitive retry.

## Security Model

The companion expands execution into the user's real browser profile, so its trust boundary differs from an isolated managed worker. Security controls protect the user and other local state while preserving the complete approved workflow surface.

- Pairing, attachment, secret, file, and action capabilities are explicit, scoped, expiring, and revocable.
- The extension communicates only with its authenticated local bridge and validates every message schema and origin.
- Commands are limited to the selected paired profile and leased targets.
- Host automation is constrained to the selected browser process and the native surfaces required by the active workflow.
- Page content cannot invoke the local bridge, mint capabilities, read companion secrets, or control other tabs.
- Observations, screenshots, traces, and errors pass through redaction before persistence or agent exposure.
- Downloads and file attachments use approved opaque handles and configured retention.
- Bounded payloads, queues, timeouts, and evidence budgets prevent page-driven resource exhaustion.

Policies remain configurable through the existing runtime model. The companion enforces the effective workflow policy consistently across extension, engine, and host adapters.

## Testing and Release Gates

Release proof uses real installed browsers and profiles in addition to deterministic fixtures. Headed execution is the primary compatibility proof because native browser UI, extensions, permissions, password managers, passkeys, and host dialogs are part of the product surface.

The workflow suite includes:

- Account signup and account-state verification.
- Login, logout, authenticated navigation, session restoration, and expired-session recovery.
- Multi-page and dynamically changing complex forms.
- File attachment through page controls, drag-and-drop, and native choosers.
- Final submission with duplicate-sensitive reconciliation.
- Popups, tabs, nested frames, downloads, dialogs, permissions, and prompts.
- Password-manager, passkey, and MFA flows where available in the local test profile.
- Extension reload, bridge crash, runtime restart, browser restart, browser update, tab replacement, and user interleaving.
- Restart in another genuine installed browser with preserved attempt lineage.
- Evidence completeness, secret redaction, file isolation, checkpointing, and reconciliation.

The browser matrix begins with Firefox stable and candidate builds. It then qualifies Chrome and Edge stable, followed by Safari on macOS where the native bridge is available. Each adapter runs deterministic local fixtures, representative public and authenticated test environments, the curated public canaries, and operator-configured sites.

An adapter qualifies only when it completes the full declared workflow with verified postconditions, valid evidence, bounded recovery behavior, and no credential or unrelated-profile leakage. Command availability or primitive parity alone is insufficient.

Results compare full workflow completion, recovery, latency, and evidence quality with the managed Chromium path. Site outage, challenge, rate limiting, workflow drift, adapter incompatibility, runtime regression, and policy denial remain distinct result classes. Deterministic failures and security invariant violations are blocking; real-site environmental conditions retain the release-certification framework's explicit `degraded` classification.

## Scope and Decomposition

The architecture is one coherent product phase but contains independently testable implementation slices. The implementation plan must preserve these module boundaries and deliver thin end-to-end capabilities rather than building all protocol layers before proving a workflow.

This phase does not replace the durable workflow coordinator, public SDK/MCP/CDP contracts, existing evidence model, or managed Chromium adapter. It extends them with native-browser execution. Distributed remote-browser hosting and browser-vendor modification are separate work.

## Implementation Order

1. Versioned companion protocol, capability model, pairing identity, and attachment leases.
2. Local companion coordinator plus Firefox extension handshake and target discovery.
3. Firefox WebDriver BiDi engine adapter with observe, navigate, target, native input, and verification.
4. Complete forms, submission, tabs, popups, frames, dialogs, downloads, and file attachment through page and native chooser paths.
5. Authenticated-profile integration, password-manager/passkey/MFA events, secret handles, and redaction proof.
6. Durable reconnect, checkpoint reconciliation, browser restart, user-interleaving detection, and restart lineage.
7. Compatibility registry, workflow-level engine selection, and restart on another genuine installed browser.
8. Chrome and Edge adapters reusing the companion contract, followed by the Safari adapter and native bridge.
9. Real-browser workflow matrix, security gates, public and authenticated canaries, performance comparison, and release certification.
