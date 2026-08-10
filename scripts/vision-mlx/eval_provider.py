#!/usr/bin/env python3
"""Evaluate a canonical vision provider on Bobby training data.

Runs any registered provider (mlx-vlm, ollama, lmstudio) over a JSONL
training set and reports action accuracy, coordinate error, and
per-journey success rates.

Usage:
    python eval_provider.py                                    # ollama, default data
    python eval_provider.py --provider ollama --limit 50
    python eval_provider.py --provider mlx-vlm --model mlx-community/Qwen2.5-VL-7B-Instruct-4bit
"""

import argparse
import json
import os
import sys
import time
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from providers import create_provider, ProposeRequest


def _target_from_example(ex: dict) -> dict:
    """Build the canonical target from either flat collector fields or a
    nested model_response dict."""
    if ex.get("model_response"):
        return ex["model_response"]
    kind = ex.get("model_action_kind", "click")
    action = {"kind": kind}
    if kind == "click":
        action["x"] = ex.get("model_click_x", 0.0)
        action["y"] = ex.get("model_click_y", 0.0)
    elif kind == "typeText":
        action["text"] = ex.get("model_text", "")
    elif kind == "extractValue":
        action["value"] = ex.get("model_extracted", "")
    return {"confidence": ex.get("model_confidence", 0.5), "action": action}


def evaluate(provider, examples: list, limit: int | None = None) -> dict:
    if limit:
        examples = examples[:limit]
    print(f"Evaluating {provider.name} on {len(examples)} examples...")

    results = []
    for i, ex in enumerate(examples):
        start = time.time()
        try:
            req = ProposeRequest(
                purpose=ex["purpose"],
                intent_kind=ex["intent_kind"],
                stuck=ex["stuck"],
                screenshot_b64=ex["image_b64"],
            )
            resp = provider.propose(req)
            pred = resp.to_dict()
            error = None
        except Exception as e:
            pred = None
            error = str(e)
        elapsed = time.time() - start
        results.append({
            "journey": ex.get("journey", "unknown"),
            "success_expected": ex.get("success", False),
            "target": _target_from_example(ex),
            "prediction": pred,
            "error": error,
            "elapsed": elapsed,
        })
        if (i + 1) % 10 == 0:
            print(f"  {i + 1}/{len(examples)} ({elapsed:.1f}s last)")

    total = len(results)
    succeeded = [r for r in results if r["prediction"] is not None]
    failed = total - len(succeeded)

    action_correct = 0
    coords = []
    journey_stats = {}

    for r in results:
        pred = r["prediction"]
        target = r["target"]
        journey = r["journey"]
        journey_stats.setdefault(journey, {"total": 0, "correct": 0})
        journey_stats[journey]["total"] += 1
        if pred is None:
            continue
        target_action = target.get("action", {})
        pred_action = pred.get("action", {})
        if pred_action.get("kind") == target_action.get("kind"):
            action_correct += 1
            journey_stats[journey]["correct"] += 1
        if (pred_action.get("kind") == "click" and target_action.get("kind") == "click"):
            dx = pred_action.get("x", 0) - target_action.get("x", 0)
            dy = pred_action.get("y", 0) - target_action.get("y", 0)
            coords.append((dx * dx + dy * dy) ** 0.5)

    coord_mae = float(np.mean(coords)) if coords else 0.0
    coord_10px = sum(1 for d in coords if d < 10) / len(coords) if coords else 0.0
    coord_50px = sum(1 for d in coords if d < 50) / len(coords) if coords else 0.0

    return {
        "provider": provider.name,
        "total_examples": total,
        "successful_predictions": len(succeeded),
        "failed_predictions": failed,
        "action_accuracy": action_correct / len(succeeded) if succeeded else 0.0,
        "coord_mae": coord_mae,
        "coord_accuracy_10px": coord_10px,
        "coord_accuracy_50px": coord_50px,
        "journey_success_rates": {
            j: s["correct"] / s["total"] for j, s in journey_stats.items() if s["total"] > 0
        },
        "avg_latency_s": float(np.mean([r["elapsed"] for r in results])),
    }


def main():
    parser = argparse.ArgumentParser(description="Evaluate a canonical vision provider")
    parser.add_argument("--provider", default=None, help="mlx-vlm | ollama | lmstudio")
    parser.add_argument("--model", default=None, help="model override")
    parser.add_argument("--input", default="data/training_data.jsonl", help="training data")
    parser.add_argument("--output", default="models/provider_eval.json", help="results output")
    parser.add_argument("--limit", type=int, default=None, help="limit examples")
    args = parser.parse_args()

    if args.model:
        os.environ["VISION_MLX_MODEL"] = args.model
    provider = create_provider(args.provider)

    examples = []
    with open(args.input) as f:
        for line in f:
            if line.strip():
                examples.append(json.loads(line))
    print(f"Loaded {len(examples)} examples from {args.input}")

    results = evaluate(provider, examples, args.limit)

    print("\n=== Evaluation Results ===")
    print(f"Provider: {results['provider']}")
    print(f"Total: {results['total_examples']}, Successful: {results['successful_predictions']}, Failed: {results['failed_predictions']}")
    print(f"Action accuracy: {results['action_accuracy']:.2%}")
    print(f"Coord MAE: {results['coord_mae']:.1f}px (within 10px: {results['coord_accuracy_10px']:.2%}, 50px: {results['coord_accuracy_50px']:.2%})")
    print(f"Avg latency: {results['avg_latency_s']:.2f}s")
    print("Journey success rates:")
    for journey, rate in results["journey_success_rates"].items():
        print(f"  {journey}: {rate:.2%}")

    out = Path(args.output)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(results, indent=2))
    print(f"\nResults saved to: {out}")


if __name__ == "__main__":
    main()
