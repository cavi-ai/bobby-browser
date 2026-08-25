# Captcha Solving Feature Implementation Plan

## Status (2026-08-17)

- **Phase 1: Core types** — done. `challenges.rs` is now exported by both
  `bobby-browser-client` and `types`; `SolveChallengeHints::default()` fixed
  to carry the 30s timeout.
- **Phase 2: Intent handler** — done. `IntentCommand::SolveChallenge`
  (Reconciliable), compiler plan, and `execute_solve_challenge` in the
  engine: vision-primary loop (screenshot → propose → act → reassess, 750ms
  poll) that fails closed on provider error, below-floor confidence, or a
  disallowed action, and completes on the new `challengeSolved` vision
  action. Unit tests: `crates/intent-engine/tests/solve_challenge.rs`.
- **Phase 3: Vision integration** — done. `challengeSolved` action across
  the wire (`vision-proxy` wire/validate/data_collector, `http_vision`),
  solveChallenge guidance in the Ollama/OpenAI propose prompts. The MLX
  adapter in `scripts/vision-mlx/` (canonical provider base + legacy
  server) now shares the same schema guidance, canonicalizes snake_case
  action kinds, and drops chatty non-action response fields instead of
  failing closed on them (a chatty model emits them deterministically —
  rejection was unrecoverable, dropping preserves the no-echo guarantee).

## Robustness findings (2026-08-18, live 3B/27B comparison)

- **Engine retry semantics**: a transient dud (unparseable/off-schema
  reply, below-floor confidence) costs one attempt and the loop
  reassesses; only the deadline is terminal for those. Disallowed actions
  and act failures stay terminal. One dud no longer kills a 120s budget.
- **solveChallenge is click-only** by design: `typeText` proposals carry
  no resolved target and the empty-selector act errors at the driver
  (TypeIntoCandidate replaced raw vision typing in #317). Prompts say
  click-only with an explicit never-type constraint. Text captchas need a
  focused-element typing path — future work.

## Model evaluation (2026-08-18, panel of 4 gauntlet screenshots + Level 2 e2e)

| Model | Result |
|---|---|
| Qwen2.5-VL-3B (old default) | Deterministically proposes typing the widget label ("I'm not a robot"); engine correctly refuses. Cannot drive. |
| Qwen2.5-VL-7B | Dead-center grounding but 0.70 self-confidence — permanently under the 0.75 floor. Cannot drive. |
| GLM-4.1V-9B-Thinking | Incoherent: unconditional challengeSolved, confidence uncorrelated with reality. Disqualified. |
| Qwen3-VL-30B-A3B | Excellent grounding once its **[0,1000) normalized coordinates** are rescaled (see below). |
| **mlx-community/Qwen3.5-27B-4bit** | **Chosen default.** Dead-center grounding, Level 2 e2e green in ~14s. |
| ollama qwen3.8:27b-mlx (qwen3_5) | Proven path (e2e green 2×); kept as the ollama profile. |

**Key discovery — normalized coordinates**: Qwen3-VL/Qwen3.5 emit click
coordinates normalized to [0, 1000), not absolute pixels (Qwen2.5-VL was
absolute). Every "misgrounded" result mapped exactly once rescaled
(428,558 → (813,547) = the checkpoint button; (296,683) → (562,669) =
checkbox center). The mlx-vlm provider now rescales per model family
(`VISION_COORD_SPACE=normalized|absolute` override) and uses the
system+user chat template — the single-message template made Qwen3.5 emit
malformed JSON (`"x": 298, 684}` with no `y` key).

- **Qwen2.5-VL-3B (the old default) cannot reliably drive
  solveChallenge**: it deterministically proposes typing the widget label
  ("I'm not a robot") on some pixel variants of the same page, and the
  fail-closed engine correctly refuses. The 3B failure is a model
  capability limit, not a pipeline defect.
- **Phase 4: CLI & ZigZagZig** — done (2026-08-18). `bobby vision solve
  --purpose … [--url … | --session … --page …] [--node vision]
  [--timeout-ms 120000] [--zigzagzig]` submits the intent over `/v1`;
  `--zigzagzig` creates the session with every capability on (humanize +
  fingerprint + JS evaluation + vision assist).
  Smoke-tested live against the gauntlet Level 2 page over authenticated
  HTTP (green checkmark confirmed from the final artifact).
- **Phase 4b: Ladder solve tactic** — done (2026-08-24). `SkillTactic::
  SolveChallenge` joined the `/zigzagzig` recovery ladder at rung 4 (skill
  v1.1.0): after observe/re-resolve/retry and before any checkpoint-bearing
  tactic, the coordinator runs the vision solve loop in place with the
  session's proven gate (`SkillRecoveryCoordinator::with_session_gate`),
  then re-observes the original postcondition. A session without vision
  assist declines the rung fail-closed and climbs on.
- **Phase 5: Learning** — done (2026-08-18). `SiteContext.challenges`
  holds per-site solve counters (success/failure + day-precision stamp,
  same privacy discipline as intent stats); promotion routes
  `solveChallenge` outcomes there instead of the control schema (a solve
  has no resolved control — it acts on pixels inside a widget iframe).
  `ContextStore::challenge_prior(site)` returns the most-attempted kind
  for a future detector; `bobby context list` prints the summary. The
  DetectChallenge consumer of the prior remains future work (solving is
  opt-in by design).
- **Phase 6: Gauntlet** — done and proven live (2026-08-18). Level 2 test
  lives in `crates/runtime-tests/tests/modern_gauntlet_level2_e2e.rs`, gated
  on `BOBBY_GAUNTLET_LEVEL=2` + reCAPTCHA keypair +
  `BOBBY_GAUNTLET_VISION_ENDPOINT`, kept out of the mandatory five-journey
  release suite. Verified end-to-end against a real Google reCAPTCHA v2
  widget (test keys) with a local qwen3.8:27b vision model behind
  `bobby vision-proxy`: the loop clicked the checkbox, the widget issued a
  token, the backend verified it via siteverify, and the onboarding record
  was created. Fixes that made it work: `grecaptcha.ready()` gate in the
  gauntlet app (render=explicit stub), the session policy naming the legacy
  `vision` node, and explicit action JSON shapes in the propose prompts.

## Overview
Build opt-in captcha solving capability that integrates with zigzagzig mode, leveraging vision assist and existing knowledge graph (context_store).

## Architecture

```
User Intent: intent_solve_challenge (CLI: bobby vision solve --zigzagzig)
  ↓
[DetectChallenge] - Uses small model (1-3B) + knowledge graph for probabilistic detection
  ↓
[ResolveChallengeType] → RecaptchaV2Checkbox | TextCaptcha | ImageGridCaptcha | etc.
  ↓
[SolveChallengeIntent] → Solves challenge via vision assist
  ↓
[Vision Assist Node] - Uses qwen3.5-27b (or model from config)
  ↓
[Cached Proposals] + [Knowledge Graph Learning]
```

## Files to Create/Modify

### 1. New Types (`crates/bobby-browser-client/src/challenges.rs`)
- `ChallengeType` enum (RecaptchaV2Checkbox, RecaptchaV3, TextCaptcha, etc.)
- `ChallengeDetection` struct (type, confidence, region)
- `SolveChallengeIntent` 
- `SolveStep` enum (Click, TypeText, WaitAndReassess)

### 2. Intent Command (`crates/bobby-browser-client/src/commands.rs`)
- Add `SolveChallenge(SolveChallengeIntent)` to `IntentCommand` enum
- Update `class()` method to return `Reconciliable`

### 3. Intent Engine (`crates/intent-engine/src/engine.rs`)
- Add `execute_solve_challenge()` handler that:
  - Bypasses DOM resolution (vision-primary)
  - Captures screenshot
  - Escalates to vision assist with solve context
  - Loops: execute step → wait → reassess until resolved

### 4. Vision Assistant (`crates/vision-proxy/src/`)
- New task type: `solve_challenge`
- Enhanced prompts per challenge type
- Multi-step loop for grid challenges

### 5. CLI (`crates/cli/src/main.rs`)
- Add `Vision { Solve {...} }` to `VisionCommands`
- Add `run_vision_solve()` handler
- Support zigzagzig mode (humanize + fingerprint + captcha solving)

### 6. Knowledge Graph Integration (`crates/context-store/`)
- Query: `get_challenge_prior(url)` → probabilistic challenge type
- Record: `record_solution(domain, challenge_type, success)`

## Implementation Steps

1. **Phase 1: Core Types** (30 min)
   - Add challenges.rs with all types
   - Wire to commands.cs intent enum
   - Compile test: `cargo build -p bobby-browser-client`

2. **Phase 2: Intent Handler** (1 hour)
   - Add `execute_solve_challenge()` to intent engine
   - Vision escalation with solve context
   - Multi-step loop for grid challenges

3. **Phase 3: Vision Integration** (1 hour)
   - Extend vision-proxy with solve task type
   - Enhance prompts per challenge type
   - Test with real captcha scenarios

4. **Phase 4: CLI & ZigZagZig** (30 min)
   - Add `bobby vision solve --zigzagzig`
   - Enable humanize/fingerprint flags
   - Slash command support (`/zigzagzig`)

5. **Phase 5: Learning** (2 hours)
   - Query knowledge graph for challenge priors
   - Record successful solutions per domain
   - Boost confidence from historical patterns

6. **Phase 6: Gauntlet Tests** (1 hour)
   - Update Level 2 test to call solve
   - Verify token returned from backend

## Key Design Decisions

1. **Opt-in only** - No automatic challenge detection on every page
2. **Vision-primary** - Skip DOM resolution, go straight to vision for captchas
3. **Iterative solving** - One step at a time (especially for image grids)
4. **Probabilistic detection** - Knowledge graph → challenge prior
5. **Learning loop** - Successes recorded to domain→challenge mapping

## Model Selection

Use existing vision assist node configuration:
- Default: qwen3.5-27b (from config)
- Small model for detection: optional 1-3B encoder
- Large model for solving: same vision assist provider

## Testing Strategy

1. Unit tests per challenge solver (recaptcha, text, image grid)
2. Integration test: gauntlet level 2 with solve intent
3. Edge cases: failed detection, timeout, verification failures

## Success Metrics

- Captcha solved without user intervention
- Confidence ≥ 0.75 (VISION_CONFIDENCE_FLOOR)
- Domain-specific learning improves subsequent solves
- Zero false positives on non-captcha pages
