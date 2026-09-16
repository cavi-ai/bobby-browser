import json
from pathlib import Path


with (Path(__file__).with_name("candidate_action_contract.json")).open() as contract_file:
    ACTION_SPECS = tuple(json.load(contract_file)["actions"])

CANDIDATE_ACTION_KINDS = frozenset(spec["kind"] for spec in ACTION_SPECS)
CANDIDATE_ACTION_BY_INTENT = {
    intent: spec["kind"] for spec in ACTION_SPECS for intent in spec["intents"]
}
LEGACY_ACTION_BY_CANDIDATE = {
    spec["kind"]: spec["legacyKind"] for spec in ACTION_SPECS
}
SNAKE_CASE_CANDIDATE_KINDS = {
    spec["acpKind"]: spec["kind"] for spec in ACTION_SPECS
}
CANDIDATE_PROMPT_RULES = ", ".join(
    f'{spec["kind"]} for {"/".join(spec["intents"])}' for spec in ACTION_SPECS
)
