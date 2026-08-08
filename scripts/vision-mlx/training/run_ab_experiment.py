#!/usr/bin/env python3
"""
A/B experiment: candidate-index classification vs coordinate regression.

Generates one synthetic corpus, trains two adapters on identical data with
identical hyperparameters (only the output schema differs), then evaluates
base, coords-adapter, and candidate-adapter on the same held-out set.

The cross-schema metric is element accuracy: did the action select the
correct element? For candidate that is index == target_index; for coords it
is the predicted point landing inside the target's bbox.

Usage:
    python run_ab_experiment.py --model mlx-community/Qwen2.5-0.5B-Instruct-4bit \
        --n-train 200 --n-eval 30 --iters 100 --output runs/ab1
"""

import argparse
import json
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).parent


def run(cmd: list, label: str):
    print(f"\n=== {label} ===")
    t0 = time.time()
    r = subprocess.run(cmd, capture_output=True, text=True, cwd=HERE)
    if r.returncode != 0:
        print(r.stdout[-2000:])
        print(r.stderr[-2000:], file=sys.stderr)
        raise SystemExit(f"{label} failed (exit {r.returncode})")
    print(f"  ({time.time() - t0:.0f}s)")


def main():
    parser = argparse.ArgumentParser(description="A/B: candidate-index vs coordinate regression")
    parser.add_argument("--model", required=True, help="Base MLX model")
    parser.add_argument("--n-train", type=int, default=200)
    parser.add_argument("--n-eval", type=int, default=30)
    parser.add_argument("--iters", type=int, default=100)
    parser.add_argument("--batch-size", type=int, default=4)
    parser.add_argument("--num-layers", type=int, default=8)
    parser.add_argument("--lora-rank", type=int, default=16)
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--max-tokens", type=int, default=256, help="Generation cap for eval")
    parser.add_argument("--output", required=True, help="Run directory")
    args = parser.parse_args()

    out = Path(args.output)
    out.mkdir(parents=True, exist_ok=True)
    train_file = out / "train.jsonl"
    eval_file = out / "eval.jsonl"

    run([sys.executable, "gen_synthetic.py", "--n", str(args.n_train),
         "--seed", str(args.seed), "--output", str(train_file)], "generate train")
    run([sys.executable, "gen_synthetic.py", "--n", str(args.n_eval),
         "--seed", str(args.seed + 1000), "--output", str(eval_file)], "generate eval")

    results = {}
    for schema in ("coords", "candidate"):
        run([sys.executable, "fine_tune_vision.py",
             "--model", args.model, "--input", str(train_file),
             "--output", str(out), "--iters", str(args.iters),
             "--batch-size", str(args.batch_size), "--num-layers", str(args.num_layers),
             "--lora-rank", str(args.lora_rank), "--seed", str(args.seed),
             "--schema", schema], f"train {schema}")

        adapter = out / f"mlx-lora-bobby-{schema}" / "adapters.safetensors"

        for variant, adapter_arg in (("base", None), ("adapter", str(adapter))):
            label = f"{schema}-{variant}"
            eval_out = out / label
            cmd = [sys.executable, "evaluate_adapter.py",
                   "--model", args.model, "--input", str(eval_file),
                   "--output", str(eval_out), "--schema", schema,
                   "--num-layers", str(args.num_layers), "--lora-rank", str(args.lora_rank),
                   "--max-tokens", str(args.max_tokens)]
            if adapter_arg:
                cmd += ["--adapter", adapter_arg]
            run(cmd, f"eval {label}")
            results[label] = json.loads((eval_out / f"{variant}_evaluation.json").read_text())

    print("\n=== RESULTS ===")
    print(
        f"{'variant':<22} {'parse rate':>11} {'kind acc':>10} "
        f"{'target acc':>11} {'payload acc':>12} {'full acc':>10} {'coord MAE':>10}"
    )
    for label, r in results.items():
        total = r["total_examples"]
        parsed = r["successful_predictions"]
        metrics = r["element"]
        mae = r["coord_mae"]
        print(
            f"{label:<22} {parsed/total:>10.0%} {r['action_accuracy']:>9.2%} "
            f"{metrics['element_accuracy']:>10.2%} {metrics['content_accuracy']:>11.2%} "
            f"{metrics['fully_correct_accuracy']:>9.2%} {mae:>10.1f}"
        )

    summary = {
        "config": vars(args),
        "results": {k: {
            "parse_rate": v["successful_predictions"] / v["total_examples"],
            "element_accuracy": v["element"]["element_accuracy"],
            "content_accuracy": v["element"]["content_accuracy"],
            "fully_correct_accuracy": v["element"]["fully_correct_accuracy"],
            "coord_mae": v["coord_mae"],
            "action_accuracy": v["action_accuracy"],
        } for k, v in results.items()},
    }
    (out / "ab_summary.json").write_text(json.dumps(summary, indent=2))
    print(f"\nSummary: {out / 'ab_summary.json'}")


if __name__ == "__main__":
    main()
