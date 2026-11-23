"""Intent envelope builders.

The wire shape matches ``packages/typescript-sdk/src/intents.ts``:
``{kind: "intent", input: {kind, input}}`` inside a schemaVersion 2 envelope.
"""

from __future__ import annotations

from typing import Any, Dict, Mapping, Optional

MAX_INTENT_PURPOSE_BYTES = 256
DEFAULT_DISMISS_OBSTRUCTION_TIMEOUT_MS = 5_000
DEFAULT_DETECT_CHALLENGE_TIMEOUT_MS = 15_000
DEFAULT_SOLVE_CHALLENGE_TIMEOUT_MS = 30_000


def assert_intent_purpose(purpose: str) -> None:
    if not isinstance(purpose, str) or purpose == "":
        raise ValueError("intent purpose must be non-empty")
    if len(purpose.encode("utf-8")) > MAX_INTENT_PURPOSE_BYTES:
        raise ValueError(f"intent purpose exceeds {MAX_INTENT_PURPOSE_BYTES} bytes")


def _default_hints() -> Dict[str, Any]:
    return {
        "role": None,
        "accessibleName": None,
        "nearText": None,
        "ordinal": None,
        "framePath": [],
        "shadowPath": [],
        "allowBestMatch": False,
    }


def _with_hints(hints: Optional[Mapping[str, Any]]) -> Dict[str, Any]:
    merged = _default_hints()
    if hints:
        merged.update(hints)
    return merged


def intent_hints_from_accessibility_target(target: Mapping[str, Any]) -> Dict[str, Any]:
    hints: Dict[str, Any] = {
        "role": target.get("role"),
        "nearText": {"kind": "exact", "value": target.get("accessibleName")},
    }
    if "ordinal" in target:
        hints["ordinal"] = target["ordinal"]
    return hints


def _normalize_fill_value(value: Mapping[str, Any]) -> Dict[str, Any]:
    if value.get("kind") == "setText":
        return {
            "kind": "setText",
            "value": value.get("value"),
            "clearFirst": value.get("clearFirst", True),
        }
    return dict(value)


def _intent(kind: str, body: Mapping[str, Any]) -> Dict[str, Any]:
    return {"kind": "intent", "input": {"kind": kind, "input": dict(body)}}


def locate_runtime_command(purpose: str, hints: Optional[Mapping[str, Any]] = None) -> Dict[str, Any]:
    assert_intent_purpose(purpose)
    return _intent("locate", {"purpose": purpose, "hints": _with_hints(hints)})


def fill_runtime_command(
    purpose: str, value: Mapping[str, Any], hints: Optional[Mapping[str, Any]] = None
) -> Dict[str, Any]:
    assert_intent_purpose(purpose)
    return _intent(
        "fill",
        {"purpose": purpose, "hints": _with_hints(hints), "value": _normalize_fill_value(value)},
    )


def complete_form_runtime_command(purpose: str, fields: list) -> Dict[str, Any]:
    assert_intent_purpose(purpose)
    if not fields:
        raise ValueError("completeForm fields must not be empty")
    if len(fields) > 128:
        raise ValueError("completeForm fields must not exceed 128 items")
    names = set()
    built = []
    for field in fields:
        name = field.get("name", "")
        if not isinstance(name, str) or name.strip() == "":
            raise ValueError("completeForm field name must not be empty")
        if name in names:
            raise ValueError(f"duplicate completeForm field name: {name}")
        names.add(name)
        assert_intent_purpose(field.get("purpose", ""))
        built.append(
            {
                **field,
                "hints": _with_hints(field.get("hints")),
                "value": _normalize_fill_value(field["value"]),
            }
        )
    return _intent("completeForm", {"purpose": purpose, "fields": built})


def submit_and_verify_runtime_command(
    purpose: str,
    hints: Optional[Mapping[str, Any]] = None,
    expected_state: Optional[Mapping[str, Any]] = None,
) -> Dict[str, Any]:
    assert_intent_purpose(purpose)
    body: Dict[str, Any] = {"purpose": purpose, "hints": _with_hints(hints)}
    if expected_state is not None:
        body["expectedState"] = dict(expected_state)
    return _intent("submitAndVerify", body)


def wait_for_state_runtime_command(condition: Mapping[str, Any], timeout_ms: int) -> Dict[str, Any]:
    return _intent("waitForState", {"condition": dict(condition), "timeoutMs": timeout_ms})


def follow_runtime_command(
    purpose: str,
    expected_destination: Mapping[str, Any],
    hints: Optional[Mapping[str, Any]] = None,
    boundary: bool = False,
) -> Dict[str, Any]:
    assert_intent_purpose(purpose)
    return _intent(
        "follow",
        {
            "purpose": purpose,
            "hints": _with_hints(hints),
            "expectedDestination": dict(expected_destination),
            "boundary": boundary,
        },
    )


def enter_code_runtime_command(
    code: str,
    hints: Optional[Mapping[str, Any]] = None,
    submit: bool = True,
    expected_state: Optional[Mapping[str, Any]] = None,
) -> Dict[str, Any]:
    if code.replace(" ", "").replace("-", "") == "":
        raise ValueError("enterCode intent needs a non-empty code")
    body: Dict[str, Any] = {"code": code, "hints": _with_hints(hints), "submit": submit}
    if expected_state is not None:
        body["expectedState"] = dict(expected_state)
    return _intent("enterCode", body)


def dismiss_obstruction_runtime_command(
    purpose: str,
    hints: Optional[Mapping[str, Any]] = None,
    timeout_ms: int = DEFAULT_DISMISS_OBSTRUCTION_TIMEOUT_MS,
) -> Dict[str, Any]:
    assert_intent_purpose(purpose)
    return _intent(
        "dismissObstruction",
        {"purpose": purpose, "hints": _with_hints(hints), "timeoutMs": timeout_ms},
    )


def extract_runtime_command(purpose: str, fields: list) -> Dict[str, Any]:
    assert_intent_purpose(purpose)
    if not fields:
        raise ValueError("extract intent must include at least one field")
    seen = set()
    built = []
    for field in fields:
        assert_intent_purpose(field.get("purpose", ""))
        name = str(field.get("name", "")).strip()
        if name == "":
            raise ValueError("extract field name must not be empty")
        if name in seen:
            raise ValueError(f"duplicate extract field name: {name}")
        seen.add(name)
        built.append(
            {
                "name": name,
                "purpose": field["purpose"],
                "hints": _with_hints(field.get("hints")),
                "value": field["value"],
            }
        )
    return _intent("extract", {"purpose": purpose, "fields": built})


def _challenge(kind: str, purpose: str, hints: Optional[Mapping[str, Any]], default_timeout: int) -> Dict[str, Any]:
    assert_intent_purpose(purpose)
    source = hints or {}
    built: Dict[str, Any] = {"timeoutMs": source.get("timeoutMs", default_timeout)}
    if "region" in source:
        built["region"] = source["region"]
    return _intent(kind, {"purpose": purpose, "hints": built})


def detect_challenge_runtime_command(
    purpose: str, hints: Optional[Mapping[str, Any]] = None
) -> Dict[str, Any]:
    return _challenge("detectChallenge", purpose, hints, DEFAULT_DETECT_CHALLENGE_TIMEOUT_MS)


def solve_challenge_runtime_command(
    purpose: str, hints: Optional[Mapping[str, Any]] = None
) -> Dict[str, Any]:
    return _challenge("solveChallenge", purpose, hints, DEFAULT_SOLVE_CHALLENGE_TIMEOUT_MS)


def intent_envelope(meta: Mapping[str, Any], command: Mapping[str, Any]) -> Dict[str, Any]:
    if command.get("kind") != "intent":
        raise ValueError('intent_envelope requires a command with kind "intent"')
    return {
        "schemaVersion": meta.get("schemaVersion", 2),
        "commandId": meta["commandId"],
        "workflowId": meta["workflowId"],
        "attemptId": meta["attemptId"],
        "sessionId": meta["sessionId"],
        "pageId": meta.get("pageId"),
        "deadline": meta["deadline"],
        "command": dict(command),
    }


def locate_envelope(
    meta: Mapping[str, Any], purpose: str, hints: Optional[Mapping[str, Any]] = None
) -> Dict[str, Any]:
    return intent_envelope(meta, locate_runtime_command(purpose, hints))


def fill_envelope(
    meta: Mapping[str, Any],
    purpose: str,
    value: Mapping[str, Any],
    hints: Optional[Mapping[str, Any]] = None,
) -> Dict[str, Any]:
    return intent_envelope(meta, fill_runtime_command(purpose, value, hints))
