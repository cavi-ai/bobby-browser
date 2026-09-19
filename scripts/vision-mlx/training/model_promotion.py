#!/usr/bin/env python3
"""Promote a measured Bobby vision adapter into an immutable local release."""

import argparse
import hashlib
import json
import os
import re
import shutil
import tempfile
from pathlib import Path

from corpus_lint import lint
from mlx_finetune import normalize_corpus_example


ASSESSMENT_SCHEMA_VERSION = 1
PROMOTION_SCHEMA_VERSION = 1
VERSION_PATTERN = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
REQUIRED_SUITES = frozenset({"corpus", "paraphrase", "contrastive"})
PROMOTION_FLOORS = {
    "corpus": {
        "element_accuracy": 0.98,
        "abstain_recall_production": 1.0,
        "abstain_recall_singletons": 0.5,
        "abstain_precision": 0.95,
    },
    "paraphrase": {"element_accuracy": 0.85},
    "contrastive": {
        "element_accuracy": 1.0,
        "abstain_recall": 0.66,
        "abstain_precision": 1.0,
    },
}


class PromotionError(ValueError):
    pass


def sha256_file(path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _adapter_files(adapter_dir: Path) -> list[Path]:
    if not adapter_dir.is_dir():
        raise PromotionError(f"adapter directory does not exist: {adapter_dir}")
    files = []
    for path in adapter_dir.rglob("*"):
        if path.is_symlink():
            raise PromotionError(f"adapter contains a symlink: {path.relative_to(adapter_dir)}")
        if path.is_file() and path.name not in {"assessment.json", "promotion.json"}:
            files.append(path)
    required = {"adapters.safetensors", "adapter_config.json", "metadata.json"}
    missing = required.difference(path.name for path in files)
    if missing:
        raise PromotionError(f"adapter is missing required files: {', '.join(sorted(missing))}")
    return sorted(files, key=lambda path: path.relative_to(adapter_dir).as_posix())


def adapter_digest(adapter_dir) -> str:
    adapter_dir = Path(adapter_dir)
    digest = hashlib.sha256()
    for path in _adapter_files(adapter_dir):
        relative = path.relative_to(adapter_dir).as_posix().encode()
        payload_digest = bytes.fromhex(sha256_file(path))
        digest.update(len(relative).to_bytes(4, "big"))
        digest.update(relative)
        digest.update(payload_digest)
    return digest.hexdigest()


def _load_corpus(corpus_path: Path) -> list[dict]:
    rows = []
    try:
        with corpus_path.open() as source:
            for line_number, line in enumerate(source, start=1):
                if not line.strip():
                    continue
                try:
                    rows.append(normalize_corpus_example(json.loads(line)))
                except (json.JSONDecodeError, ValueError) as error:
                    raise PromotionError(
                        f"invalid corpus record at line {line_number}: {error}"
                    ) from error
    except OSError as error:
        raise PromotionError(f"cannot read corpus: {error}") from error
    if not rows:
        raise PromotionError("corpus is empty")
    errors, _ = lint(rows)
    if errors:
        raise PromotionError("unsafe corpus: " + "; ".join(errors))
    return rows


def _validated_suites(suites: dict) -> dict:
    if not isinstance(suites, dict) or not suites:
        raise PromotionError("at least one evaluation suite is required")
    missing = REQUIRED_SUITES.difference(suites)
    if missing:
        raise PromotionError(f"missing required suites: {', '.join(sorted(missing))}")
    failures = []
    normalized = {}
    for suite_name in sorted(suites):
        suite = suites[suite_name]
        metrics = suite.get("metrics") if isinstance(suite, dict) else None
        floors = suite.get("floors") if isinstance(suite, dict) else None
        if not isinstance(metrics, dict) or not isinstance(floors, dict) or not floors:
            raise PromotionError(f"suite {suite_name!r} requires metrics and floors")
        canonical_floors = PROMOTION_FLOORS.get(suite_name, {})
        for metric_name, canonical_floor in canonical_floors.items():
            if floors.get(metric_name) != canonical_floor:
                raise PromotionError(
                    f"suite {suite_name!r} floor {metric_name!r} must be {canonical_floor}"
                )
        for metric_name, floor in floors.items():
            if floor is None:
                continue
            value = metrics.get(metric_name)
            if not isinstance(value, (int, float)) or isinstance(value, bool):
                failures.append(f"{suite_name}.{metric_name} is missing")
            elif value < floor:
                failures.append(
                    f"{suite_name}.{metric_name}={value} below floor {floor}"
                )
        normalized[suite_name] = {"metrics": metrics, "floors": floors}
    if failures:
        raise PromotionError("evaluation failed: " + "; ".join(failures))
    return normalized


def _load_adapter_metadata(adapter_dir: Path) -> dict:
    try:
        metadata = json.loads((adapter_dir / "metadata.json").read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise PromotionError(f"invalid adapter metadata: {error}") from error
    training_data = metadata.get("training_data")
    if training_data and Path(training_data).is_absolute():
        raise PromotionError("adapter metadata contains a private training path")
    return metadata


def _validate_model_identity(base_model: str, schema: str):
    if not base_model or Path(base_model).is_absolute() or base_model.startswith(("~", "file:")):
        raise PromotionError("base model must be a portable model identifier")
    if schema not in {"candidate", "v1"}:
        raise PromotionError("promotion requires candidate or v1 schema")


def build_assessment(adapter_dir, corpus_path, base_model, schema, suites) -> dict:
    adapter_dir = Path(adapter_dir)
    corpus_path = Path(corpus_path)
    _validate_model_identity(base_model, schema)
    rows = _load_corpus(corpus_path)
    metadata = _load_adapter_metadata(adapter_dir)
    if metadata.get("base_model") != base_model:
        raise PromotionError("base model does not match adapter metadata")
    if metadata.get("schema") != schema:
        raise PromotionError("schema does not match adapter metadata")
    return {
        "schemaVersion": ASSESSMENT_SCHEMA_VERSION,
        "verdict": "passed",
        "adapter": {
            "sha256": adapter_digest(adapter_dir),
            "baseModel": base_model,
            "schema": schema,
        },
        "corpus": {
            "sha256": sha256_file(corpus_path),
            "records": len(rows),
        },
        "suites": _validated_suites(suites),
    }


def _read_assessment(path: Path) -> dict:
    try:
        assessment = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise PromotionError(f"invalid assessment: {error}") from error
    if assessment.get("schemaVersion") != ASSESSMENT_SCHEMA_VERSION:
        raise PromotionError("unsupported assessment schema version")
    if assessment.get("verdict") != "passed":
        raise PromotionError("assessment verdict is not passed")
    _validated_suites(assessment.get("suites"))
    return assessment


def _write_private_json(path: Path, value: dict):
    path.write_text(json.dumps(value, sort_keys=True, indent=2) + "\n")
    os.chmod(path, 0o600)


def _set_current(store_dir: Path, version: str, digest: str) -> dict:
    current = {"version": version, "adapterSha256": digest}
    link_fd, link_name = tempfile.mkstemp(prefix=".current-link-", dir=store_dir)
    os.close(link_fd)
    os.unlink(link_name)
    try:
        os.symlink(Path("releases") / version, link_name, target_is_directory=True)
        os.replace(link_name, store_dir / "current")
    finally:
        if os.path.lexists(link_name):
            os.unlink(link_name)
    return current


def promote_adapter(
    adapter_dir,
    corpus_path,
    assessment_path,
    store_dir,
    version,
) -> Path:
    adapter_dir = Path(adapter_dir)
    corpus_path = Path(corpus_path)
    assessment_path = Path(assessment_path)
    store_dir = Path(store_dir)
    if not VERSION_PATTERN.fullmatch(version):
        raise PromotionError("version must be a portable release identifier")

    assessment = _read_assessment(assessment_path)
    rows = _load_corpus(corpus_path)
    actual_corpus_digest = sha256_file(corpus_path)
    if assessment.get("corpus", {}).get("sha256") != actual_corpus_digest:
        raise PromotionError("corpus digest does not match assessment")
    if assessment.get("corpus", {}).get("records") != len(rows):
        raise PromotionError("corpus record count does not match assessment")

    actual_adapter_digest = adapter_digest(adapter_dir)
    adapter_claim = assessment.get("adapter", {})
    if adapter_claim.get("sha256") != actual_adapter_digest:
        raise PromotionError("adapter digest does not match assessment")
    metadata = _load_adapter_metadata(adapter_dir)
    if metadata.get("base_model") != adapter_claim.get("baseModel"):
        raise PromotionError("base model does not match adapter metadata")
    if metadata.get("schema") != adapter_claim.get("schema"):
        raise PromotionError("schema does not match adapter metadata")

    releases = store_dir / "releases"
    release = releases / version
    store_dir.mkdir(parents=True, exist_ok=True)
    releases.mkdir(parents=True, exist_ok=True)
    os.chmod(store_dir, 0o700)
    os.chmod(releases, 0o700)
    if release.exists():
        raise PromotionError(f"release version {version!r} already exists")

    assessment_digest = sha256_file(assessment_path)
    manifest = {
        "schemaVersion": PROMOTION_SCHEMA_VERSION,
        "version": version,
        "adapter": adapter_claim,
        "corpus": assessment["corpus"],
        "assessmentSha256": assessment_digest,
        "suites": assessment["suites"],
    }

    stage = Path(tempfile.mkdtemp(prefix=".promotion-", dir=releases))
    try:
        os.chmod(stage, 0o700)
        for source in _adapter_files(adapter_dir):
            relative = source.relative_to(adapter_dir)
            destination = stage / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            os.chmod(destination.parent, 0o700)
            shutil.copyfile(source, destination)
            os.chmod(destination, 0o600)
        shutil.copyfile(assessment_path, stage / "assessment.json")
        os.chmod(stage / "assessment.json", 0o600)
        _write_private_json(stage / "promotion.json", manifest)
        os.replace(stage, release)
    except Exception:
        shutil.rmtree(stage, ignore_errors=True)
        raise

    return release


def activate_release(store_dir, version) -> dict:
    store_dir = Path(store_dir)
    if not VERSION_PATTERN.fullmatch(version):
        raise PromotionError("version must be a portable release identifier")
    release = store_dir / "releases" / version
    try:
        manifest = json.loads((release / "promotion.json").read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise PromotionError(f"invalid promoted release: {error}") from error
    if manifest.get("schemaVersion") != PROMOTION_SCHEMA_VERSION:
        raise PromotionError("unsupported promotion schema version")
    if manifest.get("version") != version:
        raise PromotionError("release version does not match manifest")
    expected_assessment = manifest.get("assessmentSha256")
    if sha256_file(release / "assessment.json") != expected_assessment:
        raise PromotionError("assessment digest does not match manifest")
    expected_adapter = manifest.get("adapter", {}).get("sha256")
    actual_adapter = adapter_digest(release)
    if actual_adapter != expected_adapter:
        raise PromotionError("adapter digest does not match manifest")
    return _set_current(store_dir, version, actual_adapter)


def main():
    parser = argparse.ArgumentParser(description="Manage verified Bobby vision adapters")
    commands = parser.add_subparsers(dest="command", required=True)
    promote = commands.add_parser("promote", help="create an immutable release")
    promote.add_argument("--adapter", required=True, help="adapter directory")
    promote.add_argument("--corpus", required=True, help="sanitized corpus JSONL")
    promote.add_argument("--assessment", required=True, help="passing assessment JSON")
    promote.add_argument("--store", required=True, help="local promotion store")
    promote.add_argument("--version", required=True, help="immutable release version")
    activate = commands.add_parser("activate", help="activate a verified existing release")
    activate.add_argument("--store", required=True, help="local promotion store")
    activate.add_argument("--version", required=True, help="release version")
    args = parser.parse_args()
    try:
        if args.command == "promote":
            result = promote_adapter(
                args.adapter,
                args.corpus,
                args.assessment,
                args.store,
                args.version,
            )
        else:
            result = activate_release(args.store, args.version)
    except PromotionError as error:
        parser.exit(1, f"promotion failed: {error}\n")
    print(result if isinstance(result, Path) else json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
