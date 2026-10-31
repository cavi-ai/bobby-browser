#!/usr/bin/env python3
"""Vision suites: score a serving provider or a text reader against verified
ground truth, with latency percentiles and the peak memory of this process
(meaningful for in-process MLX providers and readers).

Suites:
  window     corpus windows (purpose + candidates); a candidate index at or
             above the runtime confidence floor is a pick, anything below the
             floor an abstention.
  grounding  perception rows; one locate request per control whose visible
             name is unique on its page, no candidates; the click point is
             scored by the box it lands in.
  challenge  perception rows; one detectChallenge request per page, scored
             against the class named by the page's structural markers.
  ocr        perception rows; one crop per single-line text box, read by a
             text reader (tesseract, apple-vision, mlx-vlm), exact match.

Usage:
    python eval_provider.py --provider v1 --input data/oos_test.jsonl
    python eval_provider.py --suite grounding --provider mlx-vlm \
        --model mlx-community/Qwen3.5-27B-4bit --input perception.jsonl
    python eval_provider.py --suite ocr --reader tesseract --input perception.jsonl
"""

import argparse
import base64
import io
import json
import math
import resource
import subprocess
import sys
import tempfile
import time
from collections import Counter
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

SUITES = ("window", "grounding", "challenge", "ocr")


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


def latency(timings: list) -> dict:
    # The first request carries the model load.
    steady = timings[1:] or timings
    return {
        "first_request_s": timings[0] if timings else None,
        "p50_s": percentile(steady, 0.50),
        "p95_s": percentile(steady, 0.95),
        "mean_s": sum(steady) / len(steady) if steady else None,
    }


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


def timed(call):
    """Run `call`, returning (value, error class name, seconds)."""
    start = time.perf_counter()
    try:
        value, error = call(), None
    except Exception as e:
        value, error = None, type(e).__name__
    return value, error, time.perf_counter() - start


def model_of(source) -> str | None:
    return getattr(source, "model_name", None) or getattr(source, "model", None)


def evaluate(provider, examples: list, limit: int | None = None) -> tuple[dict, list]:
    """Window-pick suite over corpus examples."""
    examples = supervised_examples(examples, "v1")
    if limit:
        examples = examples[:limit]
    print(f"Evaluating {provider.name} on {len(examples)} windows...")

    rows = []
    for i, example in enumerate(examples):
        response, error, elapsed = timed(lambda: provider.propose(request_for(example)).to_dict())
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

    picks = [
        {"prediction": None if r["pick"] is None else {"action": {"index": r["pick"]}}}
        for r in rows
    ]
    window = v1_metrics(picks, examples)
    window["point_answers"] = sum(1 for r in rows if r["pick"] is None)
    window["errors"] = sum(1 for r in rows if r["error"] is not None)
    results = {
        "provider": provider.name,
        "model": model_of(provider),
        "confidence_floor": VISION_CONFIDENCE_FLOOR,
        "window": window,
        "calibration": calibration_metrics(
            [{"prediction": r["response"]} for r in rows], examples
        ),
        "latency": latency([r["elapsed"] for r in rows]),
        "memory": peak_memory_mb(),
    }
    return results, rows


# -- perception rows (bobby-perception/1) -----------------------------------

# Name sources a person reads on screen; aria and title names are not visible.
VISIBLE_NAME_SOURCES = ("text", "label")


def inside(x: float, y: float, bbox: dict) -> bool:
    return bbox["x"] <= x <= bbox["x"] + bbox["w"] and bbox["y"] <= y <= bbox["y"] + bbox["h"]


def grounding_targets(row: dict) -> list:
    elements = row["elements"]
    counts = Counter((e["role"], e["name"]) for e in elements)
    return [
        e for e in elements
        if e["name"] and e["nameSource"] in VISIBLE_NAME_SOURCES
        and counts[(e["role"], e["name"])] == 1
    ]


def grounding_outcome(response: dict | None, target: dict, elements: list, dpr: float) -> str:
    if response is None:
        return "error"
    if response.get("confidence", 0.0) < VISION_CONFIDENCE_FLOOR:
        return "abstain"
    action = response.get("action", {})
    if action.get("kind") != "click":
        return "offContract"
    # Click points are screenshot pixels; boxes are CSS pixels.
    x, y = action["x"] / dpr, action["y"] / dpr
    if inside(x, y, target["bbox"]):
        return "hit"
    if any(inside(x, y, e["bbox"]) for e in elements if e is not target):
        return "wrongElement"
    return "offTarget"


def evaluate_grounding(provider, rows: list, limit: int | None = None) -> tuple[dict, list]:
    queries = [(row, target) for row in rows for target in grounding_targets(row)]
    if limit:
        queries = queries[:limit]
    print(f"Grounding {provider.name} on {len(queries)} controls...")

    records = []
    for row, target in queries:
        request = ProposeRequest(
            purpose=f'Click the "{target["name"]}" {target["role"]}',
            intent_kind="locate",
            stuck="targetMissing",
            screenshot_b64=row["imageB64"],
        )
        response, error, elapsed = timed(lambda: provider.propose(request).to_dict())
        records.append({
            "page": row["page"],
            "role": target["role"],
            "name": target["name"],
            "bbox": target["bbox"],
            "response": response,
            "outcome": grounding_outcome(response, target, row["elements"], row["viewport"]["dpr"]),
            "error": error,
            "elapsed": elapsed,
        })

    outcomes = Counter(r["outcome"] for r in records)
    answered = outcomes["hit"] + outcomes["wrongElement"] + outcomes["offTarget"]
    grounding = {
        "queries": len(records),
        **{key: outcomes[key] for key in ("hit", "wrongElement", "offTarget", "abstain", "offContract", "error")},
        "hit_rate": outcomes["hit"] / len(records) if records else 0.0,
        "hit_rate_answered": outcomes["hit"] / answered if answered else 0.0,
    }
    results = {
        "provider": provider.name,
        "model": model_of(provider),
        "confidence_floor": VISION_CONFIDENCE_FLOOR,
        "grounding": grounding,
        "latency": latency([r["elapsed"] for r in records]),
        "memory": peak_memory_mb(),
    }
    return results, records


CHALLENGE_PURPOSE = "Check whether this page shows a captcha or verification challenge"

# Mirrors ChallengeType in crates/bobby-browser-client/src/challenges.rs; the
# runtime fails a detection naming any other type.
CHALLENGE_TYPES = frozenset((
    "recaptchaV2Checkbox", "recaptchaV3", "textCaptcha", "imageGridCaptcha", "mfaCodeEntry",
))


def challenge_answer(response: dict | None) -> str:
    """The class the runtime records; detections are kept at any confidence."""
    action = (response or {}).get("action", {})
    if action.get("kind") == "challengeDetected":
        kind = action.get("challengeType")
        return kind if kind in CHALLENGE_TYPES else "invalid"
    if action.get("kind") == "noChallengeDetected":
        return "none"
    return "invalid"


def classification_metrics(pairs: list) -> dict:
    """Accuracy, per-class precision/recall/F1 and macro F1 over the truth
    classes, from (truth, predicted) pairs."""
    confusion: dict = {}
    for truth, predicted in pairs:
        confusion.setdefault(truth, Counter())[predicted] += 1
    classes = sorted({t for t, _ in pairs} | {p for _, p in pairs})
    per_class = {}
    for cls in classes:
        tp = sum(1 for t, p in pairs if t == cls and p == cls)
        predicted = sum(1 for _, p in pairs if p == cls)
        support = sum(1 for t, _ in pairs if t == cls)
        precision = tp / predicted if predicted else 0.0
        recall = tp / support if support else 0.0
        f1 = 2 * precision * recall / (precision + recall) if precision + recall else 0.0
        per_class[cls] = {"precision": precision, "recall": recall, "f1": f1, "support": support}
    truth_classes = sorted({t for t, _ in pairs})
    return {
        "n": len(pairs),
        "accuracy": sum(1 for t, p in pairs if t == p) / len(pairs) if pairs else 0.0,
        "macro_f1": (
            sum(per_class[c]["f1"] for c in truth_classes) / len(truth_classes)
            if truth_classes else 0.0
        ),
        "per_class": per_class,
        "confusion": {t: dict(c) for t, c in confusion.items()},
    }


def evaluate_challenge(provider, rows: list, limit: int | None = None) -> tuple[dict, list]:
    if limit:
        rows = rows[:limit]
    print(f"Classifying {len(rows)} pages with {provider.name}...")
    records = []
    for row in rows:
        request = ProposeRequest(
            purpose=CHALLENGE_PURPOSE,
            intent_kind="detectChallenge",
            stuck="challengePresent",
            screenshot_b64=row["imageB64"],
        )
        response, error, elapsed = timed(lambda: provider.propose(request).to_dict())
        records.append({
            "page": row["page"],
            "truth": row["challenge"]["kind"],
            "predicted": challenge_answer(response),
            "response": response,
            "error": error,
            "elapsed": elapsed,
        })
    challenge = classification_metrics([(r["truth"], r["predicted"]) for r in records])
    challenge["errors"] = sum(1 for r in records if r["error"] is not None)
    results = {
        "provider": provider.name,
        "model": model_of(provider),
        "challenge": challenge,
        "latency": latency([r["elapsed"] for r in records]),
        "memory": peak_memory_mb(),
    }
    return results, records


def normalize_text(text: str) -> str:
    return " ".join(text.split())


def edit_distance(a: str, b: str) -> int:
    previous = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        current = [i]
        for j, cb in enumerate(b, 1):
            current.append(min(previous[j] + 1, current[j - 1] + 1, previous[j - 1] + (ca != cb)))
        previous = current
    return previous[-1]


def crop_box(bbox: dict, viewport: dict, pad: float = 2.0) -> tuple:
    """Screenshot-pixel crop for a CSS box, padded and clamped to the frame."""
    dpr = viewport["dpr"]
    return (
        max(0, math.floor((bbox["x"] - pad) * dpr)),
        max(0, math.floor((bbox["y"] - pad) * dpr)),
        min(round(viewport["w"] * dpr), math.ceil((bbox["x"] + bbox["w"] + pad) * dpr)),
        min(round(viewport["h"] * dpr), math.ceil((bbox["y"] + bbox["h"] + pad) * dpr)),
    )


def evaluate_ocr(reader, rows: list, limit: int | None = None) -> tuple[dict, list]:
    runs = [(row, run) for row in rows for run in row["textRuns"]]
    if limit:
        runs = runs[:limit]
    print(f"Reading {len(runs)} text boxes with {reader.name}...")
    images = {}
    records = []
    for row, run in runs:
        png = images.setdefault(row["page"], base64.b64decode(row["imageB64"]))
        box = crop_box(run["bbox"], row["viewport"])
        text, error, elapsed = timed(lambda: reader.read(png, box))
        truth = normalize_text(run["text"])
        read = normalize_text(text or "")
        records.append({
            "page": row["page"],
            "truth": truth,
            "read": read,
            "font_size_px": run.get("fontSizePx"),
            "exact": read == truth,
            "casefold": read.casefold() == truth.casefold(),
            "cer": edit_distance(read, truth) / max(1, len(truth)),
            "error": error,
            "elapsed": elapsed,
        })
    n = len(records)
    ocr = {
        "runs": n,
        "exact": sum(r["exact"] for r in records),
        "exact_rate": sum(r["exact"] for r in records) / n if n else 0.0,
        "casefold_rate": sum(r["casefold"] for r in records) / n if n else 0.0,
        "mean_cer": sum(r["cer"] for r in records) / n if n else 0.0,
        "errors": sum(1 for r in records if r["error"] is not None),
    }
    results = {
        "reader": reader.name,
        "model": model_of(reader),
        "ocr": ocr,
        "latency": latency([r["elapsed"] for r in records]),
        "memory": peak_memory_mb(),
    }
    return results, records


# -- text readers: read(png_bytes, crop_box) -> str -------------------------

def crop_png(png: bytes, box: tuple):
    from PIL import Image

    return Image.open(io.BytesIO(png)).convert("RGB").crop(box)


def png_bytes(image) -> bytes:
    buffer = io.BytesIO()
    image.save(buffer, format="PNG")
    return buffer.getvalue()


class TesseractReader:
    name = "tesseract"

    def read(self, png: bytes, box: tuple) -> str:
        image = crop_png(png, box)
        # Tesseract is tuned for glyphs around 30 px tall; screen text is smaller.
        image = image.resize((image.width * 3, image.height * 3))
        result = subprocess.run(
            ["tesseract", "stdin", "stdout", "--psm", "7"],
            input=png_bytes(image),
            capture_output=True,
            check=True,
        )
        return result.stdout.decode().strip()


class AppleVisionReader:
    name = "apple-vision"

    def read(self, png: bytes, box: tuple) -> str:
        import Vision
        from Foundation import NSData

        data = png_bytes(crop_png(png, box))
        handler = Vision.VNImageRequestHandler.alloc().initWithData_options_(
            NSData.dataWithBytes_length_(data, len(data)), None
        )
        request = Vision.VNRecognizeTextRequest.alloc().init()
        request.setRecognitionLevel_(Vision.VNRequestTextRecognitionLevelAccurate)
        request.setUsesLanguageCorrection_(False)
        ok, error = handler.performRequests_error_([request], None)
        if not ok:
            raise RuntimeError(str(error))
        observations = sorted(request.results() or [], key=lambda o: o.boundingBox().origin.x)
        return " ".join(o.topCandidates_(1)[0].string() for o in observations)


class MlxVlmReader:
    name = "mlx-vlm"
    PROMPT = "Transcribe the text in this image exactly as written. Reply with the text only."

    def __init__(self, model_name: str):
        self.model_name = model_name
        self._model = None
        self._processor = None

    def read(self, png: bytes, box: tuple) -> str:
        import re
        from mlx_vlm import generate, load

        if self._model is None:
            self._model, self._processor = load(self.model_name)
        with tempfile.NamedTemporaryFile(suffix=".png") as tmp:
            crop_png(png, box).save(tmp.name)
            messages = [{
                "role": "user",
                "content": [{"type": "image", "image": tmp.name}, {"type": "text", "text": self.PROMPT}],
            }]
            prompt = self._processor.apply_chat_template(
                messages, tokenize=False, add_generation_prompt=True, enable_thinking=False
            )
            result = generate(
                self._model, self._processor, prompt, image=[tmp.name],
                max_tokens=64, temp=0.0, verbose=False,
            )
        text = result.text if hasattr(result, "text") else str(result)
        return re.sub(r"<think>.*?</think>", "", text, flags=re.DOTALL).strip()


def create_reader(kind: str, model: str | None):
    if kind == "tesseract":
        return TesseractReader()
    if kind == "apple-vision":
        return AppleVisionReader()
    if kind == "mlx-vlm":
        if not model:
            raise ValueError("--model is required for the mlx-vlm reader")
        return MlxVlmReader(model)
    raise ValueError(f"unknown reader {kind!r}; expected tesseract, apple-vision or mlx-vlm")


def main():
    parser = argparse.ArgumentParser(description="Vision suites for serving providers and text readers")
    parser.add_argument("--suite", choices=SUITES, default="window")
    parser.add_argument("--provider", default=None, help="mlx-vlm | v1 | ollama | lmstudio")
    parser.add_argument("--reader", default="tesseract", help="ocr suite: tesseract | apple-vision | mlx-vlm")
    parser.add_argument("--model", default=None, help="mlx-vlm model")
    parser.add_argument("--input", required=True, help="corpus JSONL (window) or perception JSONL")
    parser.add_argument("--output", default="models/provider_eval.json", help="results JSON; per-row records go beside it")
    parser.add_argument("--limit", type=int, default=None, help="limit requests")
    args = parser.parse_args()

    with open(args.input) as f:
        rows = [json.loads(line) for line in f if line.strip()]
    print(f"Loaded {len(rows)} rows from {args.input}")

    if args.suite == "ocr":
        results, records = evaluate_ocr(create_reader(args.reader, args.model), rows, args.limit)
    else:
        provider = create_provider(args.provider, args.model)
        run = {"window": evaluate, "grounding": evaluate_grounding, "challenge": evaluate_challenge}
        results, records = run[args.suite](provider, rows, args.limit)
    results["suite"] = args.suite
    results["input"] = args.input

    print(json.dumps({k: v for k, v in results.items() if k not in ("calibration",)}, indent=2))
    out = Path(args.output)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(results, indent=2))
    records_path = out.with_suffix(".rows.jsonl")
    with open(records_path, "w") as f:
        for record in records:
            f.write(json.dumps(record) + "\n")
    print(f"\nResults: {out}\nPer-row records: {records_path}")


if __name__ == "__main__":
    main()
