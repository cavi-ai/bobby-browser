#!/usr/bin/env python3
"""Window-pick suite: run a serving vision provider over corpus windows.

Each row is sent the way the runtime sends it (purpose, intent, stuck, page
url, candidates, screenshot) and the answer is scored the way the runtime
acts on it: a candidate index at or above the confidence floor is a pick,
anything below the floor is an abstention. Reports pick accuracy, abstain
recall/precision, selective accuracy by confidence, latency percentiles and
the peak memory of this process (meaningful for in-process MLX providers).

Usage:
    python eval_provider.py --provider v1 --input data/oos_test.jsonl
    python eval_provider.py --provider mlx-vlm --model mlx-community/Qwen3.5-27B-4bit \
        --input data/oos_test.jsonl --output models/eval/qwen35-27b.json
"""

import argparse
import json
import math
import resource
import sys
import time
from pathlib import Path

HERE = Path(__file__).parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE / "training"))

from candidate_action_contract import CANDIDATE_ACTION_KINDS
from evaluate_adapter import calibration_metrics, v1_metrics
from mlx_finetune import supervised_examples
from providers import create_provider, ProposeRequest
from providers.base import VisionContext, VisionContextCandidate

# Mirrors VISION_CONFIDENCE_FLOOR in crates/intent-engine/src/vision.rs: the
# runtime discards any proposal below it.
VISION_CONFIDENCE_FLOOR = 0.75


def request_for(example: dict) -> ProposeRequest:
    candidates = example.get("context_candidates") or []
    return ProposeRequest(
        purpose=example["purpose"],
        intent_kind=example["intent_kind"],
        stuck=example["stuck"],
        screenshot_b64=example.get("image_b64") or example["imageB64"],
        context=VisionContext(
            url=example.get("context_url"),
            candidates=[
                VisionContextCandidate(role=c["role"], name=c["name"], ordinal=c.get("ordinal"))
                for c in candidates
            ],
        ),
    )


def runtime_pick(response: dict | None, n_candidates: int) -> int | None:
    """Index the runtime would act on; -1 when it acts on nothing (below the
    floor, out of range, provider error); None for a raw click point, which
    the runtime acts on but a candidate window cannot score."""
    if response is None:
        return -1
    action = response.get("action", {})
    if response.get("confidence", 0.0) < VISION_CONFIDENCE_FLOOR:
        return -1
    if action.get("kind") not in CANDIDATE_ACTION_KINDS:
        return None
    index = action.get("index")
    if not isinstance(index, int) or isinstance(index, bool) or not 0 <= index < n_candidates:
        return -1
    return index


def percentile(values: list, fraction: float) -> float | None:
    """Nearest-rank percentile."""
    if not values:
        return None
    ordered = sorted(values)
    return ordered[max(0, math.ceil(fraction * len(ordered)) - 1)]


def peak_memory_mb() -> dict:
    rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    # ru_maxrss is bytes on macOS and kilobytes on Linux.
    rss_mb = rss / (1024 * 1024) if sys.platform == "darwin" else rss / 1024
    mlx_mb = None
    if "mlx.core" in sys.modules:
        mx = sys.modules["mlx.core"]
        get_peak = getattr(mx, "get_peak_memory", None) or mx.metal.get_peak_memory
        mlx_mb = get_peak() / (1024 * 1024)
    return {"peak_rss_mb": rss_mb, "mlx_peak_mb": mlx_mb}


def evaluate(provider, examples: list, limit: int | None = None) -> tuple[dict, list]:
    examples = supervised_examples(examples, "v1")
    if limit:
        examples = examples[:limit]
    print(f"Evaluating {provider.name} on {len(examples)} windows...")

    rows = []
    for i, example in enumerate(examples):
        start = time.perf_counter()
        try:
            response = provider.propose(request_for(example)).to_dict()
            error = None
        except Exception as e:
            response = None
            error = type(e).__name__
        elapsed = time.perf_counter() - start
        n_candidates = len(example.get("context_candidates") or [])
        rows.append({
            "example_idx": i,
            "journey": example.get("journey"),
            "step": example.get("step"),
            "outcome_stage": example.get("outcome_stage"),
            "target_index": example.get("target_index"),
            "response": response,
            "pick": runtime_pick(response, n_candidates),
            "error": error,
            "elapsed": elapsed,
        })
        if (i + 1) % 10 == 0:
            print(f"  {i + 1}/{len(examples)} ({elapsed:.2f}s last)")

    picks = [
        {"prediction": None if r["pick"] is None else {"action": {"index": r["pick"]}}}
        for r in rows
    ]
    window = v1_metrics(picks, examples)
    window["point_answers"] = sum(1 for r in rows if r["pick"] is None)
    window["errors"] = sum(1 for r in rows if r["error"] is not None)

    # The first request carries the model load.
    timings = [r["elapsed"] for r in rows]
    steady = timings[1:] or timings
    results = {
        "provider": provider.name,
        "model": getattr(provider, "model_name", None) or getattr(provider, "model", None),
        "confidence_floor": VISION_CONFIDENCE_FLOOR,
        "window": window,
        "calibration": calibration_metrics(
            [{"prediction": r["response"]} for r in rows], examples
        ),
        "latency": {
            "first_request_s": timings[0] if timings else None,
            "p50_s": percentile(steady, 0.50),
            "p95_s": percentile(steady, 0.95),
            "mean_s": sum(steady) / len(steady) if steady else None,
        },
        "memory": peak_memory_mb(),
    }
    return results, rows


def main():
    parser = argparse.ArgumentParser(description="Window-pick suite for a serving vision provider")
    parser.add_argument("--provider", default=None, help="mlx-vlm | v1 | ollama | lmstudio")
    parser.add_argument("--model", default=None, help="mlx-vlm model override")
    parser.add_argument("--input", default="data/training_data.jsonl", help="corpus JSONL")
    parser.add_argument("--output", default="models/provider_eval.json", help="results JSON; per-row picks go beside it")
    parser.add_argument("--limit", type=int, default=None, help="limit windows")
    args = parser.parse_args()

    provider = create_provider(args.provider, args.model)
    with open(args.input) as f:
        examples = [json.loads(line) for line in f if line.strip()]
    print(f"Loaded {len(examples)} rows from {args.input}")

    results, rows = evaluate(provider, examples, args.limit)
    results["input"] = args.input

    window = results["window"]
    latency = results["latency"]
    memory = results["memory"]
    print("\n=== Window-pick suite ===")
    print(f"Provider: {results['provider']} ({results['model']})")
    print(f"Windows: {window['total_examples']} "
          f"({window['positive_examples']} positive, {window['negative_examples']} negative)")
    print(f"Pick accuracy: {window['element_accuracy']:.2%} on {window['answered']} answered")
    print(f"Abstain rate on positives: {window['abstain_rate_positive']:.2%}")
    for key in ("abstain_recall", "abstain_recall_production", "abstain_precision"):
        if window[key] is not None:
            print(f"{key}: {window[key]:.2%}")
    print(f"Point answers: {window['point_answers']}, errors: {window['errors']}")
    if latency["p50_s"] is not None:
        print(f"Latency: p50 {latency['p50_s']:.3f}s, p95 {latency['p95_s']:.3f}s "
              f"(first request {latency['first_request_s']:.1f}s)")
    mlx_peak = memory["mlx_peak_mb"]
    print(f"Peak RSS: {memory['peak_rss_mb']:.0f} MB"
          + (f", MLX peak: {mlx_peak:.0f} MB" if mlx_peak is not None else ""))

    out = Path(args.output)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(results, indent=2))
    picks_path = out.with_suffix(".picks.jsonl")
    with open(picks_path, "w") as f:
        for row in rows:
            f.write(json.dumps(row) + "\n")
    print(f"\nResults: {out}\nPer-row picks: {picks_path}")


if __name__ == "__main__":
    main()
