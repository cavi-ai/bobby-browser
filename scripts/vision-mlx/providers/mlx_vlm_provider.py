"""Direct local inference via mlx-vlm on Apple Silicon.

Loads a vision-language model once and runs propose/extract in-process.
This is the canonical fast path: no HTTP hop, no external server, models
quantized for Metal.

Default model: Qwen3.5-27B-4bit — grounds precisely and drives
solveChallenge end-to-end (its normalized [0,1000) coordinates are
rescaled by this provider). The old Qwen2.5-VL-3B default could not.
"""

from __future__ import annotations

import logging
import os
import tempfile
from typing import Optional

from .base import ProposeRequest, ProposeResponse, VisionProvider

log = logging.getLogger(__name__)

DEFAULT_MODEL = "mlx-community/Qwen3.5-27B-4bit"

# Model families that emit click coordinates normalized to [0, 1000) instead
# of absolute pixels (Qwen3-VL changed the convention; Qwen2.5-VL is
# absolute). Override with VISION_COORD_SPACE=normalized|absolute.
_NORMALIZED_COORD_MARKERS = ("qwen3-vl", "qwen3.5")


def uses_normalized_coords(model_name: str) -> bool:
    override = os.environ.get("VISION_COORD_SPACE", "").strip().lower()
    if override in ("normalized", "absolute"):
        return override == "normalized"
    lowered = model_name.lower()
    return any(marker in lowered for marker in _NORMALIZED_COORD_MARKERS)


def rescale_click(action: dict, width: int, height: int) -> dict:
    """Map normalized [0, 1000) click coordinates onto the screenshot frame."""
    if action.get("kind") != "click":
        return action
    return {
        "kind": "click",
        "x": action["x"] * width / 1000.0,
        "y": action["y"] * height / 1000.0,
    }


class MlxVlmProvider(VisionProvider):
    name = "mlx-vlm"

    # Prefill forces the response to begin inside the JSON skeleton, so the
    # model fills values semantically instead of choosing to fence or waffle.
    PROPOSE_PREFILL = '{"confidence": '

    def __init__(self, model_name: str = DEFAULT_MODEL, prefill: bool = True):
        self.model_name = model_name
        self.prefill = prefill
        self._model = None
        self._processor = None

    def _ensure_loaded(self):
        if self._model is not None:
            return
        from mlx_vlm import load
        log.info("Loading %s ...", self.model_name)
        self._model, self._processor = load(self.model_name)
        log.info("Model loaded.")

    def propose(self, request: ProposeRequest) -> ProposeResponse:
        self._ensure_loaded()
        from mlx_vlm import generate

        image = self.decode_image(request.screenshot_b64)
        prompt_text = self.build_propose_prompt(request)

        # Qwen2.5-VL processor needs an image on disk
        with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as tmp:
            image.save(tmp.name)
            image_path = tmp.name

        try:
            full_prompt = self._chat_prompt(prompt_text, image_path)
            prefill = self.PROPOSE_PREFILL if self.prefill else ""
            result = generate(
                self._model,
                self._processor,
                full_prompt + prefill,
                image=[image_path],
                max_tokens=256,
                temp=0.1,
                verbose=False,
            )
            raw_text = prefill + (result.text if hasattr(result, "text") else str(result))
            log.debug("model raw output: %s", raw_text)
            raw = self.parse_json_content(raw_text)
            log.debug("parsed: %s", raw)
            response = self.normalize_response(raw)
            if uses_normalized_coords(self.model_name):
                response = ProposeResponse(
                    confidence=response.confidence,
                    action=rescale_click(response.action, image.width, image.height),
                )
            return response
        finally:
            os.unlink(image_path)

    def _chat_prompt(self, text: str, image_path: str) -> str:
        """System+user chat template when the processor supports it: the
        single-message template makes some families drop JSON keys (observed:
        Qwen3.5 emitting {"x": 298, 684} with no "y" key)."""
        try:
            messages = [
                {"role": "system", "content": self.PROPOSE_SYSTEM},
                {
                    "role": "user",
                    "content": [
                        {"type": "image", "image": image_path},
                        {"type": "text", "text": text},
                    ],
                },
            ]
            return self._processor.apply_chat_template(
                messages, tokenize=False, add_generation_prompt=True
            )
        except Exception:
            from mlx_vlm.prompt_utils import apply_chat_template

            return apply_chat_template(
                self._processor,
                self._model.config,
                f"{self.PROPOSE_SYSTEM}\n\n{text}",
                num_images=1,
            )

    def extract(self, schema: dict, content: str, purpose: Optional[str]) -> dict:
        self._ensure_loaded()
        import json as jsonlib
        from mlx_vlm import generate

        user_text = (
            f"extract structured value from page content.\n"
            f"purpose: {purpose or ''}\n"
            f"schema: {jsonlib.dumps(schema)}\n"
            f"content:\n{content}"
        )
        result = generate(
            self._model,
            self._processor,
            f"{self.EXTRACT_SYSTEM}\n\n{user_text}",
            max_tokens=512,
            temp=0.1,
            verbose=False,
        )
        raw = self.parse_json_content(result.text if hasattr(result, "text") else str(result))
        return raw["value"] if "value" in raw else raw
