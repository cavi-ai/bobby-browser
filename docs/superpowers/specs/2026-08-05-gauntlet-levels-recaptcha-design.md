# Bobby Gauntlet Levels and reCAPTCHA Design

## Goal

Turn the existing Northstar gauntlet into a tiered training course. Level 1
preserves the current five release-gate journeys. Level 2 increases UI variance
and places a real Google reCAPTCHA v2 checkbox in the onboarding journey.

The gauntlet supplies the real obstacle and verifies its result. It does not
contain CAPTCHA-solving or bypass logic.

## Level contract

- A gauntlet run has an explicit `level` value: `1` or `2`.
- Missing level means Level 1, preserving current callers and URLs.
- The scenario seed still makes all gauntlet-owned variance reproducible.
- Level 1 application behavior and existing journey names remain unchanged.
- Level 2 records its level, seed, selected traps, and durable outcome in the
  scenario state used by assertions and failure evidence.
- Unknown levels fail during scenario construction rather than silently
  falling back.

## Configuration flow

`ScenarioConfig` owns the level and the optional Level 2 reCAPTCHA settings.
The scenario server exposes a bounded public run-configuration endpoint that
returns only the level and reCAPTCHA site key. The secret never reaches the
browser, logs, evidence bundles, or serialized scenario state.

The browser preserves `run` and `level` across Northstar navigation. Page
components consume a small immutable run configuration rather than reading
process environment or inventing their own level rules.

Level 2 requires:

- `BOBBY_GAUNTLET_RECAPTCHA_SITE_KEY`
- `BOBBY_GAUNTLET_RECAPTCHA_SECRET`

The live Level 2 gate fails before browser launch when either value is absent.
Level 1 never requires or reads them.

## Level 2 traps

The first Level 2 slice adds three seeded trap families:

1. **Interruptions:** an additional dismissible modal and an extra popup are
   selected at stable journey boundaries. They cannot change authoritative
   server state by themselves.
2. **Irregular onboarding form:** field order varies by seed; one field is
   conditionally rendered; two controls have similar visible copy but distinct
   accessible names; and one control appears after a bounded delay. Submitted
   data and server validation remain identical to Level 1.
3. **Real reCAPTCHA:** the onboarding form renders Google's reCAPTCHA v2
   checkbox using the configured site key. Submission includes the browser's
   `g-recaptcha-response` token.

Trap selection is deterministic for a `(level, seed, journey)` tuple. Google’s
challenge contents are external and intentionally not deterministic.

## reCAPTCHA verification

The scenario server verifies the submitted token through Google's documented
`siteverify` endpoint using the configured secret. Verification is bounded by
a short timeout and accepts only a successful response. Missing, malformed,
expired, duplicate, or rejected tokens fail onboarding without creating a
customer.

The verifier is an injected interface:

- production Level 2 uses the Google verifier;
- unit and contract tests use a deterministic fake;
- Level 1 uses no verifier.

Verification records retain only a boolean result and stable error category.
Tokens, secrets, Google response bodies, and IP addresses are never retained.

## Failure behavior

- Missing Level 2 keys: configuration error before the scenario starts.
- reCAPTCHA script load failure: visible form error; no submission.
- Verification timeout or upstream failure: stable retryable challenge error;
  no durable mutation.
- Invalid token: stable invalid-challenge error; no durable mutation.
- Trap initialization failure: Level 2 journey fails with the trap identifier
  in diagnostic evidence.

## Test strategy

Package tests prove:

- Level 1 remains the default and does not load reCAPTCHA.
- Level 2 renders seeded irregular variants and the configured site key.
- Level 2 submission includes the reCAPTCHA token.
- Missing or invalid tokens cannot create a customer.
- A successful injected verification permits the existing durable onboarding
  mutation.
- Secrets and tokens do not appear in public state or diagnostics.

Rust scenario tests prove level parsing, fail-fast environment validation,
deterministic trap selection, verifier timeout/error mapping, and zero mutation
on rejected verification.

The live Level 2 browser journey is separately selectable and requires real
keys plus installed Chromium. Existing Level 1 release-gate commands and test
names remain stable.

## Initial scope

This slice implements Levels 1 and 2, the three Level 2 trap families, and the
onboarding reCAPTCHA gate. It does not add a CAPTCHA solver, scoring ladder,
Level 3, third-party site automation, or randomized server data outside the
existing seeded scenario model.
