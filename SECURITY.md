# Security Policy

bobby-browser is a browser-automation runtime that executes untrusted actions
against real browsers on behalf of authenticated principals. Security is a
primary design goal, not an afterthought.

## Status

This project is in **alpha**. The security model below is enforced and tested,
but the surface is still evolving before 1.0. Do not expose the runtime to
untrusted networks; it is designed to be reached over loopback or an operator-
controlled boundary.

## Security model

- **Fail closed.** Authentication and authorization deny by default. A request
  that cannot be positively authorized is rejected, never allowed through.
- **Capability-scoped tokens.** Every bearer binds one principal to an explicit
  capability set and expiry. Capabilities are re-checked at dispatch, including
  on long-lived MCP and CDP connections. Revocation and expiry take effect
  immediately.
- **Bounded issuance.** Tokens are minted only by a principal holding
  `authority:admin`. Issued capabilities must be a subset of the issuer's, cannot
  include `authority:admin`, and are TTL-capped. Only SHA-256 hashes of bearers
  are persisted — never the bearer itself.
- **No credentials in URLs, logs, or committed config.** Bearers travel only in
  the `Authorization` header. The issuance response returns a bearer exactly
  once. Structured logs and error paths are redacted.
- **Per-primitive capability enforcement.** Privileged browser primitives
  require their own capability beyond `browser:mutate`: file upload requires
  `file:upload`, file download requires `file:download`.
- **JavaScript evaluation is deny-by-default and double-gated.** Running JS
  requires both the `javascript:evaluate` capability *and* a session explicitly
  created with `executionPolicy.javascriptEvaluation = true`. An unknown session
  fails closed. Execution is time- and result-size-bounded.
- **Per-principal isolation.** Independent in-flight quotas prevent one tenant
  from starving another; server state (runtime binding, MCP lifecycle,
  idempotency) is scoped per principal.
- **Bounded everything.** Request bodies, MCP frames, tool input, event reads,
  download sizes, redirect counts, JS results, and connection counts are all
  bounded; overload returns typed, retryable errors rather than failing open.

## Reporting a vulnerability

**Please do not open a public issue for security vulnerabilities.**

Report privately through GitHub's
[private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
on this repository (Security → Report a vulnerability). Include:

- a description of the issue and its impact,
- steps to reproduce (a minimal proof of concept if possible),
- affected component/surface (HTTP issuance, MCP, CDP gateway, worker, …),
- any suggested remediation.

You can expect an acknowledgement and an initial assessment. Please allow a
reasonable window for a fix before any public disclosure.

## Scope

In scope: authentication/authorization bypass, capability or policy escapes
(especially anything that lets JavaScript run without both gates), token
disclosure, cross-principal data or state leakage, sandbox/policy bypass on the
adaptive HTTP or CDP surfaces, and denial-of-service that bypasses the runtime's
bounds.

Out of scope: issues that require an already-compromised host or the bootstrap
`authority:admin` credential, and vulnerabilities in third-party browsers
themselves (report those upstream).
