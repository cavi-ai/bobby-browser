"""Scores a trained interactable detector on perception rows and optionally exports it to ONNX.

    uv run --no-project --python 3.12 --with torch --with transformers --with timm --with scipy \
        --with pillow --with onnx --with onnxruntime python eval_detector.py --model model/ \
        --rows fixtures.jsonl [--onnx model.onnx]

Prints precision and recall at IoU 0.5 for the threshold in the model's metrics.json, per page and
in total, and the mean time per screenshot.
"""

import argparse
import json
import pathlib
import time

import torch
from transformers import RTDetrV2ForObjectDetection

from detector_data import boxes, match, png, rows
from train_detector import HEIGHT, WIDTH, predict, tensor


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--model", required=True, type=pathlib.Path)
    parser.add_argument("--rows", required=True)
    parser.add_argument("--onnx", type=pathlib.Path)
    args = parser.parse_args()

    threshold = json.loads((args.model / "metrics.json").read_text())["threshold"]
    device = "mps" if torch.backends.mps.is_available() else "cpu"
    model = RTDetrV2ForObjectDetection.from_pretrained(args.model).to(device).eval()
    hits = predicted = truth = 0
    seconds = []
    for row in rows(args.rows):
        pixels, size = tensor(png(row))
        started = time.time()
        found = predict(model, pixels, size, device)
        seconds.append(time.time() - started)
        kept = [box for score, box in found if score >= threshold]
        h, p, t = match(kept, boxes(row))
        hits, predicted, truth = hits + h, predicted + p, truth + t
        print(f"{row['page']}: {h}/{t} found, {p - h} extra", flush=True)
    precision = hits / predicted if predicted else 0.0
    recall = hits / truth if truth else 0.0
    print(
        f"threshold {threshold}: precision {precision:.3f} recall {recall:.3f} "
        f"({hits}/{truth} found, {predicted} predicted), {1000 * sum(seconds[1:]) / max(1, len(seconds) - 1):.0f} ms per screenshot on {device}"
    )

    if args.onnx:
        import numpy
        import onnxruntime

        cpu = model.to("cpu")
        sample = torch.rand(1, 3, HEIGHT, WIDTH)
        torch.onnx.export(
            cpu, (sample,), str(args.onnx), input_names=["pixel_values"], output_names=["logits", "pred_boxes"],
            opset_version=17, dynamo=False,
        )
        session = onnxruntime.InferenceSession(str(args.onnx), providers=["CPUExecutionProvider"])
        logits, pred_boxes = session.run(["logits", "pred_boxes"], {"pixel_values": sample.numpy()})
        with torch.no_grad():
            expected = cpu(pixel_values=sample)
        drift = max(
            float(numpy.abs(logits - expected.logits.numpy()).max()),
            float(numpy.abs(pred_boxes - expected.pred_boxes.numpy()).max()),
        )
        started = time.time()
        for _ in range(5):
            session.run(None, {"pixel_values": sample.numpy()})
        print(
            f"onnx {args.onnx} {args.onnx.stat().st_size / 1e6:.1f} MB, max drift {drift:.2e}, "
            f"{1000 * (time.time() - started) / 5:.0f} ms per screenshot on CPU"
        )


if __name__ == "__main__":
    main()
