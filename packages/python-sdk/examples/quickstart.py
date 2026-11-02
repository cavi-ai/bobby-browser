"""Fill and submit a form with the Python SDK, then keep a screenshot.

Run against a bobby runtime:

    BOBBY_RUNTIME_URL="$(bobby runtime start)" \\
    AUTOMATION_RUNTIME_TOKEN="$(bobby token --stdout)" \\
    BOBBY_EXAMPLE_URL=https://your.site/signup \\
    python3 quickstart.py

The page needs a "Full name" textbox, a "Work email" textbox, a "Submit"
button, and a status element labelled "Confirmation" that shows "Thanks" once
the form is sent.
"""

import os
import uuid
from datetime import datetime, timedelta, timezone

from bobby_browser import BrowserRuntimeClient, RequestOptions

client = BrowserRuntimeClient(
    os.environ.get("BOBBY_RUNTIME_URL", "http://127.0.0.1:7777"),
    os.environ["AUTOMATION_RUNTIME_TOKEN"],
)
form_url = os.environ["BOBBY_EXAMPLE_URL"]


def options(timeout_ms: int = 30_000) -> RequestOptions:
    """A fresh idempotency key: resend the same options to retry safely."""
    return RequestOptions(idempotency_key=str(uuid.uuid4()), timeout_ms=timeout_ms)


def now(seconds: int = 0) -> str:
    return (datetime.now(timezone.utc) + timedelta(seconds=seconds)).isoformat()


session = client.create_session({"profile": "default", "proxy": None}, options())
page = client.open_page({"session_id": session["id"]}, options())
workflow_id = str(uuid.uuid4())


def run(command: dict, command_id: str = "", attempt_id: str = "") -> dict:
    """Submit one command in this workflow and require it to complete.

    The command deadline must fall inside the request's own deadline: the
    request waits 60 s for a command that must finish within 50 s.
    """
    envelope = {
        "schemaVersion": 2,
        "commandId": command_id or str(uuid.uuid4()),
        "workflowId": workflow_id,
        "attemptId": attempt_id or str(uuid.uuid4()),
        "sessionId": session["id"],
        "pageId": page["id"],
        "deadline": now(50),
        "command": command,
    }
    outcome = client.submit_command(envelope, options(timeout_ms=60_000))
    if outcome["status"] != "completed":
        raise SystemExit(f"{command['input']['kind']}: {outcome['status']} {outcome.get('error')}")
    return outcome


def primitive(kind: str, input: dict) -> dict:
    return run({"kind": "primitive", "input": {"kind": kind, "input": input}})


def intent(kind: str, input: dict, **ids: str) -> dict:
    return run({"kind": "intent", "input": {"kind": kind, "input": input}}, **ids)


def text(value: str) -> dict:
    return {"kind": "setText", "value": value, "clearFirst": True}


try:
    primitive(
        "navigate",
        {"url": form_url, "waitUntil": "domContentLoaded", "timeoutMs": 30_000},
    )

    # Intents name a purpose; the runtime finds the control, acts, and verifies.
    filled = intent(
        "completeForm",
        {
            "purpose": "sign up",
            "fields": [
                {"name": "name", "purpose": "Full name", "value": text("Maya Chen"),
                 "hints": {"role": "textbox", "accessibleName": "Full name"}},
                {"name": "email", "purpose": "Work email", "value": text("maya@atlas.example"),
                 "hints": {"role": "textbox", "accessibleName": "Work email"}},
            ],
        },
    )

    # A submit changes state on the site, so it runs only after a checkpoint
    # that names it: the workflow, attempt, session, page, and the submit's
    # own command id. Its evidence is the verified form fill.
    submit_id, attempt_id = str(uuid.uuid4()), str(uuid.uuid4())
    client.create_checkpoint(
        {
            "checkpoint": {
                "schemaVersion": 1,
                "checkpointId": str(uuid.uuid4()),
                "workflowId": workflow_id,
                "attemptId": attempt_id,
                "sessionId": session["id"],
                "pageId": page["id"],
                "restartUrl": form_url,
                "currentUrl": form_url,
                "cursor": None,
                "boundaryCommandId": submit_id,
                "recoveryClass": "boundary",
                "invariants": [],
                "replayableInputs": [],
                "evidence": [],
                "createdAt": now(),
            },
            "evidenceRefs": [filled["commandId"]],
        },
        options(),
    )
    intent(
        "submitAndVerify",
        {
            "purpose": "Submit",
            "hints": {"role": "button", "accessibleName": "Submit"},
            "expectedState": {
                "condition": {"kind": "text",
                              "target": {"role": "status", "accessibleName": "Confirmation"},
                              "matcher": {"kind": "contains", "value": "Thanks"}},
                "timeoutMs": 10_000,
            },
        },
        command_id=submit_id,
        attempt_id=attempt_id,
    )

    extracted = intent(
        "extract",
        {
            "purpose": "confirmation",
            "fields": [{"name": "message", "purpose": "confirmation message",
                        "hints": {"role": "status", "accessibleName": "Confirmation"}}],
        },
    )
    for item in extracted["evidence"]:
        if item["kind"] == "extraction":
            # An unresolved field has no value and says why in errorCode.
            print(f"{item['field']}: {item.get('value', item.get('errorCode'))}")

    # Artifacts come back digest-verified: no bytes unless SHA-256 matches.
    shot = primitive("captureScreenshot", {"mode": {"kind": "viewport"}})
    reference = next(item for item in shot["evidence"] if item["kind"] == "screenshot")
    image = client.read_artifact(
        {
            "artifactId": reference["artifactId"],
            "sha256": reference["sha256"],
            "bytes": reference["bytes"],
            "mediaType": reference["mediaType"],
        }
    )
    with open("confirmation.png", "wb") as file:
        file.write(image)
    print(f"screenshot: {len(image)} bytes")
finally:
    client.delete_session(session["id"])
