"""Fine-tunes RT-DETRv2-R18 (Apache-2.0) to find interactable UI elements in screenshots.

    uv run --no-project --python 3.12 --with torch --with transformers --with timm --with scipy \
        --with pillow python train_detector.py --train rows.jsonl --val fixtures.jsonl --out model/

Rows come from the ignored perception collectors. Writes the best checkpoint by validation F1 to
--out, with `metrics.json` naming the score threshold it was measured at.
"""

import argparse
import io
import json
import math
import pathlib
import random
import time

import torch
from PIL import Image
from transformers import RTDetrV2ForObjectDetection

from detector_data import boxes, match, png, rows

BASE = "PekingU/rtdetr_v2_r18vd"
HEIGHT, WIDTH = 544, 1024


def tensor(image_bytes):
    image = Image.open(io.BytesIO(image_bytes)).convert("RGB")
    size = image.size
    image = image.resize((WIDTH, HEIGHT), Image.BILINEAR)
    pixels = torch.frombuffer(bytearray(image.tobytes()), dtype=torch.uint8)
    # Bytes, not floats: a float copy of every training page would not fit in memory.
    return pixels.view(HEIGHT, WIDTH, 3).permute(2, 0, 1).contiguous(), size


def target(row, size):
    width, height = size
    cxcywh = [[(x + w / 2) / width, (y + h / 2) / height, w / width, h / height] for x, y, w, h in boxes(row)]
    return {
        "class_labels": torch.zeros(len(cxcywh), dtype=torch.long),
        "boxes": torch.tensor(cxcywh, dtype=torch.float32).reshape(-1, 4),
    }


def load(path):
    examples = []
    for row in rows(path):
        pixels, size = tensor(png(row))
        examples.append((pixels, target(row, size), size, boxes(row)))
    return examples


@torch.no_grad()
def predict(model, pixels, size, device):
    """Boxes (x, y, w, h) in screenshot pixels with scores, best first."""
    out = model(pixel_values=pixels.unsqueeze(0).to(device).float() / 255.0)
    scores = out.logits[0].sigmoid()[:, 0].cpu()
    width, height = size
    found = []
    for (cx, cy, w, h), score in zip(out.pred_boxes[0].cpu().tolist(), scores.tolist()):
        found.append((score, ((cx - w / 2) * width, (cy - h / 2) * height, w * width, h * height)))
    found.sort(key=lambda item: -item[0])
    return found


def evaluate(model, examples, device, thresholds=(0.3, 0.4, 0.5, 0.6, 0.7)):
    model.eval()
    predictions = [predict(model, pixels, size, device) for pixels, _, size, _ in examples]
    model.train()
    results = {}
    for threshold in thresholds:
        hits = predicted = truth = 0
        for found, (_, _, _, true_boxes) in zip(predictions, examples):
            kept = [box for score, box in found if score >= threshold]
            h, p, t = match(kept, true_boxes)
            hits, predicted, truth = hits + h, predicted + p, truth + t
        precision = hits / predicted if predicted else 0.0
        recall = hits / truth if truth else 0.0
        f1 = 2 * precision * recall / (precision + recall) if precision + recall else 0.0
        results[threshold] = {"precision": precision, "recall": recall, "f1": f1}
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--train", required=True)
    parser.add_argument("--val", required=True)
    parser.add_argument("--out", required=True, type=pathlib.Path)
    parser.add_argument("--epochs", type=int, default=20)
    parser.add_argument("--batch", type=int, default=4)
    parser.add_argument("--lr", type=float, default=1e-4)
    parser.add_argument("--minutes", type=float, default=90.0, help="stop after this much training time")
    parser.add_argument("--seed", type=int, default=7)
    parser.add_argument("--init", default=BASE, help="checkpoint to start from")
    args = parser.parse_args()

    random.seed(args.seed)
    torch.manual_seed(args.seed)
    device = "mps" if torch.backends.mps.is_available() else "cpu"
    train, val = load(args.train), load(args.val)
    print(f"train {len(train)} pages, val {len(val)} pages, device {device}", flush=True)

    model = RTDetrV2ForObjectDetection.from_pretrained(
        args.init, num_labels=1, id2label={0: "interactable"}, label2id={"interactable": 0}, ignore_mismatched_sizes=True
    ).to(device)
    backbone = [p for n, p in model.named_parameters() if "backbone" in n]
    rest = [p for n, p in model.named_parameters() if "backbone" not in n]
    optimizer = torch.optim.AdamW(
        [{"params": backbone, "lr": args.lr / 10}, {"params": rest, "lr": args.lr}], weight_decay=1e-4
    )
    steps = args.epochs * math.ceil(len(train) / args.batch)
    schedule = torch.optim.lr_scheduler.OneCycleLR(
        optimizer, max_lr=[args.lr / 10, args.lr], total_steps=steps, pct_start=0.05
    )

    args.out.mkdir(parents=True, exist_ok=True)
    best = {"f1": -1.0}
    started = time.time()
    model.train()
    for epoch in range(args.epochs):
        random.shuffle(train)
        losses = []
        for start in range(0, len(train), args.batch):
            batch = train[start:start + args.batch]
            pixels = torch.stack([pixels for pixels, _, _, _ in batch]).to(device).float() / 255.0
            labels = [{key: value.to(device) for key, value in target.items()} for _, target, _, _ in batch]
            loss = model(pixel_values=pixels, labels=labels).loss
            optimizer.zero_grad()
            loss.backward()
            torch.nn.utils.clip_grad_norm_(model.parameters(), 0.1)
            optimizer.step()
            schedule.step()
            losses.append(loss.item())
        scores = evaluate(model, val, device)
        threshold, score = max(scores.items(), key=lambda item: item[1]["f1"])
        elapsed = (time.time() - started) / 60
        print(
            f"epoch {epoch + 1}: loss {sum(losses) / len(losses):.3f} val f1 {score['f1']:.3f} "
            f"(p {score['precision']:.3f} r {score['recall']:.3f} at {threshold}) {elapsed:.1f} min",
            flush=True,
        )
        if score["f1"] > best["f1"]:
            best = {**score, "threshold": threshold, "epoch": epoch + 1}
            model.save_pretrained(args.out)
            (args.out / "metrics.json").write_text(
                json.dumps({"base": BASE, "init": args.init, "input": [HEIGHT, WIDTH], **best}, indent=2)
            )
        if elapsed > args.minutes:
            print(f"time budget reached after epoch {epoch + 1}", flush=True)
            break
    print(f"best {json.dumps(best)}", flush=True)


if __name__ == "__main__":
    main()
