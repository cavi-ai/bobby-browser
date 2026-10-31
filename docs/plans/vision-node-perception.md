# Vision node: from a 27B decider to a small perception layer

Status: plan · Owner: vision node · Created 2026-10-31 · Inputs: code-review-graph analysis of
`main@5e25a6f`, model research, CUA review.

## Goal

Add value downstream for the user through agent efficiency: **fewer calls, less memory, fewer
tokens.** Anything that costs more must earn it with higher accuracy or speed, and the plan names
that tradeoff. The vision node stops acting as a 27B "decider". It becomes the perception layer
that keeps the page graph fresh. The driver agent queries that graph the way it queries
code-review-graph for a repo: semantic hits, hot spots, no raw dumps.

## What the code does today (verified, not from docs)

Graph build: 707 files, 14,258 nodes. `src-vision` (1,058 nodes) and `src-vision-execute` (688)
are the 3rd and 7th largest communities in the codebase.

| Path | Where | What actually crosses the wire |
|---|---|---|
| Escalation (stuck step) | `intent-engine/src/engine.rs` `escalate_with_vision` | Full viewport PNG, purpose, ≤5 `(role, name)` candidates → index or click point |
| Form prefill | `engine.rs` `collect_prefill_batch` | **One full-screenshot request per field** (concurrency 4), same PNG each time |
| Password-manager fill | `engine.rs` `fill_with_password_manager` | Full PNG → raw click point on browser chrome (not page DOM) |
| Challenge detect / solve | `solveChallenge`, `detectChallenge` | Full PNG → class + region, or multistep clicks |
| `extract_structured` | `vision-proxy` `/extract` | **Text only.** Page text plus schema. No image at all |

Facts that drive the plan:

1. **Deterministic matching is exact-only.** `dom-engine/src/resolver.rs::score` returns no match
   unless role, accessible name and label are equal after trimming. "E-mail" against "Email
   address" is a miss, and the next stop is the 27B VLM.
2. **The candidate window is pure text.** `vision_window_eligible` admits only candidates that
   have both a role and a name. The most common vision call is "pick 1 of ≤5 labelled strings
   for this purpose". That is semantic matching, not vision.
3. **The team already proved it.** `scripts/vision-mlx/providers/v1_provider.py`
   (`BOBBY-VISION/1`) is a *text-only* LoRA selector on `Qwen2.5-7B-Instruct-4bit` that replies
   with a bare index or −1. `training/adapter_freshness.py` already gates it at **≥98% element
   accuracy**, 100% production-negative abstain recall and ≥95% abstain precision. It is not a
   product path, though:
   - The managed child (`cli/src/main.rs::spawn_mlx_server`) always passes
     `--provider mlx-vlm`, so v1 only runs when someone starts it by hand.
   - The runtime still captures, encodes and ships a full screenshot that v1 ignores, because
     `VisionProposeRequest.screenshot_png` is mandatory.
   - One provider serves every request, so there is no routing of named windows to v1 and
     unnamed cases to a VLM.
4. **Prefill re-encodes the same image N times.** A 10-field form runs about 10 full-image
   prefills on a 27B model. Estimate: about 900–1,100 image tokens each at 1280×720 for
   Qwen-family encoders. One encoding would do, and for named candidates zero would.
5. **The graph matcher is lexical.** `page-runtime/src/context.rs::ContextQuery` uses exact, then
   token overlap with a 0.75 floor. Every `context_ask` miss sends the agent to `a11y_snapshot`,
   the most expensive read in its context window.
6. **Corpus attribution bug (fixed in this branch).** `vision-proxy/src/mlx.rs` labelled every MLX
   corpus row with the *default* model (`Qwen3.5-27B-4bit`) whatever `--model` served it. Per-model
   A/B evaluation, the core of this plan, would have been silently wrong.
7. **Stale docs.** `training/TRAINING_RECIPE.md` still says "waiting for MLX vision support" and
   names llava:7b as current. Treat it as history, not as the spec.

## Target architecture: tiered resolution, cheapest first

```
agent purpose ─► T0 exact DOM/AX match (exists)            ~0 ms, 0 MB
                 │ miss / ambiguous
                 ▼
                 T1 decision model, text only (NEW)         ms, ~1–4 GB, no screenshot
                 │ Kev `choice` or BOBBY-VISION/1: purpose ↔ ≤5 (role, name) + none
                 │ calibrated probability ≥0.75, margin, runtime verifies
                 │ miss / unnamed controls / low margin
                 ▼
                 T2 perception (NEW small models)           one screenshot, many queries
                 │ PP-OCRv6 / Apple Vision + 2B grounder (point → runtime hit-test)
                 │ → facts written INTO the page graph
                 │ (visual labels for unnamed controls, text-in-box, challenge class)
                 ▼
                 T3 heavy VLM (today's 27B, opt-in)         solveChallenge loop / last resort
```

Rules every tier keeps (they already exist and stay non-negotiable):

- The runtime owns grounding and verification. Tiers propose and postconditions decide. The
  0.75 floor, the evidence digest and the allowed-actions checks all stay as they are.
- Nothing typed or extracted ever reaches a model, and the corpus stays index-only.
- Each tier reports `path` in evidence (`deterministic`, `semanticText`, `perception`,
  `visionFallback`). Measured per tier, so a tier that does not pay its way gets removed.

### Why this shape (tradeoffs, user and agent first)

| Change | Calls | Memory | Tokens (agent) | Accuracy / speed | Verdict |
|---|---|---|---|---|---|
| T1 text matcher | −1 VLM call per named near-miss | 27B → ≤1B resident | 0 change | ms instead of seconds; accuracy must be ≥ 27B on the corpus window suite | **Do**, gated on eval |
| Prefill: one image, N queries | N → 1 image encodings | same | 0 | form fill latency drops roughly with N | **Do** |
| T2 perception writes graph facts | fewer repeat escalations (remembered visual labels) | +small model | **−** fewer `a11y_snapshot` after `context_ask` misses | next visit resolves at T0/T1 | **Do** after T1 |
| Semantic `context_ask` | − snapshot calls | +embedding index per page (bounded) | **large −** (snapshot avoided) | hit-rate ↑, same floor | **Do**, uses T1 model |
| Keep 27B as default | — | ~15+ GB | — | best on solveChallenge | **Demote** to opt-in T3 |
| Kev decision model for T1 | N field picks → 1 call | 16 GB → 3.8 GB (0.8B) | 0 | calibrated probabilities; must hold the freshness floors | **Do**, competes with the v1 adapter |
| Vendor CUA | — | +sidecar | — | duplicates CDP core; AGPL/FSL parts; pre-release churn | **Don't** |
| `cua-driver` sidecar for browser-chrome prompts | −1 VLM call per password-manager fill | small native binary | 0 | deterministic AX click (if reachable) | **Spike only** |

## Model candidates

Research pass on 2026-10-31. These are published numbers. The shortlist gets re-measured on our
own suites before any default changes. SS-Pro = ScreenSpot-Pro (desktop pro apps), SSv2 =
ScreenSpot-v2 (web and mobile, near saturation), OSW-G = OSWorld-G.

**Baseline: Qwen3.5-27B-4bit.** SS-Pro 70.3, OCRBench 89.4
([card](https://huggingface.co/Qwen/Qwen3.5-27B)). On an M5 Max: 28 tok/s decode, 1.67 s TTFT at
1k tokens, **16.0 GB peak** ([oMLX](https://omlx.ai/benchmarks/performance/unzup0mb)).

| Tier | Model | Size / RAM | License | Runtime | Published | Coordinates |
|---|---|---|---|---|---|---|
| T2 text | **PP-OCRv6** tiny / small / medium | 1.5M / 7.7M / 34.5M params | Apache-2.0 | ONNX; pure-Rust crates (`pure-onnx-ocr`, `ppocr-rs`) | medium: det 86.2 Hmean, rec 83.2 (Baidu in-house); screen-text rec 82.5 vs 57.6 (v5 mobile) | absolute px polygons |
| T2 text (macOS) | **Apple Vision OCR** | ships with the OS | OS API | native | ~0.2 s/call on M3 Max (third-party test) | normalized |
| T2 elements | **OmniParser `icon_detect_v3`** | YOLO, needs torch today | **MIT (v3 only; original `icon_detect` is AGPL)** | TorchScript; ONNX export UNVERIFIED | v3 accuracy not published | xyxy px |
| T2 ground | **UI-Venus-1.5-2B** | 2.44B, ~2 GB at 6-bit | Apache-2.0 | mlx-community 6-bit, GGUF | SS-Pro 57.7, SSv2 92.8, OSW-G 59.4 (ClawGUI re-measured 58.8 / 93.2 / 59.0) | Qwen3-VL 0–1000 (UNVERIFIED for this fine-tune) |
| T2 ground | **MAI-UI-2B** | 2.13B | Apache-2.0 | mlx-community 6-bit, GGUF | 57.4 / 92.5 / 52.0 | 0–999 |
| T2 ground (big-RAM) | **GUI-Owl-1.5-4B** | ~4.4B | MIT (card changed from Apache; re-check) | GGUF; self-convert to MLX | 65.3 / 92.8 / 61.7 | UNVERIFIED |
| Train base | **Qwen3.5-2B** | 1.6 GB MLX 4-bit | Apache-2.0 | official Ollama and MLX | SS-Pro 54.5, OCRBench 85.4; 308 tok/s on M5 Max | likely 0–1000 |
| Low-RAM | **Qwen3.5-0.8B** | 1.0–1.3 GB | Apache-2.0 | official Ollama and MLX | SS-Pro 46.5, OCRBench 79.1 | as above |

Reading of the numbers, for the user and the agent:

- **Web-like grounding is saturated at 2B.** SSv2 is 92–93 for the 2B specialists vs 95.9 for an
  8B. The 27B's lead (70 vs 58 SS-Pro) is on high-resolution professional desktop apps, which is
  not our workload. Memory: **16 GB → ~2 GB**. Latency: from ~2 s+ to roughly <0.4 s per point
  (derived from a same-architecture 2B TTFT of 223 ms at 1k tokens; UNVERIFIED for these
  checkpoints).
- **OCR does not need a VLM.** A 7.7M–34.5M-param OCR (or Apple Vision at 0 MB) reads text boxes
  once per screenshot. `extractValue` and many named-control questions then become graph lookups
  with no model call.
- **Embeddings are not a primary matcher.** Off-the-shelf CLIP-style models score Recall@1 of
  0.08–0.13 on text-to-GUI retrieval, and UI screenshot embeddings collapse (cosine 0.87–0.96).
  This applies to *image* embeddings. T1 matches text against text and is a different problem.
- **Mark candidates for the grounder; never send bare text lists.** In GUI-Primitives, 60–92% of
  grounding misses fall outside every candidate region, and marking candidates lifts selection by
  35–57 points. T2 therefore sends candidate boxes as marks or crops, returns a **point**, and the
  runtime hit-tests it against DOM boxes. The model never chooses the action.
- **Fine-tune choice:** Qwen3.5-2B (or UI-Venus-1.5-2B when the corpus is grounding-only).
  Train bf16 LoRA, not QLoRA (vendor guidance, Unsloth), then re-check accuracy after 4-bit
  export.
- **Reasoning tokens are hidden latency.** MAI-UI's prompt asks for reasoning, Holo2 is built on a
  Thinking base, and Qwen3.5 ≥4B thinks by default. Disable or cap reasoning everywhere.

**License traps (excluded from defaults):** OmniParser `icon_detect` v1/v2 (AGPL); Holo1.5-3B
(Qwen Research); KV-Ground-4B (CC BY-NC-SA); jina-embeddings-v4 (non-commercial); FastVLM
(apple-amlr); MobileCLIP2 (Apple TOU).

**T1 base (text-only):** not covered by the image benchmarks above. Candidates are the existing
`BOBBY-VISION/1` contract on the smallest Qwen3.5 text path, kev/laya, or a cross-encoder
reranker. All unmeasured; the window suite decides.

**Challenges:** detection goes deterministic first (challenge iframe sources and DOM markers),
with vision only for image-only cases. We do not invest in training challenge *solvers*. The
default stays detect, pause, hand off to the user, and the existing `solveChallenge` path stays
opt-in on T3.

Sources: Qwen3.5 cards ([2B](https://huggingface.co/Qwen/Qwen3.5-2B),
[27B](https://huggingface.co/Qwen/Qwen3.5-27B)),
[UI-Venus-1.5-2B](https://huggingface.co/inclusionAI/UI-Venus-1.5-2B),
[MAI-UI-2B](https://huggingface.co/Tongyi-MAI/MAI-UI-2B),
[GUI-Owl-1.5-4B](https://huggingface.co/mPLUG/GUI-Owl-1.5-4B-Instruct),
[ClawGUI re-measurement](https://zju-real.github.io/ClawGUI-Page),
[PaddleOCR 3.7 / PP-OCRv6](https://github.com/PaddlePaddle/PaddleOCR/releases/tag/v3.7.0),
[OmniParser v2](https://huggingface.co/microsoft/OmniParser-v2.0),
[Unsloth Qwen3.5 fine-tune](https://unsloth.ai/docs/models/qwen3.5/fine-tune),
[GUI-Primitives](https://arxiv.org/abs/2608.21832),
[Ollama qwen3.5 tags](https://ollama.com/library/qwen3.5/tags).

## Decision models: Kev (local), Laya (local), Jev (hosted)

These are *decision models*, not LLMs. They take a `state` plus named, bounded questions and return
typed answers (`choice`, `score`, `noul`) with calibrated probabilities. They are the decision half
that this plan pulls *out* of the vision node.

- **Kev** ([jaredpalmer/kev](https://github.com/jaredpalmer/kev))
  - Apache-2.0. Sizes 0.8B / 4B / 9B / 27B on Qwen3.5/3.8 bases.
  - Local HTTP `POST /v1/systemone`; uses MLX automatically on Apple Silicon.
  - Up to 255 options per `choice`; each checkpoint ships a fitted temperature.
  - Peak on Apple Silicon: 0.8B **3.8 GB**, 4B **13.0 GB**.
  - Model time for 6 questions: 16–23 ms (0.8B, L4 GPU). Apple Silicon latency is UNVERIFIED.
  - Fine-tunes from labelled JSONL (`kev.train`, CUDA in the docs).
- **Laya** (ConvAI): open-weight, built for >50-class classification. License to be verified
  before bundling.
- **Jev** (TypeSafe): hosted subscription. **Excluded from the default path**: per-call cost, and
  page structure would leave the machine. Opt-in only, like the OpenAI upstream.

Where they add real value, ranked:

1. **T1 candidate pick (primary).**
   - Request: one `choice` question per stuck field. `state` carries purpose, url and recent command
     kinds; `criteria` carries the ≤5 candidate `(role, name)` labels plus `none`.
   - Value 1, calibration: calibrated probabilities replace the VLM's self-reported "confidence",
     so the existing 0.75 floor finally means a probability. Abstain is `none` winning or a low
     margin.
   - Value 2, batching: `questions` is a map, so **a 10-field prefill is one call**. That is P2 for
     free on the text path.
   - Contract: maps 1:1 onto `BOBBY-VISION/1` (bare index or −1). Integration is a `kev` upstream in
     `vision-proxy` (HTTP, like `ollama.rs`), declared `needs_image = false`.
2. **Page-state classification.** `detectChallenge` (6 classes + none), obstruction and login-wall,
   read from DOM/AX structure plus T2 OCR text. Today a 27B VLM produces this single enum.
3. **Prompt-injection screen** (`noul`) on page text before it reaches the driver agent. See
   `security/prompt-injection.md` and the `prompt_injection_canary` tests.

Costs and cautions:

- Kev-0.8B (3.8 GB) still cuts resident memory ~4× vs the 27B (16 GB).
- Kev's own general benchmark puts 0.8B at 0.65–0.70 on *new* sources vs 0.82–0.84 for 4B. We
  fine-tune on our window corpus and gate on the existing freshness floors (98% / 100% / 95%),
  plus the out-of-sample split (`training/oos_split.py`).
- Precedent: CUA-S1-forms reported 99.7% in-distribution but **29.3%** on out-of-catalogue forms
  in an independent eval. In-distribution numbers alone do not count.
- Privacy is unchanged: `state` carries sanitized labels only (the corpus sanitizers), never typed
  values or page text.

## CUA (trycua/cua)

**Verdict: do not vendor any of it.** The value is narrow, and it is worth one measured spike, as
a sidecar.

Reasons, with sources in the review notes:

- **Overlap.** `cua-driver` ships CDP click/type/select/dialogs/`set_input_files`/downloads, which
  duplicates bobby's core ([tools](https://cua.ai/docs/cua-driver/reference/mcp-tools)).
- **Churn.**
  - The whole Python stack (computer, computer-server, agent, som) is deprecated in favour of a
    Rust core ([README](https://github.com/trycua/cua)).
  - Every `cua-driver` release is pre-release: 0.33.4 → 0.38.0 in 16 days.
  - The SDK broke at 0.5.0, and it is not on crates.io.
  - Vendoring it the way we vendor chromiumoxide would mean owning a fast-moving codebase. That is
    technical debt, not leverage.
- **License traps.**
  - `cua-perception` bundles the OmniParser `icon_detect` ONNX, which is **AGPL-3.0**, with
    network-use obligations.
  - `cua-som` is AGPL.
  - `spacesd` and Spaces are **FSL-1.1** with a competing-use clause; bobby is arguably a
    competing product
    ([LICENSING.md](https://raw.githubusercontent.com/trycua/cua/main/legal/LICENSING.md)).

The one real value case:

- **Native surfaces CDP cannot reach.** `cua-driver` (MIT, `cua-driver mcp` over stdio) drives the
  platform accessibility tree: OS file pickers, print/save sheets and, *probably*, browser-chrome
  popups (UNVERIFIED).
- Today `passwordManagerFill` (#830) sends a full screenshot to the 27B VLM to find the password
  manager's prompt, then makes a trusted click. If `cua-driver` can address that prompt through
  the AX tree, a 16 GB VLM call becomes a deterministic, verifiable AX click.

**Spike (P5, optional):**

- Run `cua-driver` as an opt-in sidecar process behind a new capability, with a pinned version and
  no perception extension.
- Compare it with the current vision path on the password-manager prompt: success rate, latency
  and RAM.
- Adopt only if it wins. Otherwise drop it.
- Lume (MIT) is relevant only if we later want macOS VM isolation for untrusted sessions.

## Shared contract: schema, skill, refs

One perception contract that every agent (driver, spawned vision subagents, SDKs) reads the
same way:

- `types` crate: `PerceptionFact { kind: text|element|challenge, bbox, label, confidence, source,
  generation }`. It is generated into the OpenAPI spec and the TS/Python SDKs the way existing
  contracts are, so there is no hand-written duplicate.
- MCP resource `bobby://vision` with the tiers, when each fires, and the evidence `path` values.
  The subagent work in flight elsewhere consumes this contract and must not fork it.
- `skill/SKILL.md`: one short section. Ask the graph first (`context_ask` /
  `workflow_observe goal`), never request screenshots for named controls, and read `path` to know
  what resolved a step.

## Phased plan

Each phase ships on its own, behind config, with its eval numbers in the PR.

**P0: measure first** (this branch)
- [x] Fix MLX corpus model attribution (`vision-proxy/src/mlx.rs`, `cli/src/main.rs`) plus a
  regression test (commit `0f60331`).
- [ ] Per-tier suites on the existing harness (`training/adapter_freshness.py` suites,
  `evaluate_adapter.py`): window-pick accuracy with abstain calibration, grounding hit-in-bbox,
  OCR exact match, challenge-class F1, plus p50/p95 latency and peak RSS. Baseline: Qwen3.5-27B.
  Ground truth is *verified* outcomes only, never a prior model's answer.

**P1: T1 semantic text tier (productize what exists)**
- [ ] Provider capability `needs_image: bool` on `VisionAssist`. Text-only providers skip
  screenshot capture entirely (saves the CDP capture, PNG encode and multi-MB base64 body).
- [ ] Route named-candidate windows to the T1 provider (v1 contract) and keep the VLM for unnamed
  cases. Today it is one provider for everything.
- [ ] `bobby vision connect` can select the v1 provider and adapter. Today the managed child is
  hard-wired to `--provider mlx-vlm`.
- [ ] Shrink the T1 base: re-train `BOBBY-VISION/1` on the smallest base that holds the existing
  freshness floors (98% / 100% / 95%). Try Qwen3.5-0.8B and 2B text paths first. A kev/laya
  classifier competes here on equal terms.

**P2: one image, many queries**
- [ ] Batch prefill: one request carrying one screenshot and N purposes, answered with N indices.
  The wire stays bounded by the existing 32-proposal cap.

**P3: perception writes the graph**
- [ ] T2 page parse: PP-OCRv6 through ONNX in Rust (Apple Vision on macOS) for text boxes, and
  `icon_detect_v3` for non-DOM interactables. One pass per screenshot, cached by image hash and
  page generation.
- [ ] T2 grounder (UI-Venus-1.5-2B or MAI-UI-2B; measured on our suite) returns a point. The
  runtime hit-tests it against candidate boxes and records `PerceptionFact`s.
- [ ] Persist verified visual labels in `context-store` (same privacy rules: labels only, never
  values) so the next visit resolves at T0/T1.
- [ ] Semantic `ContextQuery` scoring with the T1 model, so `context_ask` hits more and the agent
  snapshots less.

**P4: demote 27B**
- [ ] Default stack becomes T1 plus T2. The 27B stays as opt-in T3 for `solveChallenge`.
  `bobby doctor` reports which tiers are active and their RAM.

**P5 (optional): `cua-driver` spike** for the password-manager prompt (see §CUA).

## Open questions for the owner

1. Which Kev size is running now, on which host and port, and does Laya's license allow bundling?
2. Target floor hardware for the default stack (8 GB / 16 GB / Apple Silicon only?). This decides
   2B vs 0.8B for the grounder.
3. Can the corpus collected so far (`scripts/vision-mlx/data/`, local only) be used to train T1,
   and roughly how many verified rows does it hold?
4. Allow `gui-agent.github.io` in the environment's network policy if we want the live
   ScreenSpot leaderboard in future research passes (it was blocked in this pass).
