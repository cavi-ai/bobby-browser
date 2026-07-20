# Release Certification Design

## Purpose

Complete the version 1 browser automation runtime with production release proof for security, Chromium compatibility, public-site operation, and performance. The release system must preserve fail-closed security while distinguishing runtime regressions from ordinary public-site and network drift.

## Outcomes

The runtime gains one modular release-gate framework with four independently runnable suites:

1. Deterministic adversarial security gates.
2. Chromium compatibility gates for the pinned production revision, an upgrade candidate, and optional operator-provided local builds.
3. Declarative public-site canaries, including a curated non-destructive default suite and operator-configured targets.
4. Consolidated performance evaluation and a final version 1 certification verdict.

Every suite produces durable, redacted evidence through one result contract. A separate policy evaluator produces `passed`, `degraded`, or `blocked`; execution code does not decide release policy.

## Architecture

Add a `release-gates` crate as the orchestration and policy layer. It loads a versioned release manifest, validates configuration, selects suites, enforces bounded concurrency and deadlines, and persists a common evidence envelope. Existing component-owned tests and runtime paths remain authoritative; the crate coordinates them rather than moving their logic into a central harness.

Four runners implement a common gate interface:

- `SecurityGate` executes deterministic hostile fixtures and containment attacks.
- `CompatibilityGate` verifies browser identity and supported behavior across configured Chromium builds.
- `CanaryGate` compiles and executes declarative workflows through normal authenticated runtime interfaces.
- `PerformanceGate` consumes reproducible measurements and evaluates approved thresholds.

Each runner returns observations. A separate `PolicyEvaluator` classifies the combined evidence. This permits each suite to run independently and prevents environmental failures in one suite from obscuring deterministic results in another.

The CLI exposes focused commands such as `release-gates security`, `release-gates compatibility`, `release-gates canary`, and `release-gates performance`, plus `release-gates certify` for the complete decision. Execution is local by default, authenticated, bounded, and artifact-backed.

## Release Manifest

A versioned release manifest defines:

- Enabled and required suites.
- Suite deadlines, concurrency limits, and evidence budgets.
- Pinned and candidate Chromium identities.
- Optional local Chromium executable supplied by the operator.
- Performance hard limits and comparison policy.
- Canary definition locations and verdict policy.
- Artifact retention and redaction policy.

Unknown fields, unsupported manifest versions, contradictory settings, missing required suites, and invalid limits are configuration errors and block certification before browser execution.

The manifest is separate from canary workflow files. Release policy cannot be weakened by adding or editing a canary definition.

## Common Gate Results

Every suite emits a versioned result envelope containing:

- Suite and check identity.
- Runtime, build, operating-system, and Chromium identities where relevant.
- Digests of effective configuration and workflow definitions.
- Start time, bounded duration, and completion state.
- Structured observations and assertion results.
- Stable failure classification and redacted diagnostics.
- References and hashes for retained evidence.
- Environmental signals used to distinguish a runtime regression from external drift.

Malformed, incomplete, unauthenticated, or unverifiable evidence cannot satisfy a required gate. Certification produces machine-readable JSON and a concise human report. The final bundle includes a digest over the effective manifest and all normalized results so operators can detect later alteration.

## Security Gate

Security fixtures are deterministic and required for release. They cover:

- Cross-session and cross-principal access.
- Host filesystem and workspace escape attempts.
- Loopback, link-local, private-network, cloud-metadata, redirect, and DNS-rebinding bypass attempts.
- Secret exposure through logs, observations, DOM, screenshots, traces, errors, URLs, and command arguments.
- Oversized frames, bodies, artifacts, queues, and event streams.
- Resource exhaustion and overload behavior.
- Capability, expiry, revocation, idempotency, and recovery-boundary enforcement.
- Popup, worker, WebSocket, service-worker, download, and navigation policy re-evaluation.

Security gates run against controlled local hostile fixtures and use the production runtime boundary. A policy violation, missing expected denial, secret disclosure, unbounded operation, or incomplete proof blocks release. Security failures are never downgraded to environmental degradation.

## Chromium Compatibility Gate

Compatibility uses three build roles:

- The pinned production revision is required and blocking.
- A configured upgrade candidate reports readiness and remains non-blocking until an intentional upgrade makes it the pinned revision.
- An arbitrary operator-provided local build may be probed and is always non-blocking unless the manifest explicitly promotes that exact identity to the pinned role.

The gate verifies exact browser identity before testing supported protocol behavior. It exercises the public compatibility contract, including navigation, target lifecycle, DOM and accessibility inspection, input, frames, popups, downloads, recovery signals, and the compiled CDP support manifest. Unsupported behavior must fail with the documented typed outcome rather than silently changing semantics.

A pinned-build incompatibility blocks release. Candidate or local-build incompatibility produces `degraded` compatibility evidence. Missing or ambiguous browser identity blocks any role declared required.

## Declarative Canary Workflows

Canary workflows use a versioned YAML schema that compiles into the runtime's existing primitive and intent command model. The format includes:

- Workflow metadata and target origins.
- A `nonDestructive` classification and declared required capabilities.
- Entry URL and bounded ordered steps.
- Assertions over URLs, visible state, extracted values, artifacts, and evidence.
- Per-step and total deadlines.
- Explicit retry classification and cleanup expectations.
- Secret references resolved by an operator-provided secret source.

The compiler rejects unknown fields and commands, invalid assertions, undeclared capabilities, embedded secrets, unbounded waits or retries, and inconsistent destructive classifications. It calculates the effective capability set before execution.

The shipped curated suite is always non-destructive. It may navigate, inspect, target, wait, extract, capture bounded evidence, and perform downloads only when the download is known to be safe and does not alter remote state. It cannot submit data, modify accounts, purchase, publish, message, delete, or trigger other externally visible mutations.

Operator workflows may target any public URL. Mutating commands require an explicit destructive declaration, explicit capabilities, and policy grants. Uncertain mutations are not replayed. Secret values are resolved only at execution time and are never serialized into compiled workflows or persisted evidence.

## Canary Classification

Public canaries are real-environment evidence, not deterministic fixtures. The runner records signals needed to distinguish:

- Runtime regressions.
- Site markup or workflow drift.
- Site outage or network failure.
- Rate limiting or challenge pages.
- Policy denial.
- Invalid operator configuration.

Retries are bounded and limited to operations the runtime classifies as replayable. A public observation blocks certification only when repeated evidence isolates a runtime regression or a required safety invariant fails. Site drift, outage, rate limits, challenges, and insufficient environmental evidence produce `degraded` rather than a false runtime failure.

Deterministic local mirrors remain the authoritative blocking proof for behaviors also exercised by public canaries.

## Performance Gate

The performance runner reuses the established persistent-fixture methodology: discard warmup, collect seven paired samples, separate adapter operation time from harness-envelope wall time, and record process-tree memory before, at peak, and after transport closure.

The release manifest defines hard resource and latency limits for correctness-critical properties, including bounded queues, rejection work, concurrency, memory, artifact reads, timeouts, and cleanup. Crossing a hard approved limit blocks release. Comparative measurements that lack an approved hard threshold are reported as trend evidence and cannot manufacture a blocking regression.

Missing samples, mismatched work, unverified browser identity, retained processes, invalid measurement order, or incomplete raw evidence block a required performance gate. Raw profiles and samples remain reproducible artifacts rather than committed source files.

## Verdict Policy

The policy evaluator returns exactly one overall verdict:

- `passed`: every required deterministic gate passed, every required evidence bundle is complete, and no blocking limit was crossed.
- `degraded`: no blocking invariant failed, but one or more non-deterministic or non-required probes could not establish healthy operation.
- `blocked`: a deterministic test failed, a security invariant was violated, the pinned Chromium build is incompatible, required evidence is missing, configuration is invalid, or an approved hard performance limit was crossed.

`degraded` never becomes `passed`, but it exits separately from a blocked result so release automation and operators can apply an explicit policy. No failure may be silently downgraded.

## Certification Flow

`release-gates certify` runs in dependency order:

1. Validate and digest all configuration.
2. Run deterministic security gates.
3. Verify pinned Chromium compatibility, then candidate and optional local builds.
4. Run deterministic browser workflow proof.
5. Run performance gates.
6. Run configured public canaries.
7. Normalize evidence and evaluate the final verdict.

A blocking prerequisite prevents dependent work that would be misleading or unsafe, while independent suites may still finish to provide diagnostic evidence. Cancellation and deadlines produce explicit incomplete results rather than partial success.

## Testing

Unit tests cover manifest and YAML parsing, schema versions, capability calculation, secret-reference handling, classification, threshold evaluation, redaction, digest stability, and verdict aggregation.

Deterministic integration tests prove:

- Every security attack is denied with bounded work and typed evidence.
- Pinned, candidate, and local Chromium roles receive the correct blocking policy.
- Canary compilation and execution use normal authenticated runtime interfaces.
- Curated canaries reject destructive steps at compile time.
- Operator mutations require all explicit declarations and grants.
- Site drift, outages, challenges, and runtime regressions classify distinctly.
- Performance samples and evidence completeness are evaluated reproducibly.
- Mixed suite outcomes produce the correct final verdict and process exit behavior.

The curated public suite exercises representative navigation, semantic targeting, extraction, dynamic waits, downloads where safe, and multi-page state. Release documentation records the exact commands and evidence locations. Health checks and smoke tests remain preliminary signals and cannot satisfy certification.

## Scope Boundaries

This phase does not add distributed scheduling, remote execution, new browser engines, an account-mutation canary service, automatic secret management, or autonomous release publishing. Firefox and WebKit remain future work. The runtime operates ordinary public sites within operator policy; the default canaries remain deliberately non-destructive.

## Implementation Order

1. Common manifest, result envelope, verdict evaluator, and CLI skeleton.
2. Deterministic security runner and adversarial fixtures.
3. Chromium identity and compatibility runner.
4. Canary YAML compiler and deterministic execution fixtures.
5. Curated defaults and operator-configured public execution.
6. Consolidated performance evaluator.
7. Full certification orchestration, reports, and version 1 release proof.
