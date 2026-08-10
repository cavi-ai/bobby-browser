"""Canonical vision provider interface for Bobby Browser.

Mirrors the Rust `VisionAssist` trait and `vision-proxy` wire format:

    propose(request) -> {"confidence": f32, "action": {...}}
    extract(request) -> {"value": <json>}

Action kinds match `crates/vision-proxy/src/wire.rs`:
    - {"kind": "click", "x": f64, "y": f64}        CSS pixels in the screenshot
    - {"kind": "typeText", "text": str}
    - {"kind": "extractValue", "value": str}

Providers are swappable via `create_provider()`: mlx-vlm (direct local
inference), ollama, lmstudio, or openai — same interface, different backend.
"""

from __future__ import annotations

import base64
import json
import logging
from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from typing import Any, Optional

log = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# Canonical wire types (mirror crates/vision-proxy/src/wire.rs)
# ---------------------------------------------------------------------------

@dataclass
class VisionContextCandidate:
    role: str
    name: str
    ordinal: Optional[int] = None


@dataclass
class VisionContext:
    url: Optional[str] = None
    candidates: list = field(default_factory=list)
    recent_command_kinds: list = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: Optional[dict]) -> "VisionContext":
        if not data:
            return cls()
        return cls(
            url=data.get("url"),
            candidates=[
                VisionContextCandidate(
                    role=c.get("role", ""),
                    name=c.get("name", ""),
                    ordinal=c.get("ordinal"),
                )
                for c in data.get("candidates", [])
            ],
            recent_command_kinds=data.get("recentCommandKinds", []),
        )


@dataclass
class ProposeRequest:
    """Canonical propose request (camelCase mirrors the wire format)."""
    purpose: str
    intent_kind: str
    stuck: str
    screenshot_b64: str
    context: Optional[VisionContext] = None

    @classmethod
    def from_dict(cls, data: dict) -> "ProposeRequest":
        return cls(
            purpose=data["purpose"],
            intent_kind=data["intentKind"],
            stuck=data["stuck"],
            screenshot_b64=data["screenshotPng"],
            context=VisionContext.from_dict(data.get("context")),
        )


@dataclass
class ProposeResponse:
    confidence: float
    action: dict

    def to_dict(self) -> dict:
        return {"confidence": self.confidence, "action": self.action}

    def validate(self) -> None:
        if not (0.0 <= self.confidence <= 1.0):
            raise ValueError(f"confidence out of range: {self.confidence}")
        kind = self.action.get("kind")
        if kind not in ("click", "typeText", "extractValue"):
            raise ValueError(f"invalid action kind: {kind}")
        if kind == "click":
            x, y = self.action.get("x"), self.action.get("y")
            if not (isinstance(x, (int, float)) and isinstance(y, (int, float))):
                raise ValueError(f"click coordinates not finite: x={x}, y={y}")


# ---------------------------------------------------------------------------
# Canonical provider interface
# ---------------------------------------------------------------------------

class VisionProvider(ABC):
    """Canonical interface all vision backends implement."""

    name: str = "abstract"

    @abstractmethod
    def propose(self, request: ProposeRequest) -> ProposeResponse:
        """Return a confidence-scored action for a screenshot + context."""
        ...

    @abstractmethod
    def extract(self, schema: dict, content: str, purpose: Optional[str]) -> dict:
        """Return a JSON value matching `schema` extracted from page content."""
        ...

    # -- shared prompt construction (matches crates/vision-proxy/src/ollama.rs) --

    PROPOSE_SYSTEM = (
        "You are a vision assistant for a browser automation agent. "
        "Analyze the screenshot and return ONLY valid JSON matching one of these shapes:\n"
        '{"confidence": 0.0..1.0, "action": {"kind": "click", "x": number, "y": number}}\n'
        '{"confidence": 0.0..1.0, "action": {"kind": "typeText", "text": string}}\n'
        '{"confidence": 0.0..1.0, "action": {"kind": "extractValue", "value": string}}\n'
        "Click coordinates are CSS pixels relative to the screenshot image. "
        "Do not include markdown fences, comments, or any text outside the JSON object."
    )

    EXTRACT_SYSTEM = (
        'Return only JSON {"value": <json matching the caller schema>}.'
    )

    def build_propose_prompt(self, request: ProposeRequest) -> str:
        text = (
            f"purpose: {request.purpose}\n"
            f"intentKind: {request.intent_kind}\n"
            f"stuck: {request.stuck}"
        )
        ctx = request.context
        if ctx:
            if ctx.url:
                text += f"\nurl: {ctx.url}"
            if ctx.candidates:
                text += "\ncandidates:"
                for c in ctx.candidates:
                    text += f'\n- {c.role} "{c.name}"'
                    if c.ordinal is not None:
                        text += f" (#{c.ordinal})"
            if ctx.recent_command_kinds:
                text += f"\nrecentCommands: {', '.join(ctx.recent_command_kinds)}"
        return text

    @staticmethod
    def decode_image(b64: str):
        """Decode base64 PNG to PIL Image."""
        from PIL import Image
        import io
        return Image.open(io.BytesIO(base64.b64decode(b64))).convert("RGB")

    @staticmethod
    def parse_json_content(content: str) -> dict:
        """Extract a JSON object from model output, stripping markdown fences
        and tolerating trailing junk after the closing brace."""
        text = content.strip()
        if text.startswith("```json"):
            text = text[7:]
        elif text.startswith("```"):
            text = text[3:]
        text = text.strip()
        if text.endswith("```"):
            text = text[:-3].strip()

        try:
            return json.loads(text)
        except json.JSONDecodeError:
            pass

        start = text.find("{")
        if start < 0:
            raise ValueError("no JSON object in model output")

        # Depth-aware scan for the matching closing brace; ignores braces
        # inside string literals.
        depth = 0
        in_string = False
        escape = False
        for i in range(start, len(text)):
            ch = text[i]
            if escape:
                escape = False
                continue
            if ch == "\\":
                if in_string:
                    escape = True
                continue
            if ch == '"':
                in_string = not in_string
                continue
            if in_string:
                continue
            if ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
                if depth == 0:
                    return json.loads(text[start : i + 1])

        raise ValueError("unbalanced braces in model output")

    @classmethod
    def normalize_action(cls, action: Any) -> dict:
        """Map model-specific action kinds onto the canonical schema."""
        if isinstance(action, str):
            # Model emitted a bare action name, e.g. "click"
            action = {"kind": action}
        if not isinstance(action, dict):
            action = {"kind": "click"}

        kind = action.get("kind", "click")
        if kind in ("terminate", "abort", "refuse", "none", "noop"):
            # Model abstained: emit a zero-coordinate click; the low
            # confidence (prefilled 0.0) fails the runtime floor, which is
            # the correct abstention path.
            kind = "click"
            action = {"kind": kind, "x": 0.0, "y": 0.0}
        if kind in ("left_click", "leftClick", "mouse_click", "press"):
            kind = "click"
        elif kind in ("type", "inputText", "enterText", "text"):
            kind = "typeText"
        elif kind in ("extract", "read", "getValue"):
            kind = "extractValue"

        normalized = {"kind": kind}
        if kind == "click":
            # Models emit coordinates variously: {x, y}, {coordinate: [x, y]},
            # {position: {x, y}}, or {clickX, clickY}.
            x = action.get("x", action.get("clickX"))
            y = action.get("y", action.get("clickY"))
            coord = action.get("coordinate") or action.get("coordinates") or action.get("position")
            if (x is None or y is None) and coord is not None:
                if isinstance(coord, (list, tuple)) and len(coord) >= 2:
                    x, y = coord[0], coord[1]
                elif isinstance(coord, dict):
                    x = coord.get("x", x)
                    y = coord.get("y", y)
            normalized["x"] = float(x if x is not None else 0.0)
            normalized["y"] = float(y if y is not None else 0.0)
        elif kind == "typeText":
            normalized["text"] = str(action.get("text", action.get("value", "")))
        else:
            normalized["value"] = str(action.get("value", action.get("text", "")))
        return normalized

    @classmethod
    def normalize_response(cls, raw: dict) -> ProposeResponse:
        """Normalize a parsed model response into the canonical schema."""
        if not isinstance(raw, dict):
            raw = {"action": raw}
        action = raw.get("action", {})

        # Model may emit "action": "click" with coordinate fields as siblings
        if isinstance(action, str):
            action = {
                "kind": action,
                **{k: raw[k] for k in ("x", "y", "coordinate", "coordinates", "position", "clickX", "clickY", "text", "value") if k in raw},
            }

        normalized_action = cls.normalize_action(action)
        confidence = float(raw.get("confidence", 0.5))
        confidence = max(0.0, min(1.0, confidence))
        response = ProposeResponse(confidence=confidence, action=normalized_action)
        response.validate()
        return response
