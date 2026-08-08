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
import sys
import time
from pathlib import Path

from mlx_finetune import build_completion, build_prompt


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
    if not isinstance(parsed, dict) or "action" not in parsed:
        return None
    return parsed


def generate_predictions(model, tokenizer, examples: list, max_tokens: int) -> list:
    from mlx_lm.generate import generate

    predictions = []
    for i, example in enumerate(examples):
        start = time.time()
        prompt = build_prompt(example)
        messages = [{"role": "user", "content": prompt}]
        text = tokenizer.apply_chat_template(messages, tokenize=False, add_generation_prompt=True)
        try:
            output = generate(model, tokenizer, prompt=text, max_tokens=max_tokens, verbose=False)
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
            "target": build_completion(example),
            "elapsed": time.time() - start,
        })
        if (i + 1) % 10 == 0:
            print(f"  Generated {i + 1}/{len(examples)}")
    return predictions


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
    args = parser.parse_args()

    from mlx_lm import load

    from fine_tune_vision import FineTuneConfig, VisionEvaluator

    with open(args.input, "r") as f:
        examples = [json.loads(line) for line in f if line.strip()]
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

    predictions = generate_predictions(model, tokenizer, examples, args.max_tokens)

    evaluator = VisionEvaluator(FineTuneConfig())
    results = evaluator.evaluate_predictions(predictions)

    print("\n=== Evaluation Results ===")
    print(f"Total examples: {results['total_examples']}")
    print(f"Successful predictions: {results['successful_predictions']}")
    print(f"Action accuracy: {results['action_accuracy']:.2%}")
    print(f"Coordinate accuracy (within 10px): {results['coord_accuracy_10px']:.2%}")
    print(f"Coordinate accuracy (within 50px): {results['coord_accuracy_50px']:.2%}")
    print(f"Coordinate MAE: {results['coord_mae']:.2f}")
    print(f"Avg confidence: {results['avg_confidence']:.4f}")
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
