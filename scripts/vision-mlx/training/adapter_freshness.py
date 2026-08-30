#!/usr/bin/env python3
"""Adapter freshness gate: is the canonical adapter still good?

Runs the trained adapter against the standing evaluation suites and
fails when any metric drops below its floor:

  corpus (in-sample)   element accuracy + abstain recall/precision
  paraphrase probe     unambiguous-task robustness (holds the accept band)
  contrastive probe    boundary abstention: ambiguous rows must abstain,
                       disambiguated rows must resolve

Floors default to the current best adapter's measured cells; tighten
them only with a measured improvement, never to make a run pass.

    python adapter_freshness.py \
        --adapter models/v6-crowding/mlx-lora-bobby-v1 \
        --corpus data/vision-corpus-v7.jsonl

Needs MLX + the base model (this is the local/nightly tier; the CI tier
is corpus_lint.py, which needs no model). The live production-chain
control (intent_vision_collection::v1_positive_control) is the third
tier and runs with Chrome + a running proxy.
"""

import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).parent

SUITES = {
    "corpus": {
        # input supplied via --corpus
        "floors": {
            # Catastrophe bands, not best-observed cells: the #317-class
            # regression scored 0% recall; a healthy adapter has never
            # dropped below 90%. Precision flaps ±3 points between
            # identical-config retrains at this corpus scale (measured);
            # the floor sits below the flap band.
            "element_accuracy": 0.98,
            "abstain_recall": 0.90,
            "abstain_precision": 0.95,
        },
    },
    "paraphrase": {
        "input": "data/paraphrase-probe.jsonl",
        "floors": {
            "element_accuracy": 0.85,  # of answered
            "abstain_rate_positive": None,  # informational
        },
    },
    "contrastive": {
        "input": "data/contrastive-probe.jsonl",
        "floors": {
            "element_accuracy": 1.00,  # disambiguated rows must all resolve
            # 2/3 ambiguous rows abstained is the observed flap floor for
            # a healthy adapter; 0/3 is the no-abstention failure mode.
            "abstain_recall": 0.66,
            # False abstains on disambiguated rows are the safety signal
            # that never flaps — hold it at 100%.
            "abstain_precision": 1.00,
        },
    },
}


def run_suite(model, adapter, input_path, workdir):
    out_dir = tempfile.mkdtemp(prefix="freshness-")
    # Convention bridge: v1_provider (mlx_lm.load) takes the adapter
    # DIRECTORY; evaluate_adapter (mx.load) takes the .safetensors FILE.
    if adapter and Path(adapter).is_dir():
        adapter = str(Path(adapter) / "adapters.safetensors")
    cmd = [
        sys.executable,
        str(HERE / "evaluate_adapter.py"),
        "--schema",
        "v1",
        "--model",
        model,
        "--input",
        str(input_path),
        "--output",
        out_dir,
    ]
    if adapter:
        cmd += ["--adapter", str(adapter)]
    result = subprocess.run(cmd, cwd=workdir, capture_output=True, text=True)
    if result.returncode != 0:
        print(result.stdout[-2000:])
        print(result.stderr[-2000:], file=sys.stderr)
        raise SystemExit(f"evaluation failed for {input_path}")
    return json.loads((Path(out_dir) / "adapter_evaluation.json").read_text())


def check(suite, metrics, floors):
    failures = []
    for metric, floor in floors.items():
        if floor is None:
            continue
        value = metrics.get(metric)
        if value is None:
            failures.append(f"{metric}: missing (floor {floor})")
        elif value < floor:
            failures.append(f"{metric}: {value:.4f} below floor {floor}")
    return failures


def main():
    parser = argparse.ArgumentParser(description="Adapter freshness gate")
    parser.add_argument("--adapter", required=True, help="adapter directory (with adapter_config.json)")
    parser.add_argument("--model", default="mlx-community/Qwen2.5-7B-Instruct-4bit")
    parser.add_argument("--corpus", default="data/vision-corpus-v7.jsonl")
    parser.add_argument(
        "--suites",
        nargs="*",
        choices=list(SUITES),
        default=list(SUITES),
        help="subset of suites to run",
    )
    args = parser.parse_args()

    workdir = Path.cwd()
    all_failures = {}
    for name in args.suites:
        suite = SUITES[name]
        input_path = args.corpus if name == "corpus" else suite["input"]
        print(f"[{name}] evaluating {input_path} ...", flush=True)
        metrics = run_suite(args.model, args.adapter, input_path, workdir)
        failures = check(name, metrics, suite["floors"])
        summary = ", ".join(
            f"{k}={v:.4f}" if isinstance(v, float) else f"{k}={v}"
            for k, v in metrics.items()
            if k in ("element_accuracy", "abstain_recall", "abstain_precision", "abstain_rate_positive")
        )
        print(f"[{name}] {summary}")
        if failures:
            all_failures[name] = failures

    if all_failures:
        print("\nFRESHNESS GATE FAILED:")
        for name, failures in all_failures.items():
            for failure in failures:
                print(f"  [{name}] {failure}")
        sys.exit(1)
    print("\nfreshness gate: all suites pass")


if __name__ == "__main__":
    main()
