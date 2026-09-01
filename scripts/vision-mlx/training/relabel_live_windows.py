#!/usr/bin/env python3
"""Relabel live-harvest abstain records as scripted ground truth.

The corpus unification bootstrap (§4v): the harvest harness captures
production-shaped escalation windows, but the adapter abstains on
answerable steps because it never saw production windows in training.
The scripted fallback that runs next knows the true target, so those
records become ground truth with production windows: same window the
production path builds, label from the script.

Purpose -> target mapping lives here (it must match the harvest
harness's phrasing lists in intent_vision_gauntlet.rs). Relabeled
records carry journey="gauntlet-live" and
outcome_stage="liveWindowScriptedLabel" for provenance.

    python relabel_live_windows.py \
        --input /tmp/vision-harvest/vision-corpus.jsonl \
        --output data/gauntlet-live-relabeled.jsonl
"""

import argparse
import json

# purpose -> (target candidate name, action kind, step)
STEP_BY_PURPOSE = {}
for purpose in [
    "Put 'Atlas' in the lookup box",
    "Type 'Atlas' into the finder field",
    "Enter 'Atlas' in the client search input",
    "Key 'Atlas' into the customer finder",
]:
    STEP_BY_PURPOSE[purpose] = ("Search customers", "typeText", "type_search")
for purpose in [
    "Push the button to search",
    "Fire off the customer search",
    "Execute the lookup with the button",
    "Trigger the search now",
]:
    STEP_BY_PURPOSE[purpose] = ("Search", "click", "run_search")
for purpose in [
    "Bring up the Atlas Labs record",
    "Open the Atlas Labs page",
    "Go to the Atlas Labs customer",
    "Pull up the Atlas Labs account",
]:
    STEP_BY_PURPOSE[purpose] = ("Atlas Labs", "click", "open_customer")
for purpose in [
    "Pick the high priority for this customer",
    "Set the priority dropdown to high",
    "Choose high in the priority selector",
    "Mark the customer priority as high",
]:
    STEP_BY_PURPOSE[purpose] = ("Customer priority", "click", "select_priority")
for purpose in [
    "Store the priority change with the save button",
    "Commit the new priority setting",
    "Save the updated customer priority",
    "Apply the priority change with the save control",
]:
    STEP_BY_PURPOSE[purpose] = ("Save priority", "click", "save_priority")
for purpose in [
    "Upload the staged document to the server",
    "Send the staged document off",
    "Push the staged document upload through",
    "Submit the staged customer document",
]:
    STEP_BY_PURPOSE[purpose] = ("Upload document", "click", "click_upload")
for purpose in [
    "Put 'Maya Chen' in the name field",
    "Type 'Maya Chen' where the name goes",
    "Enter 'Maya Chen' for the contact name",
    "Fill in 'Maya Chen' as the name",
]:
    STEP_BY_PURPOSE[purpose] = ("Full name", "typeText", "type_full_name")
for purpose in [
    "Type 'maya@atlas.example' into the email field",
    "Put 'maya@atlas.example' in the email box",
    "Enter the email 'maya@atlas.example'",
    "Fill in 'maya@atlas.example' for email",
]:
    STEP_BY_PURPOSE[purpose] = ("Work email", "typeText", "type_work_email")
for purpose in [
    "Enter 'Atlas Labs' as the company",
    "Type 'Atlas Labs' in the company field",
    "Put 'Atlas Labs' where the company goes",
    "Fill in 'Atlas Labs' as the organization",
]:
    STEP_BY_PURPOSE[purpose] = ("Company name", "typeText", "type_company")
for purpose in [
    "Put '02110' in the postal box",
    "Type '02110' into the postal field",
    "Enter '02110' for the postal code area",
    "Fill in '02110' in the postal slot",
    "Enter '10001' in the postal code box",
    "Put '10001' in the postal field",
    "Type '10001' into the postal box",
    "Correct the postal code to '10001'",
]:
    STEP_BY_PURPOSE[purpose] = ("Postal code", "typeText", "type_postal_code")
for purpose in [
    "Pick the growth plan",
    "Choose growth for the plan",
    "Select the growth tier",
    "Go with the growth plan",
]:
    STEP_BY_PURPOSE[purpose] = ("Plan", "click", "select_plan")
for purpose in [
    "Choose annual billing",
    "Pick the annual billing cycle",
    "Set billing to annual",
    "Select annual for the billing cycle",
]:
    STEP_BY_PURPOSE[purpose] = ("Billing cycle", "click", "select_billing")
for purpose in [
    "Create the customer account",
    "Create the new customer",
    "Register the customer now",
    "Create this customer record",
]:
    STEP_BY_PURPOSE[purpose] = ("Create customer", "click", "submit_form")
for purpose in ["Open the awards page"]:
    STEP_BY_PURPOSE[purpose] = ("Awards", "click", "absent_probe")
for purpose in ["Open the likes tab"]:
    STEP_BY_PURPOSE[purpose] = ("Likes", "click", "absent_probe")
for purpose in ["Open the learning tab"]:
    STEP_BY_PURPOSE[purpose] = ("Learning", "click", "absent_probe")


def relabel(record):
    mapping = STEP_BY_PURPOSE.get(record.get("purpose"))
    if mapping is None:
        return None
    target_name, action_kind, step = mapping
    candidates = record.get("contextCandidates") or record.get("context_candidates") or []
    index = next(
        (i for i, c in enumerate(candidates) if c.get("name") == target_name),
        None,
    )
    if index is None:
        return None
    return {
        "image_b64": record.get("imageB64") or record.get("image_b64"),
        "purpose": record["purpose"],
        "intent_kind": record.get("intentKind") or record.get("intent_kind"),
        "stuck": record.get("stuck", "targetMissing"),
        "context_url": record.get("contextUrl") or record.get("context_url"),
        "context_candidates": [
            {"role": c.get("role"), "name": c.get("name")} for c in candidates
        ],
        "target_index": index,
        "model_response": {"confidence": 1.0, "action": {"kind": action_kind}},
        "success": True,
        "journey": "gauntlet-live",
        "step": step,
        "outcome_stage": "liveWindowScriptedLabel",
    }


def validate_positives(records):
    """Drop harvest positives whose verified pick was the wrong element.

    The runtime's verification proves the action LANDED, not that it was
    right for the purpose: a committed wrong pick (e.g. a nav link instead
    of the submit button) verifies as success=True with a valid target
    index. Against the scripted step mapping those records are poison —
    they would teach the model the wrong target. Only records whose
    purpose has a known scripted target are checked; unknown purposes pass
    through (they may come from other journeys).
    """
    kept = []
    dropped = []
    for record in records:
        purpose = record.get("purpose")
        mapping = STEP_BY_PURPOSE.get(purpose)
        if mapping is None or record.get("success") is not True:
            kept.append(record)
            continue
        candidates = (
            record.get("contextCandidates") or record.get("context_candidates") or []
        )
        index = (
            record.get("targetIndex")
            if "targetIndex" in record
            else record.get("target_index")
        )
        picked = (
            candidates[index].get("name")
            if isinstance(index, int) and 0 <= index < len(candidates)
            else None
        )
        if picked == mapping[0]:
            kept.append(record)
        else:
            dropped.append((purpose, mapping[0], picked))
    return kept, dropped


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", required=True, help="harvest corpus JSONL (engine records)")
    parser.add_argument("--output", required=True, help="relabeled ground-truth JSONL")
    args = parser.parse_args()

    relabeled = []
    validated_positives = []
    skipped = 0
    dropped_wrong = []
    with open(args.input) as f:
        for line in f:
            if not line.strip():
                continue
            record = json.loads(line)
            # Only abstain/failure records need relabeling; verified
            # positives are validated against the scripted target — a
            # mechanically-verified wrong pick is poison (§4w lesson).
            if record.get("success") is not False:
                validated_positives.append(record)
                continue
            out = relabel(record)
            if out is None:
                skipped += 1
            else:
                relabeled.append(out)

    validated_positives, dropped_wrong = validate_positives(validated_positives)
    with open(args.output, "w") as f:
        # Output = validated production positives (as captured) + relabeled
        # ground truth; both are fit for the corpus, negatives stay out.
        for record in validated_positives + relabeled:
            f.write(json.dumps(record) + "\n")
    for purpose, wanted, picked in dropped_wrong:
        print(f"dropped wrong pick: {purpose!r} picked {picked!r}, wanted {wanted!r}")
    print(
        f"relabeled {len(relabeled)} production-window records "
        f"(skipped {skipped} unmappable); validated {len(validated_positives)} "
        f"positives ({len(dropped_wrong)} wrong picks dropped)"
    )


if __name__ == "__main__":
    main()
