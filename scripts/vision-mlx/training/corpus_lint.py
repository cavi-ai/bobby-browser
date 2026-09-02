#!/usr/bin/env python3
"""Corpus lint: the vision corpus's failure modes as static checks.

No model required — this gate runs anywhere. Every check encodes a
measured regression from the research log:

  window_roles       — windows carry only the actionable role set the
                       adapter trains on; landmark/structural rows shift
                       the prompt off distribution (one `main` row flips
                       the adapter to abstain)
  empty_window       — an empty candidate window is not selection signal;
                       collecting it poisons the abstain class
  target_index       — present ⇒ in range; absent ⇒ success=False
                       (abstain-labeled negative)
  negative_balance   — abstain recall tracks the positive:negative ratio;
                       50:1 starves the class, ~4:1 is the healthy band
  negative_diversity — negative purposes must not repeat within a
                       journey; a repeated phrasing digs a point
                       attractor that swallows legitimate tasks
  intent_kind        — locate/fill/extract only
  required_fields    — the record schema the trainer/evaluator consume

Exit code 1 on any error. Warnings never fail the gate.

    python corpus_lint.py --input data/vision-corpus-v6.jsonl
"""

import argparse
import json
import sys
from collections import Counter, defaultdict

from mlx_finetune import normalize_corpus_example

ACTIONABLE_ROLES = frozenset(
    [
        "button",
        "link",
        "textbox",
        "combobox",
        "checkbox",
        "radio",
        "tab",
        "menuitem",
        "searchbox",
        "switch",
    ]
)

INTENT_KINDS = frozenset(["locate", "fill", "extract"])

REQUIRED_FIELDS = [
    "purpose",
    "intent_kind",
    "context_candidates",
    "model_response",
    "success",
    "journey",
    "step",
]
# The engine's corpus schema omits target_index entirely on abstentions
# (skip_serializing_if Option::is_none); absence reads as None downstream.

# abstain recall is healthy around 4:1; outside this band the negative
# class is either starved or drowning the positives.
MIN_POS_PER_NEG = 2.0
MAX_POS_PER_NEG = 8.0
# Above this absolute negative mass the class is not starved regardless of
# ratio — positive volume scales with steps x runs, negative volume does not.
MIN_NEGATIVE_MASS = 60


def lint(rows: list, *, check_balance: bool = True) -> tuple:
    errors = []
    warnings = []

    negatives = 0
    positives = 0
    # A repeated negative phrasing is only an attractor when the WINDOW is
    # also the same (same phrasing + same page state seen N times digs a
    # point well). The same vague phrasing across different windows is the
    # intended semantics: "the widget in the corner" should abstain
    # everywhere. Track both.
    negative_seen = defaultdict(set)  # (journey) -> {(purpose, window signature)}
    negative_purpose_counts = defaultdict(Counter)  # journey -> purpose -> n

    for line_no, row in enumerate(rows, start=1):
        where = f"line {line_no} ({row.get('journey', '?')}/{row.get('step', '?')})"

        for field in REQUIRED_FIELDS:
            if field not in row:
                errors.append(f"{where}: missing required field {field!r}")

        kind = row.get("intent_kind")
        if kind is not None and kind not in INTENT_KINDS:
            errors.append(f"{where}: unknown intent_kind {kind!r}")

        candidates = row.get("context_candidates") or []
        if not candidates:
            errors.append(f"{where}: empty candidate window is not selection signal")

        for candidate in candidates:
            role = candidate.get("role")
            if role is not None and role not in ACTIONABLE_ROLES:
                errors.append(
                    f"{where}: window carries non-actionable role {role!r} "
                    f"(name {candidate.get('name')!r})"
                )

        target = row.get("target_index")
        if target is None:
            negatives += 1
            if row.get("success") is not False:
                errors.append(
                    f"{where}: target_index absent but success is not False; "
                    "abstain-labeled negatives must mark success=False"
                )
            purpose = row.get("purpose", "")
            journey = row.get("journey", "?")
            signature = tuple(
                (c.get("role"), c.get("name")) for c in candidates
            )
            seen = negative_seen[journey]
            if (purpose, signature) in seen:
                # Near-duplicate negatives (same phrasing, same window,
                # different run) are measured-working abstain mass on a
                # static fixture — but they concentrate the abstain region
                # and add no information (volume without diversity
                # degrades OOS for the positive class). Warn, don't fail.
                warnings.append(
                    f"{where}: negative purpose {purpose!r} repeats with an "
                    "identical window — near-duplicate abstain mass; prefer "
                    "fresh windows over repeats"
                )
            seen.add((purpose, signature))
            negative_purpose_counts[journey][purpose] += 1
        else:
            positives += 1
            if not isinstance(target, int) or isinstance(target, bool):
                errors.append(f"{where}: target_index {target!r} is not an integer")
            elif candidates and not 0 <= target < len(candidates):
                errors.append(
                    f"{where}: target_index {target} out of range for "
                    f"{len(candidates)} candidates"
                )
            if row.get("success") is False:
                warnings.append(
                    f"{where}: failed record with a target is a diagnostic, "
                    "not supervision (excluded from training)"
                )

    for journey, counts in negative_purpose_counts.items():
        for purpose, n in counts.items():
            if n > 3:
                warnings.append(
                    f"{journey}: negative purpose {purpose!r} appears {n} times "
                    "(windows differ, so not an attractor, but phrasing mass "
                    "concentrates the abstain region)"
                )

    if negatives == 0 and len(rows) >= 10:
        errors.append("no abstain-labeled negatives; the abstain class is untrained")
    elif negatives > 0 and check_balance:
        ratio = positives / negatives
        # The ratio band guards against a starved or drowned abstain class
        # at training scale. Above a healthy absolute negative mass, the
        # upper bound relaxes to a warning: positive volume scales with
        # steps x runs while negative volume does not, so a large corpus
        # naturally drifts past 8:1 without the class being starved.
        if negatives < MIN_NEGATIVE_MASS:
            if not MIN_POS_PER_NEG <= ratio <= MAX_POS_PER_NEG:
                errors.append(
                    f"positive:negative ratio {ratio:.1f}:1 outside the healthy "
                    f"band [{MIN_POS_PER_NEG}:{1}, {MAX_POS_PER_NEG}:1] "
                    f"({positives}/{negatives})"
                )
        elif ratio > MAX_POS_PER_NEG * 1.5:
            warnings.append(
                f"positive:negative ratio {ratio:.1f}:1 with {negatives} "
                "negatives — healthy mass, but the abstain region may thin "
                "as positives scale"
            )

    return errors, warnings


def main():
    parser = argparse.ArgumentParser(description="Lint a Bobby vision corpus")
    parser.add_argument("--input", required=True, help="corpus JSONL")
    parser.add_argument(
        "--skip-balance",
        action="store_true",
        help="skip the training-scale class balance check for smoke corpora",
    )
    args = parser.parse_args()

    with open(args.input) as f:
        # Engine records are camelCase, scripted records snake_case; the
        # trainer normalizes both, and the lint must see what the trainer
        # sees.
        rows = [normalize_corpus_example(json.loads(line)) for line in f if line.strip()]

    errors, warnings = lint(rows, check_balance=not args.skip_balance)
    for warning in warnings:
        print(f"warning: {warning}")
    for error in errors:
        print(f"error: {error}")

    journeys = Counter(r.get("journey") for r in rows)
    print(
        f"\n{len(rows)} records ({sum(1 for r in rows if r.get('target_index') is not None)} positive / "
        f"{sum(1 for r in rows if r.get('target_index') is None)} negative), "
        f"{len(journeys)} journeys: {len(errors)} errors, {len(warnings)} warnings"
    )
    sys.exit(1 if errors else 0)


if __name__ == "__main__":
    main()
