"""Structural validation for versioned, read-only capability observations."""
from datetime import datetime
import re
import uuid

_TIMESTAMP = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?(?:Z|[+-]\d{2}:\d{2})$")

_REASONS = ("unsupportedByEngine", "missingCapability", "policyDisabled", "storageUnavailable", "workerUnavailable", "connectionUnknown", "disconnected", "metadataUnavailable")

def _text(value, maximum):
    if not isinstance(value, str):
        return False
    try:
        return 0 < len(value.encode("utf-8")) <= maximum and not re.search(r"[\x00-\x1f\x7f]", value)
    except UnicodeError:
        return False

def is_session_capabilities(value):
    if not isinstance(value, dict) or set(value) != {"schemaVersion", "sessionId", "observedAt", "engine", "connection", "features"}:
        return False
    if type(value["schemaVersion"]) is not int or value["schemaVersion"] != 1:
        return False
    if not isinstance(value["sessionId"], str) or not isinstance(value["observedAt"], str):
        return False
    if not _TIMESTAMP.fullmatch(value["observedAt"]):
        return False
    try:
        uuid.UUID(value["sessionId"])
        if datetime.fromisoformat(value["observedAt"].replace("Z", "+00:00")).tzinfo is None:
            return False
    except (ValueError, TypeError):
        return False
    if value["engine"] is not None and not _text(value["engine"], 32):
        return False
    if value["connection"] not in ("connected", "disconnected", "unknown"):
        return False
    features = value["features"]
    if not isinstance(features, list) or not 1 <= len(features) <= 64:
        return False
    names = set()
    for feature in features:
        if not isinstance(feature, dict) or set(feature) != {"name", "supported", "available", "reason", "requiredCapabilities", "policyRequirement"}:
            return False
        if not _text(feature["name"], 128) or feature["name"] in names:
            return False
        names.add(feature["name"])
        if feature["supported"] is not None and not isinstance(feature["supported"], bool):
            return False
        if not isinstance(feature["available"], bool) or (feature["reason"] is not None and feature["reason"] not in _REASONS):
            return False
        capabilities = feature["requiredCapabilities"]
        if not isinstance(capabilities, list) or len(capabilities) > 64 or not all(_text(capability, 128) for capability in capabilities):
            return False
        if feature["policyRequirement"] is not None and not _text(feature["policyRequirement"], 128):
            return False
    return True
