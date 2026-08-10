#!/usr/bin/env python3
"""Out-of-sample corpus split for generalization checks.

In-sample adapter metrics are memorization; the honest number is a
journey the model never saw in training. This splits the merged corpus
by journey into train/test files for exactly that:

    python oos_split.py --input data/vision-corpus-all.jsonl \
        --holdout documents --out-dir data/

    python mlx_finetune.py --schema v1 --input data/oos_train.jsonl ...
    python evaluate_adapter.py --schema v1 --input data/oos_test.jsonl \
        --adapter models/.../adapters.safetensors ...
"""

import argparse
import json
from collections import Counter


def main():
    parser = argparse.ArgumentParser(description="Split corpus by held-out journey")
    parser.add_argument("--input", default="data/vision-corpus-all.jsonl")
    parser.add_argument("--holdout", required=True, help="journey to hold out")
    parser.add_argument("--out-dir", default="data")
    args = parser.parse_args()

    with open(args.input) as f:
        rows = [json.loads(line) for line in f if line.strip()]

    journeys = Counter(r.get("journey") for r in rows)
    print(f"journeys: {dict(journeys)}")
    if args.holdout not in journeys:
        raise SystemExit(
            f"holdout {args.holdout!r} not in corpus; expected one of: {sorted(journeys)}"
        )

    train = [r for r in rows if r.get("journey") != args.holdout]
    test = [r for r in rows if r.get("journey") == args.holdout]

    train_path = f"{args.out_dir}/oos_train.jsonl"
    test_path = f"{args.out_dir}/oos_test.jsonl"
    with open(train_path, "w") as f:
        for r in train:
            f.write(json.dumps(r) + "\n")
    with open(test_path, "w") as f:
        for r in test:
            f.write(json.dumps(r) + "\n")

    pos = sum(1 for r in train if r.get("target_index") is not None)
    print(f"train: {len(train)} ({pos} pos / {len(train) - pos} neg) -> {train_path}")
    print(f"test: {len(test)} (held-out: {args.holdout}) -> {test_path}")


if __name__ == "__main__":
    main()
