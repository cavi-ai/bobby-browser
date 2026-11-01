"""Perception rows as detector training examples: one class, `interactable`, from the DOM boxes the
collector records. Pure Python so it is testable without the training stack."""

import base64
import json

# Boxes smaller than this on either side, in screenshot pixels, are not drawn controls.
MIN_SIDE = 4.0


def rows(path):
    with open(path) as handle:
        for line in handle:
            if line.strip():
                yield json.loads(line)


def boxes(row):
    """Interactable boxes as (x, y, width, height) in screenshot pixels."""
    scale = float((row.get("viewport") or {}).get("dpr") or 1.0)
    out = []
    for element in row.get("elements", []):
        box = element["bbox"]
        x, y, w, h = (float(box[key]) * scale for key in ("x", "y", "w", "h"))
        if w >= MIN_SIDE and h >= MIN_SIDE:
            out.append((x, y, w, h))
    return out


def png(row):
    return base64.b64decode(row["imageB64"])


def iou(a, b):
    ax, ay, aw, ah = a
    bx, by, bw, bh = b
    left, top = max(ax, bx), max(ay, by)
    right, bottom = min(ax + aw, bx + bw), min(ay + ah, by + bh)
    inter = max(0.0, right - left) * max(0.0, bottom - top)
    union = aw * ah + bw * bh - inter
    return inter / union if union > 0 else 0.0


def match(predicted, truth, threshold=0.5):
    """Greedy one-to-one matching by score order: (true positives, predictions, truths)."""
    used = set()
    hits = 0
    for box in predicted:
        best, best_iou = None, threshold
        for index, target in enumerate(truth):
            if index in used:
                continue
            overlap = iou(box, target)
            if overlap >= best_iou:
                best, best_iou = index, overlap
        if best is not None:
            used.add(best)
            hits += 1
    return hits, len(predicted), len(truth)
