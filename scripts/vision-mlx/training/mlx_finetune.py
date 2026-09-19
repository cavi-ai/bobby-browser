#!/usr/bin/env python3
"""
MLX LoRA fine-tuning for Bobby's action-prediction model.

Converts Bobby training data (screenshot context + expected action JSON) into
mlx-lm's CompletionsDataset format and runs LoRA fine-tuning on a text model.
The prompt carries the page context (url, candidates, stuck state); the
completion is the action JSON. Images stay on the Ollama inference path until
MLX vision-encoder support lands.

Usage:
    python mlx_finetune.py \
        --model mlx-community/Qwen2.5-7B-Instruct-4bit \
        --input data/training_data.jsonl \
        --output models/ \
        --iters 300 --lora-rank 16
"""

import argparse
import hashlib
import json
import shutil
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path


@dataclass
class MLXFineTuneConfig:
    model_name: str = "mlx-community/Qwen2.5-7B-Instruct-4bit"
    input_path: str = "data/training_data.jsonl"
    output_dir: str = "models"
    iters: int = 300
    batch_size: int = 4
    learning_rate: float = 1e-5
    lora_rank: int = 16
    lora_alpha: float = 32.0
    lora_dropout: float = 0.05
    num_layers: int = 16
    max_seq_length: int = 1024
    steps_per_report: int = 10
    steps_per_eval: int = 100
    save_every: int = 100
    val_batches: int = 25
    seed: int = 42
    mask_prompt: bool = True
    train_ratio: float = 0.9
    schema: str = "candidate"  # candidate index is the privacy-safe corpus boundary


SYSTEM_PROMPT = (
    "You are a vision assistant for a browser automation agent called Bobby. "
    "Analyze the screenshot and return ONLY valid JSON matching this schema: "
    '{"confidence": 0.0..1.0, "action": {"kind": "click" | "typeText" | "extractValue", ...}}. '
    "Click coordinates are CSS pixels relative to the screenshot image."
)


CANDIDATE_SYSTEM_PROMPT = (
    "You are a vision assistant for a browser automation agent called Bobby. "
    "Analyze the screenshot and return ONLY valid JSON matching this schema: "
    '{"confidence": 0.0..1.0, "action": {"kind": "clickCandidate", "index": N} | '
    '{"kind": "typeIntoCandidate", "index": N} | '
    '{"kind": "extractFromCandidate", "index": N}}. '
    "The index refers to the numbered candidate list in the prompt."
)


def build_prompt(example: dict, schema: str = "candidate") -> str:
    example = normalize_corpus_example(example)
    if schema == "v1":
        return build_v1_prompt(example)
    user_text = f"purpose: {example['purpose']}\nintentKind: {example['intent_kind']}\nstuck: {example['stuck']}"
    if example.get("context_url"):
        user_text += f"\nurl: {example['context_url']}"
    candidates = example.get("context_candidates")
    if candidates:
        if schema == "candidate":
            user_text += "\ncandidates:"
            for i, c in enumerate(candidates):
                user_text += f"\n{i}: {c['role']} \"{c['name']}\""
        else:
            user_text += "\ncandidates:"
            for c in candidates:
                user_text += f"\n- {c['role']} \"{c['name']}\""
    system = CANDIDATE_SYSTEM_PROMPT if schema == "candidate" else SYSTEM_PROMPT
    return f"{system}\n\n{user_text}"


V1_STABLE_PREFIX = """BOBBY-VISION/1
ROLE: element selector for a browser automation runtime
RULES: reply with ONLY the index of the element that satisfies the task. No text, no JSON, no explanation. If nothing fits, reply -1."""


def build_v1_prompt(example: dict) -> str:
    """The BOBBY-VISION/1 wire format: stable prefix + varying block."""
    example = normalize_corpus_example(example)
    block = f"TASK: {example['purpose']}"
    # The intent's role hint narrows the candidate space ("Submit the form"
    # -> the button), exactly as LocateIntent.hints.role carries in
    # production. Emitted only when present.
    hint_role = example.get("hint_role")
    if hint_role:
        block += f"\nHINT: role={hint_role}"
    if example.get("context_url"):
        block += f"\nPAGE: {example['context_url']}"
    candidates = example.get("context_candidates") or []
    if candidates:
        rows = "\n".join(f"{i}|{c['role']}|{c['name']}" for i, c in enumerate(candidates))
        block += f"\nELEMENTS:\n{rows}"
    return f"{V1_STABLE_PREFIX}\n\n{block}"


def build_v1_completion(example: dict) -> str:
    """Bare index ground truth; -1 when the example has no valid target."""
    index = selected_index(example)
    return str(-1 if index is None else index)


def normalize_corpus_example(example: dict) -> dict:
    """Normalize the serialized Rust corpus schema without guessing payload fields."""
    normalized = dict(example)
    for camel, snake in (
        ("intentKind", "intent_kind"),
        ("contextUrl", "context_url"),
        ("contextCandidates", "context_candidates"),
        ("targetIndex", "target_index"),
        ("modelResponse", "model_response"),
        ("outcomeStage", "outcome_stage"),
        ("privacyVersion", "privacy_version"),
    ):
        if camel in example and snake in example and example[camel] != example[snake]:
            raise ValueError(f"conflicting {camel}/{snake} fields")
        if camel in example:
            normalized[snake] = example[camel]
    context = normalized.get("context")
    if isinstance(context, dict):
        for source, target in (
            ("url", "context_url"),
            ("candidates", "context_candidates"),
            ("recentCommandKinds", "context_recent_commands"),
        ):
            if source not in context:
                continue
            if target in normalized and normalized[target] != context[source]:
                raise ValueError(f"conflicting context.{source}/{target} fields")
            normalized[target] = context[source]
    return normalized


def corpus_privacy_errors(example: dict) -> list[str]:
    """Return reasons a record is unsafe to persist or train on."""
    example = normalize_corpus_example(example)
    errors = []
    if example.get("privacy_version") != 1:
        errors.append("privacy_version must be 1")

    url = example.get("context_url")
    if url:
        from urllib.parse import urlsplit

        parsed = urlsplit(url)
        if (
            parsed.scheme not in ("http", "https")
            or parsed.username is not None
            or parsed.password is not None
            or parsed.query
            or parsed.fragment
        ):
            errors.append("unsafe context_url")
        previous_sensitive = False
        for segment in parsed.path.split("/"):
            compact = "".join(ch for ch in segment if ch.isalnum())
            uuid_shape = len(segment) == 36 and all(
                segment[index] == "-" for index in (8, 13, 18, 23)
            )
            if previous_sensitive and segment != ":id":
                errors.append("unsafe context_url path segment")
            elif uuid_shape or (
                len(segment) >= 24
                and len(compact) * 10 >= len(segment) * 8
                and any(ch.isdigit() for ch in segment)
                and any(ch.isalpha() for ch in segment)
            ):
                errors.append("unsafe context_url path segment")
            previous_sensitive = segment.lower() in {
                "auth", "code", "credential", "invite", "key", "magic",
                "password", "reset", "secret", "session", "token",
            }

    action = model_response(example).get("action") or {}
    if action.get("kind") in ("typeText", "extractValue") or any(
        field in action for field in ("text", "value")
    ):
        errors.append("payload-bearing action is forbidden")

    forbidden_fields = {
        "password",
        "token",
        "secret",
        "authorization",
        "cookie",
        "modeltext",
        "modelextracted",
        "errormessage",
    }
    opaque_metadata_fields = {
        "imageb64",
        "imagehash",
        "contexturl",
        "timestamp",
    }

    def inspect(value, path="record"):
        if isinstance(value, dict):
            for key, child in value.items():
                compact = "".join(ch for ch in key.lower() if ch.isalnum())
                if compact in forbidden_fields:
                    errors.append(f"forbidden sensitive field {path}.{key}")
                if compact not in opaque_metadata_fields:
                    inspect(child, f"{path}.{key}")
        elif isinstance(value, list):
            for index, child in enumerate(value):
                inspect(child, f"{path}[{index}]")
        elif isinstance(value, str):
            lower = value.lower()
            explicit = any(
                marker in lower
                for marker in ("bearer ", "basic ", "sk-", "ghp_", "github_pat_", "akia")
            )
            tokens = [token.strip("\"'()[]{}<>,;:!?") for token in value.split()]
            email = any(
                "@" in token and "." in token.rsplit("@", 1)[-1]
                for token in tokens
            )
            high_entropy = any(
                len(token) >= 24
                and sum(ch.isalnum() for ch in token) * 10 >= len(token) * 8
                and any(ch.isdigit() for ch in token)
                and any(ch.isalpha() for ch in token)
                for token in tokens
            )
            if explicit or email or high_entropy:
                errors.append(f"sensitive value at {path}")

    inspect(example)
    return list(dict.fromkeys(errors))


def supervised_examples(examples: list, schema: str = "candidate") -> list:
    """Exclude diagnostic failure records from supervised model paths.

    V1 keeps abstain-labeled negatives (success=False with no target
    index): their completion is "-1", which is valid supervision for the
    abstain class, not a diagnostic (whitepaper §4e/§4f). Other schemas
    have no abstain target, so all failed records stay excluded.
    """
    supervised = []
    for example in examples:
        normalized = normalize_corpus_example(example)
        if normalized.get("success") is True:
            supervised.append(normalized)
        elif schema == "v1" and selected_index(normalized) is None:
            supervised.append(normalized)
    return supervised


def deduplicate_examples(examples: list) -> list:
    """Drop replayed supervision while preserving distinct targets."""
    unique = []
    seen = set()
    for example in examples:
        semantic = normalize_corpus_example(example)
        semantic = {
            key: value
            for key, value in semantic.items()
            if key not in {"timestamp", "run_id", "model_name", "image_hash"}
        }
        identity = json.dumps(semantic, sort_keys=True, separators=(",", ":"))
        if identity in seen:
            continue
        seen.add(identity)
        unique.append(example)
    return unique


def selected_index(example: dict):
    """Read the corpus boundary explicitly (Rust camelCase or legacy snake_case)."""
    camel = example.get("targetIndex")
    snake = example.get("target_index")
    if camel is not None and snake is not None and camel != snake:
        raise ValueError("conflicting target index fields")
    return camel if camel is not None else snake


def model_response(example: dict) -> dict:
    camel = example.get("modelResponse")
    snake = example.get("model_response")
    if camel is not None and snake is not None and camel != snake:
        raise ValueError("conflicting model response fields")
    return camel if camel is not None else (snake or {})


def build_completion(example: dict, schema: str = "candidate") -> str:
    example = normalize_corpus_example(example)
    if (
        example.get("success") is False
        and not (schema == "v1" and selected_index(example) is None)
    ):
        raise ValueError("unsuccessful corpus records are diagnostics, not supervised examples")
    if schema == "v1":
        return build_v1_completion(example)
    if schema == "candidate":
        response = model_response(example)
        confidence = response.get("confidence", 0.5)
        index = selected_index(example)
        if index is None:
            raise ValueError("candidate corpus record requires target_index")
        action = response.get("action", {})
        kind = action.get("kind", "click")
        if kind in ("typeText", "type_into_candidate", "typeIntoCandidate"):
            out_action = {"kind": "typeIntoCandidate", "index": index}
        elif kind in ("extractValue", "extract_from_candidate", "extractFromCandidate"):
            out_action = {"kind": "extractFromCandidate", "index": index}
        else:
            out_action = {"kind": "clickCandidate", "index": index}
        return json.dumps({"confidence": confidence, "action": out_action})

    response = model_response(example)
    action = response.get("action", {})
    if example.get("privacy_version") == 1 or action.get("kind") in (
        "clickCandidate",
        "typeIntoCandidate",
        "extractFromCandidate",
        "click_candidate",
        "type_into_candidate",
        "extract_from_candidate",
    ):
        raise ValueError("candidate-only corpus requires the candidate or v1 schema")
    confidence = response.get("confidence", 0.5)
    kind = action.get("kind")

    if kind == "click":
        out = {"confidence": confidence, "action": {"kind": "click", "x": action.get("x", 0.0), "y": action.get("y", 0.0)}}
    elif kind == "typeText":
        out = {"confidence": confidence, "action": {"kind": "typeText", "text": action.get("text", "")}}
    elif kind == "extractValue":
        out = {"confidence": confidence, "action": {"kind": "extractValue", "value": action.get("value", "")}}
    else:
        out = {"confidence": 0.5, "action": {"kind": "click", "x": 0.0, "y": 0.0}}
    return json.dumps(out)


def load_examples(path: str, schema: str = "candidate") -> list:
    examples = []
    with open(path, "r") as f:
        for line in f:
            if line.strip():
                example = json.loads(line)
                errors = corpus_privacy_errors(example)
                if errors:
                    raise ValueError("unsafe corpus record: " + "; ".join(errors))
                examples.append(example)
    return deduplicate_examples(supervised_examples(examples, schema))


def write_mlx_dataset(examples: list, out_dir: Path, train_ratio: float, seed: int, schema: str = "candidate") -> dict:
    import random

    records = [
        {"prompt": build_prompt(e, schema), "completion": build_completion(e, schema)}
        for e in examples
    ]
    rng = random.Random(seed)
    rng.shuffle(records)
    split = int(len(records) * train_ratio)
    train, valid = records[:split], records[split:]

    out_dir.mkdir(parents=True, exist_ok=True)
    for name, rows in (("train", train), ("valid", valid)):
        with open(out_dir / f"{name}.jsonl", "w") as f:
            for r in rows:
                f.write(json.dumps(r) + "\n")

    return {"train": len(train), "valid": len(valid), "data_dir": str(out_dir)}


def _sha256_file(path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def build_training_metadata(
    config: MLXFineTuneConfig,
    counts: dict,
    *,
    corpus_records: int,
    supervised_examples: int,
) -> dict:
    return {
        "base_model": config.model_name,
        "schema": config.schema,
        "adapter_file": "adapters.safetensors",
        "training_corpus": {
            "sha256": _sha256_file(config.input_path),
            "records": corpus_records,
            "supervised_examples": supervised_examples,
        },
        "iters": config.iters,
        "batch_size": config.batch_size,
        "learning_rate": config.learning_rate,
        "lora_rank": config.lora_rank,
        "lora_alpha": config.lora_alpha,
        "lora_dropout": config.lora_dropout,
        "num_layers": config.num_layers,
        "train_examples": counts["train"],
        "valid_examples": counts["valid"],
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "mode": "text-only LoRA (prompt carries page context; images stay on the Ollama path)",
    }


def run_mlx_finetune(config: MLXFineTuneConfig) -> dict:
    import types

    import mlx.optimizers as optim
    from mlx_lm import load
    from mlx_lm.tuner import TrainingArgs, train
    from mlx_lm.tuner.datasets import CacheDataset, load_dataset
    from mlx_lm.tuner.utils import linear_to_lora_layers

    with open(config.input_path) as corpus_source:
        corpus_records = sum(1 for line in corpus_source if line.strip())
    examples = load_examples(config.input_path, config.schema)
    if not examples:
        raise SystemExit(f"no training examples in {config.input_path}")

    output_path = Path(config.output_dir) / f"mlx-lora-bobby-{config.schema}"
    output_path.mkdir(parents=True, exist_ok=True)

    data_dir = Path(tempfile.mkdtemp(prefix="bobby-mlx-data-"))
    try:
        counts = write_mlx_dataset(examples, data_dir, config.train_ratio, config.seed, config.schema)
        print(f"Dataset: {counts['train']} train, {counts['valid']} valid")

        print(f"Loading model: {config.model_name}")
        model, tokenizer = load(config.model_name)

        model.freeze()
        lora_config = {
            "rank": config.lora_rank,
            "scale": config.lora_alpha,
            "dropout": config.lora_dropout,
        }
        linear_to_lora_layers(model, config.num_layers, lora_config)

        dataset_args = types.SimpleNamespace(
            data=str(data_dir),
            train=True,
            test=False,
            hf_dataset=False,
            mask_prompt=config.mask_prompt,
        )
        train_ds, valid_ds, _ = load_dataset(dataset_args, tokenizer)

        adapter_file = str(output_path / "adapters.safetensors")
        args = TrainingArgs(
            batch_size=config.batch_size,
            iters=config.iters,
            val_batches=config.val_batches,
            steps_per_report=config.steps_per_report,
            steps_per_eval=config.steps_per_eval,
            steps_per_save=config.save_every,
            max_seq_length=config.max_seq_length,
            adapter_file=adapter_file,
        )

        optimizer = optim.Adam(learning_rate=config.learning_rate)

        print(f"Training: {config.iters} iters, LoRA rank {config.lora_rank}, "
              f"{config.num_layers} layers, lr {config.learning_rate}")
        train(
            model=model,
            optimizer=optimizer,
            train_dataset=CacheDataset(train_ds),
            val_dataset=CacheDataset(valid_ds) if len(valid_ds) else None,
            args=args,
        )

        metadata = build_training_metadata(
            config,
            counts,
            corpus_records=corpus_records,
            supervised_examples=len(examples),
        )
        (output_path / "metadata.json").write_text(json.dumps(metadata, indent=2))

        # mlx_lm.load(adapter_path=...) requires adapter_config.json beside
        # the weights; without it the serving path (v1_provider) cannot load
        # the adapter at all. Write it at training time, not by hand later.
        adapter_config = {
            "fine_tune_type": "lora",
            "num_layers": config.num_layers,
            "lora_parameters": {
                "rank": config.lora_rank,
                "scale": config.lora_alpha,
                "dropout": config.lora_dropout,
            },
        }
        (output_path / "adapter_config.json").write_text(json.dumps(adapter_config, indent=2))
        print(f"\nAdapter saved: {adapter_file}")
        print(f"Metadata saved: {output_path / 'metadata.json'}")
        return metadata
    finally:
        shutil.rmtree(data_dir, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser(description="Bobby MLX LoRA fine-tuning")
    parser.add_argument("--model", default="mlx-community/Qwen2.5-7B-Instruct-4bit", help="MLX model (HF repo or local path)")
    parser.add_argument("--input", default="data/training_data.jsonl", help="Bobby training data JSONL")
    parser.add_argument("--output", default="models", help="Output directory")
    parser.add_argument("--iters", type=int, default=300, help="Training iterations")
    parser.add_argument("--batch-size", type=int, default=4, help="Batch size")
    parser.add_argument("--lr", type=float, default=1e-5, help="Learning rate")
    parser.add_argument("--lora-rank", type=int, default=16, help="LoRA rank")
    parser.add_argument("--lora-alpha", type=float, default=32.0, help="LoRA alpha (scale)")
    parser.add_argument("--lora-dropout", type=float, default=0.05, help="LoRA dropout")
    parser.add_argument("--num-layers", type=int, default=16, help="Number of trailing layers to convert to LoRA")
    parser.add_argument("--max-seq-length", type=int, default=1024, help="Max sequence length")
    parser.add_argument("--seed", type=int, default=42, help="Shuffle seed")
    parser.add_argument("--schema", choices=["coords", "candidate", "v1"], default="candidate", help="Output schema: candidate-index classification, x,y regression, or v1 index")
    args = parser.parse_args()

    config = MLXFineTuneConfig(
        model_name=args.model,
        input_path=args.input,
        output_dir=args.output,
        iters=args.iters,
        batch_size=args.batch_size,
        learning_rate=args.lr,
        lora_rank=args.lora_rank,
        lora_alpha=args.lora_alpha,
        lora_dropout=args.lora_dropout,
        num_layers=args.num_layers,
        max_seq_length=args.max_seq_length,
        seed=args.seed,
        schema=args.schema,
    )
    run_mlx_finetune(config)


if __name__ == "__main__":
    main()
