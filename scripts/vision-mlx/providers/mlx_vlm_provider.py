"""Direct local inference via mlx-vlm on Apple Silicon.

Loads a vision-language model once and runs propose/extract in-process.
This is the canonical fast path: no HTTP hop, no external server, models
quantized for Metal.

Default model: Qwen2.5-VL-3B-Instruct-4bit (~2GB, ~200 tok/s on M-series).
Opt into larger models with VISION_MLX_MODEL (7B for better accuracy).
"""

from __future__ import annotations

import logging
import os
import tempfile
from typing import Optional

from .base import ProposeRequest, ProposeResponse, VisionProvider

log = logging.getLogger(__name__)

DEFAULT_MODEL = "mlx-community/Qwen2.5-VL-3B-Instruct-4bit"


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
        from mlx_vlm.prompt_utils import apply_chat_template

        image = self.decode_image(request.screenshot_b64)
        prompt_text = self.build_propose_prompt(request)

        # Qwen2.5-VL processor needs an image on disk
        with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as tmp:
            image.save(tmp.name)
            image_path = tmp.name

        try:
            full_prompt = apply_chat_template(
                self._processor,
                self._model.config,
                f"{self.PROPOSE_SYSTEM}\n\n{prompt_text}",
                num_images=1,
            )
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
            try:
                raw = self.parse_json_content(raw_text)
            except (ValueError, KeyError) as error:
                # Malformed model output degrades to a clean abstain: zero
                # confidence fails the runtime floor, which is the designed
                # fallback — a 500 here would read as provider failure.
                log.warning("unparseable model output, abstaining: %s", error)
                return ProposeResponse(
                    confidence=0.0,
                    action={"kind": "click", "x": 0.0, "y": 0.0},
                )
            log.debug("parsed: %s", raw)
            return self.normalize_response(raw)
        finally:
            os.unlink(image_path)

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
