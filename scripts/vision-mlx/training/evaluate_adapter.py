#!/usr/bin/env python3
"""
Evaluate a Bobby MLX LoRA adapter against ground-truth training data.

Generates predictions with a base MLX model plus optional adapter weights,
parses each output as Bobby's action JSON, and scores with the same
VisionEvaluator the Ollama path uses (action accuracy, coordinate MAE,
journey success rates, confidence calibration).

Usage:
    python evaluate_adapter.py \
        --model mlx-community/Qwen2.5-7B-Instruct-4bit \
        --adapter models/mlx-lora-bobby/adapters.safetensors \
        --input data/training_data.jsonl \
        --output models/mlx-lora-bobby/

    # Base-model baseline (no adapter):
    python evaluate_adapter.py --model mlx-community/Qwen2.5-7B-Instruct-4bit \
        --input data/training_data.jsonl
"""

import argparse
import json
import math
import sys
import time
from pathlib import Path

from mlx_finetune import (
    build_completion,
    build_prompt,
    normalize_corpus_example,
    supervised_examples,
)

SUPPORTED_ACTION_KINDS = {
    "click",
    "typeText",
    "extractValue",
    "clickCandidate",
    "typeIntoCandidate",
    "extractFromCandidate",
}


def finite_number(value) -> bool:
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(value)
    )


def parse_prediction(text: str) -> dict | None:
    """Extract Bobby action JSON from generated text."""
    content = text.strip()
    if content.startswith("```json"):
        content = content[7:]
    if content.startswith("```"):
        content = content[3:]
    content = content.strip().rstrip("```").strip()

    start = content.find("{")
    end = content.rfind("}")
    if start == -1 or end == -1 or end <= start:
        return None
    try:
        parsed = json.loads(content[start : end + 1])
    except json.JSONDecodeError:
        return None
    if not isinstance(parsed, dict) or not isinstance(parsed.get("action"), dict):
        return None
    action = parsed["action"]
    if action.get("kind") not in SUPPORTED_ACTION_KINDS:
        return None
    if action.get("kind") == "click" and not (
        finite_number(action.get("x")) and finite_number(action.get("y"))
    ):
        return None
    if "confidence" in parsed and not finite_number(parsed["confidence"]):
        return None
    return parsed


def parse_v1_response(text: str, n_candidates: int) -> int | None:
    """Decode the strict BOBBY-VISION/1 wire format.

    Only a bare in-range integer or the explicit ``-1`` abstention token is
    valid. Malformed output is a parse failure, not an abstention.
    """
    stripped = text.strip()
    try:
        value = int(stripped)
    except ValueError:
        return None
    if value == -1:
        return -1
    if 0 <= value < n_candidates:
        return value
    return None


def generate_predictions(model, tokenizer, examples: list, max_tokens: int, schema: str = "coords") -> list:
    examples = supervised_examples(examples, schema)
    from mlx_lm.generate import generate

    predictions = []
    for i, example in enumerate(examples):
        start = time.time()
        prompt = build_prompt(example, schema)
        messages = [{"role": "user", "content": prompt}]
        template_kwargs = {"tokenize": False, "add_generation_prompt": True}
        if schema == "v1":
            # The v1 wire is a bare index; the instruct template's thinking
            # channel would emit an empty <think> wrapper and break strict
            # decoders. Qwen3 supports suppressing it at the template.
            try:
                text = tokenizer.apply_chat_template(
                    messages, enable_thinking=False, **template_kwargs
                )
            except TypeError:
                text = tokenizer.apply_chat_template(messages, **template_kwargs)
        else:
            text = tokenizer.apply_chat_template(messages, **template_kwargs)
        try:
            output = generate(model, tokenizer, prompt=text, max_tokens=max_tokens, verbose=False)
            if schema == "v1":
                n = len(example.get("context_candidates") or [])
                index = parse_v1_response(output, n)
                prediction = None if index is None else {"action": {"kind": "v1", "index": index}}
            else:
                prediction = parse_prediction(output)
        except Exception as e:
            print(f"  Error on example {i}: {e}")
            prediction = None
        predictions.append({
            "example_idx": i,
            "journey": example.get("journey", ""),
            "step": example.get("step", ""),
            "success": example.get("success", False),
            "prediction": prediction,
            "target": build_completion(example, schema),
            "elapsed": time.time() - start,
        })
        if (i + 1) % 10 == 0:
            print(f"  Generated {i + 1}/{len(examples)}")
    return predictions


def target_bbox(example: dict) -> dict | None:
    example = normalize_corpus_example(example)
    idx = example.get("target_index")
    candidates = example.get("context_candidates") or []
    if idx is None or idx >= len(candidates):
        return None
    return candidates[idx].get("bbox")


def point_in_bbox(x: float, y: float, bbox: dict) -> bool:
    if not finite_number(x) or not finite_number(y):
        return False
    return bbox["x"] <= x <= bbox["x"] + bbox["w"] and bbox["y"] <= y <= bbox["y"] + bbox["h"]


def element_accuracy(predictions: list, examples: list) -> dict:
    """Score target selection, payload, and full-action correctness separately.

    candidate kinds (clickCandidate/typeIntoCandidate/extractFromCandidate):
        index == target_index
    coords click: predicted (x, y) inside the target candidate's bbox
    coords typeText: the sole textbox is implied; text is scored as payload
    coords extractValue: the returned value identifies the unique target and payload
    """
    examples = [normalize_corpus_example(example) for example in examples]
    scored = 0
    correct = 0
    content_scored = 0
    content_correct = 0
    fully_correct = 0
    for p, e in zip(predictions, examples):
        pred = p.get("prediction")
        if pred is None:
            continue
        action = pred.get("action", {})
        kind = action.get("kind")
        target = e.get("targetIndex", e.get("target_index"))
        target_action = e.get("modelResponse", e.get("model_response", {})).get("action", {})
        expected_kind = target_action.get("kind")
        canonicalize = lambda value: {
            "clickCandidate": "click",
            "click_candidate": "click",
            "typeIntoCandidate": "typeText",
            "type_into_candidate": "typeText",
            "extractFromCandidate": "extractValue",
            "extract_from_candidate": "extractValue",
        }.get(value, value)
        canonical_kind = canonicalize(kind)
        expected_kind = canonicalize(expected_kind)

        if kind in (
            "clickCandidate",
            "click_candidate",
            "typeIntoCandidate",
            "type_into_candidate",
            "extractFromCandidate",
            "extract_from_candidate",
            "click",
            "typeText",
            "extractValue",
        ) and canonical_kind != expected_kind:
            scored += 1
            continue

        target_correct = False
        payload_required = False
        payload_correct = False
        if kind in ("clickCandidate", "click_candidate", "typeIntoCandidate", "type_into_candidate", "extractFromCandidate", "extract_from_candidate"):
            if target is None:
                continue
            scored += 1
            target_correct = action.get("index") == target
        elif kind == "click":
            bbox = target_bbox(e)
            if bbox is None:
                continue
            scored += 1
            target_correct = point_in_bbox(action.get("x", -1), action.get("y", -1), bbox)
        elif kind == "typeText":
            scored += 1
            target_correct = True  # the generated purpose names the sole textbox
            payload_required = True
            payload_correct = action.get("text") == target_action.get("text")
        elif kind == "extractValue":
            scored += 1
            target_correct = action.get("value") == target_action.get("value")
            payload_required = True
            payload_correct = target_correct
        else:
            continue

        if target_correct:
            correct += 1
        if payload_required:
            content_scored += 1
            if payload_correct:
                content_correct += 1
        if target_correct and (not payload_required or payload_correct):
            fully_correct += 1

    return {
        "scored": scored,
        "correct": correct,
        "element_accuracy": correct / scored if scored else 0.0,
        "content_scored": content_scored,
        "content_correct": content_correct,
        "content_accuracy": content_correct / content_scored if content_scored else 0.0,
        "fully_correct": fully_correct,
        "fully_correct_accuracy": fully_correct / scored if scored else 0.0,
    }


def is_correct(pred: dict | None, e: dict) -> bool | None:
    """Per-item element correctness. None = unscored (unparsed or untargeted)."""
    if pred is None:
        return None
    action = pred.get("action", {})
    kind = action.get("kind")
    target = e.get("target_index")
    target_action = e.get("model_response", {}).get("action", {})

    if kind in ("clickCandidate", "typeIntoCandidate", "extractFromCandidate"):
        if target is None:
            return None
        return action.get("index") == target
    if kind == "click":
        bbox = target_bbox(e)
        if bbox is None:
            return None
        return point_in_bbox(action.get("x", -1), action.get("y", -1), bbox)
    if kind == "typeText":
        return action.get("text") == target_action.get("text")
    if kind == "extractValue":
        return action.get("value") == target_action.get("value")
    return None


def calibration_metrics(predictions: list, examples: list) -> dict:
    """Confidence-vs-correctness analysis for the routing claim: does gating
    low-confidence predictions raise accuracy on the kept set?

    - ece: expected calibration error over 10 bins
    - selective: accuracy/coverage at each confidence threshold
    - separation: mean confidence of correct vs incorrect predictions
    """
    examples = [normalize_corpus_example(example) for example in examples]
    pairs = []
    for p, e in zip(predictions, examples):
        pred = p.get("prediction")
        correct = is_correct(pred, e)
        if correct is None:
            continue
        confidence = pred.get("confidence", 0.5)
        pairs.append((confidence, correct))

    if not pairs:
        return {"ece": 0.0, "selective": [], "separation": {}, "scored": 0}

    bins: list[list[tuple[float, bool]]] = [[] for _ in range(10)]
    for confidence, correct in pairs:
        bins[min(int(confidence * 10), 9)].append((confidence, correct))
    ece = 0.0
    for bucket in bins:
        if not bucket:
            continue
        acc = sum(1 for _, c in bucket if c) / len(bucket)
        conf = sum(c for c, _ in bucket) / len(bucket)
        ece += (len(bucket) / len(pairs)) * abs(acc - conf)

    selective = []
    for threshold in (0.5, 0.6, 0.7, 0.8, 0.85, 0.9, 0.95):
        kept = [(c, ok) for c, ok in pairs if c >= threshold]
        if kept:
            acc = sum(1 for _, ok in kept if ok) / len(kept)
            selective.append({
                "threshold": threshold,
                "coverage": len(kept) / len(pairs),
                "accuracy": acc,
            })

    correct_confs = [c for c, ok in pairs if ok]
    incorrect_confs = [c for c, ok in pairs if not ok]
    separation = {
        "correct_mean": sum(correct_confs) / len(correct_confs) if correct_confs else None,
        "incorrect_mean": sum(incorrect_confs) / len(incorrect_confs) if incorrect_confs else None,
        "incorrect_count": len(incorrect_confs),
    }

    return {"ece": ece, "selective": selective, "separation": separation, "scored": len(pairs)}


def v1_metrics(predictions: list, examples: list) -> dict:
    """BOBBY-VISION/1 scoring.

    Positive examples (target_index present): correct iff predicted index
    equals the target. Negative examples (flagged, no target): correct iff
    the model abstains (-1). Also reports abstention precision/recall — the
    routing signal confidence could not provide.
    """
    examples = [normalize_corpus_example(example) for example in examples]
    answered_scored = 0
    answered_correct = 0
    pos_total = 0
    pos_abstained = 0
    neg_total = 0
    neg_abstained = 0
    # Split the abstain class: production negatives (real escalations,
    # load-bearing) vs scripted ambiguous singletons (boundary research;
    # their recall flaps 66-100% between identical-config retrains at this
    # corpus scale). The gate floors them separately.
    neg_prod_total = 0
    neg_prod_abstained = 0

    for p, e in zip(predictions, examples):
        pred = p.get("prediction")
        index = None if pred is None else pred.get("action", {}).get("index")
        if e.get("negative") or e.get("target_index") is None:
            neg_total += 1
            is_ambiguous = e.get("outcome_stage") == "scriptedAmbiguous"
            if not is_ambiguous:
                neg_prod_total += 1
            if index == -1:
                neg_abstained += 1
                if not is_ambiguous:
                    neg_prod_abstained += 1
            continue
        pos_total += 1
        if index == -1:
            pos_abstained += 1
            continue
        if index is None:
            continue
        answered_scored += 1
        if index == e["target_index"]:
            answered_correct += 1

    return {
        "total_examples": len(predictions),
        "positive_examples": pos_total,
        "negative_examples": neg_total,
        "answered": answered_scored,
        "element_accuracy": answered_correct / answered_scored if answered_scored else 0.0,
        "abstain_rate_positive": pos_abstained / pos_total if pos_total else 0.0,
        "abstain_recall": neg_abstained / neg_total if neg_total else None,
        "abstain_recall_production": (
            neg_prod_abstained / neg_prod_total if neg_prod_total else None
        ),
        "abstain_precision": (
            neg_abstained / (neg_abstained + pos_abstained)
            if (neg_abstained + pos_abstained) else None
        ),
    }


def main():
    parser = argparse.ArgumentParser(description="Evaluate Bobby MLX LoRA adapter")
    parser.add_argument("--model", required=True, help="Base MLX model (HF repo or local path)")
    parser.add_argument("--adapter", default=None, help="Adapter .safetensors (omit for base-model baseline)")
    parser.add_argument("--input", default="data/training_data.jsonl", help="Eval data JSONL")
    parser.add_argument("--output", default=None, help="Directory for predictions/results (default: alongside adapter or cwd)")
    parser.add_argument("--limit", type=int, default=None, help="Evaluate only the first N examples")
    parser.add_argument("--max-tokens", type=int, default=256, help="Max generation tokens")
    parser.add_argument("--lora-rank", type=int, default=16, help="LoRA rank used at training time")
    parser.add_argument("--lora-alpha", type=float, default=32.0, help="LoRA alpha used at training time")
    parser.add_argument("--num-layers", type=int, default=16, help="Trailing LoRA layers used at training time")
    parser.add_argument("--schema", choices=["coords", "candidate", "v1"], default="coords", help="Output schema the model was trained with")
    args = parser.parse_args()

    from mlx_lm import load

    from fine_tune_vision import FineTuneConfig, VisionEvaluator

    with open(args.input, "r") as f:
        examples = supervised_examples([json.loads(line) for line in f if line.strip()], args.schema)
    if args.limit:
        examples = examples[: args.limit]
    print(f"Evaluating {len(examples)} examples on {args.model}"
          + (f" + adapter {args.adapter}" if args.adapter else " (base, no adapter)"))

    model, tokenizer = load(args.model)
    if args.adapter:
        from mlx_lm.tuner.utils import linear_to_lora_layers

        model.freeze()
        linear_to_lora_layers(
            model,
            args.num_layers,
            {"rank": args.lora_rank, "scale": args.lora_alpha, "dropout": 0.0},
        )
        model.load_weights(args.adapter, strict=False)

    predictions = generate_predictions(model, tokenizer, examples, args.max_tokens, args.schema)

    if args.schema == "v1":
        results = v1_metrics(predictions, examples)
        print("\n=== V1 Evaluation ===")
        print(f"Examples: {results['total_examples']} "
              f"({results['positive_examples']} positive, {results['negative_examples']} negative)")
        print(f"Element accuracy (answered): {results['element_accuracy']:.2%} "
              f"on {results['answered']} answered")
        print(f"Abstain rate on positives: {results['abstain_rate_positive']:.2%}")
        if results["abstain_recall"] is not None:
            print(f"Abstain recall (negatives caught): {results['abstain_recall']:.2%}")
        if results["abstain_precision"] is not None:
            print(f"Abstain precision: {results['abstain_precision']:.2%}")
    else:
        evaluator = VisionEvaluator(FineTuneConfig())
        results = evaluator.evaluate_predictions(predictions)
        results["element"] = element_accuracy(predictions, examples)
        results["calibration"] = calibration_metrics(predictions, examples)

    print("\n=== Evaluation Results ===")
    if args.schema == "v1":
        pass
    else:
        print(f"Total examples: {results['total_examples']}")
        print(f"Successful predictions: {results['successful_predictions']}")
        print(f"Action accuracy: {results['action_accuracy']:.2%}")
        print(f"Coordinate accuracy (within 10px): {results['coord_accuracy_10px']:.2%}")
        print(f"Coordinate accuracy (within 50px): {results['coord_accuracy_50px']:.2%}")
        print(f"Coordinate MAE: {results['coord_mae']:.2f}")
        print(f"Avg confidence: {results['avg_confidence']:.4f}")
        print(f"Element accuracy (correct target selected): "
              f"{results['element']['correct']}/{results['element']['scored']} "
              f"= {results['element']['element_accuracy']:.2%}")
        if results["element"]["content_scored"]:
            print(f"Content accuracy (text/value match): "
                  f"{results['element']['content_correct']}/{results['element']['content_scored']} "
                  f"= {results['element']['content_accuracy']:.2%}")
        print(f"Fully correct actions: "
              f"{results['element']['fully_correct']}/{results['element']['scored']} "
              f"= {results['element']['fully_correct_accuracy']:.2%}")
        cal = results["calibration"]
        if cal["scored"]:
            print(f"\nCalibration (ECE): {cal['ece']:.4f}")
            sep = cal["separation"]
            if sep.get("incorrect_count"):
                print(f"Confidence: correct mean {sep['correct_mean']:.3f} vs "
                      f"incorrect mean {sep['incorrect_mean']:.3f} "
                      f"({sep['incorrect_count']} errors)")
            print("Selective accuracy (confidence gate):")
            for row in cal["selective"]:
                print(f"  >= {row['threshold']:.2f}: {row['accuracy']:.2%} accurate "
                      f"on {row['coverage']:.0%} coverage")
        print("\nJourney success rates:")
        for journey, rate in results["journey_success_rates"].items():
            print(f"  {journey}: {rate:.2%}")

    out_dir = Path(args.output) if args.output else (
        Path(args.adapter).parent if args.adapter else Path(".")
    )
    out_dir.mkdir(parents=True, exist_ok=True)
    label = "adapter" if args.adapter else "base"

    pred_path = out_dir / f"{label}_predictions.jsonl"
    with open(pred_path, "w") as f:
        for p in predictions:
            f.write(json.dumps(p) + "\n")

    results_path = out_dir / f"{label}_evaluation.json"
    results_path.write_text(json.dumps(results, indent=2))
    print(f"\nPredictions: {pred_path}")
    print(f"Results: {results_path}")


if __name__ == "__main__":
    main()
