#!/usr/bin/env python3
"""
Deterministic synthetic data generator for the candidate-index experiment.

Each example is a page with 3-8 non-overlapping candidate elements (button,
link, textbox) placed on a 1280x800 canvas. Every candidate carries a bbox.
The ground-truth click point is sampled inside the target candidate's bbox,
so both output schemas can be derived from one record:

  coords:    {"kind": "click", "x": ..., "y": ...}
  candidate: {"kind": "clickCandidate", "index": ...}

Usage:
    python gen_synthetic.py --n 200 --seed 42 --output data/synth.jsonl
"""

import argparse
import json
import random
from pathlib import Path

CANVAS_W, CANVAS_H = 1280, 800
ROLES = ["button", "link", "textbox", "checkbox", "tab"]
VERBS = ["Submit", "Login", "Continue", "Save", "Cancel", "Next", "Back", "Search",
         "Delete", "Confirm", "Upload", "Download", "Edit", "Close", "Apply"]
NOUNS = ["Account", "Order", "Report", "Settings", "Profile", "Invoice", "Ticket",
         "Document", "Payment", "Subscription", "Filter", "Export", "Draft", "User"]
PURPOSES = ["Fill form", "Submit request", "Open settings", "Confirm dialog",
            "Navigate to next step", "Save changes", "Search records", "Upload file"]
TEXT_VALUES = ["alice@example.com", "hunter2", "Acme Corp", "2026-08-08",
               "+1-555-0100", "42", "john.doe", "CA-94110"]
JOURNEYS = ["onboarding", "customer-update", "documents", "authorization", "report-recovery"]


def non_overlapping_boxes(rng: random.Random, count: int) -> list:
    boxes = []
    attempts = 0
    while len(boxes) < count and attempts < 500:
        attempts += 1
        w = rng.randint(90, 260)
        h = rng.randint(28, 56)
        x = rng.randint(20, CANVAS_W - w - 20)
        y = rng.randint(20, CANVAS_H - h - 20)
        if all(abs(x - bx) > (w + bw) / 2 + 12 or abs(y - by) > (h + bh) / 2 + 12
               for bx, by, bw, bh in boxes):
            boxes.append((x, y, w, h))
    return boxes


def make_example(rng: random.Random, i: int, negative: bool = False) -> dict:
    n = rng.randint(3, 8)
    boxes = non_overlapping_boxes(rng, n)

    # Learnable rules, one per action kind:
    #   click:        purpose "Verb the noun"        -> candidate "Verb Noun"
    #   typeText:     purpose "Enter '<text>' ..."   -> the only textbox
    #   extractValue: purpose "Read the <noun>"      -> candidate whose name's
    #                 noun matches; value is that candidate's name
    # Positions stay random so x,y remain unpredictable from the prompt.
    # `negative` removes the target's verb from the pool so the purpose has
    # no valid match: ground truth for abstention (target_index omitted).
    kind = rng.choices(["click", "typeText", "extractValue"], weights=[0.5, 0.25, 0.25])[0]
    verbs = rng.sample(VERBS, n)
    nouns = rng.sample(NOUNS, n)  # unique nouns: extractValue matches on noun
    target_index = rng.randrange(n)

    if negative:
        absent_verb = rng.choice([v for v in VERBS if v not in verbs])
    else:
        absent_verb = None

    candidates = []
    for j, (x, y, w, h) in enumerate(boxes):
        candidates.append({
            "role": rng.choice(ROLES),
            "name": f"{verbs[j]} {nouns[j]}",
            "bbox": {"x": x, "y": y, "w": w, "h": h},
        })

    target = candidates[target_index]
    bbox = target["bbox"]
    margin_x = max(4, int(bbox["w"] * 0.15))
    margin_y = max(4, int(bbox["h"] * 0.15))
    cx = rng.randint(bbox["x"] + margin_x, bbox["x"] + bbox["w"] - margin_x)
    cy = rng.randint(bbox["y"] + margin_y, bbox["y"] + bbox["h"] - margin_y)
    confidence = round(rng.uniform(0.7, 0.95), 2)

    if negative:
        # The purpose references an element the page does not have; there is
        # no ground-truth target, and the correct response is abstention.
        noun = rng.choice(nouns).lower()
        purpose = f"{absent_verb} the {noun}"
        action = {"kind": "click", "x": 0.0, "y": 0.0}
        record = {
            "image_b64": "",
            "purpose": purpose,
            "intent_kind": "locate",
            "stuck": "targetMissing",
            "context_url": f"https://example.com/{rng.choice(JOURNEYS)}/page{i}",
            "context_candidates": candidates,
            "model_response": {
                "confidence": confidence,
                "action": action,
            },
            "success": True,
            "journey": rng.choice(JOURNEYS),
            "step": f"step_{i}",
            "negative": True,
        }
        return record

    if kind == "click":
        purpose = f"{verbs[target_index]} the {target['name'].split(' ', 1)[1].lower()}"
        action = {"kind": "click", "x": float(cx), "y": float(cy)}
    elif kind == "typeText":
        # Exactly one textbox; the purpose quotes the text to enter.
        target["role"] = "textbox"
        for j, c in enumerate(candidates):
            if j != target_index and c["role"] == "textbox":
                c["role"] = "button"
        text = rng.choice(TEXT_VALUES)
        noun = target["name"].split(" ", 1)[1].lower()
        purpose = f"Enter '{text}' into the {noun} field"
        action = {"kind": "typeText", "text": text}
    else:
        noun = target["name"].split(" ", 1)[1].lower()
        purpose = f"Read the {noun}"
        action = {"kind": "extractValue", "value": target["name"]}

    return {
        "image_b64": "",
        "purpose": purpose,
        "intent_kind": "locate",
        "stuck": "targetMissing",
        "context_url": f"https://example.com/{rng.choice(JOURNEYS)}/page{i}",
        "context_candidates": candidates,
        "target_index": target_index,
        "hint_role": target["role"],
        "model_response": {
            "confidence": confidence,
            "action": action,
        },
        "success": True,
        "journey": rng.choice(JOURNEYS),
        "step": f"step_{i}",
    }


def main():
    parser = argparse.ArgumentParser(description="Synthetic data generator (bbox candidates)")
    parser.add_argument("--n", type=int, default=200, help="Number of examples")
    parser.add_argument("--seed", type=int, default=42, help="RNG seed")
    parser.add_argument("--neg-frac", type=float, default=0.0,
                        help="Fraction of examples with no valid target (abstention ground truth)")
    parser.add_argument("--output", required=True, help="Output JSONL path")
    args = parser.parse_args()

    rng = random.Random(args.seed)
    out = Path(args.output)
    out.parent.mkdir(parents=True, exist_ok=True)
    negatives = 0
    with open(out, "w") as f:
        for i in range(args.n):
            negative = rng.random() < args.neg_frac
            negatives += int(negative)
            f.write(json.dumps(make_example(rng, i, negative)) + "\n")
    print(f"wrote {args.n} examples to {out} (seed {args.seed}, {negatives} negatives)")


if __name__ == "__main__":
    main()
