"""LM Studio provider (OpenAI-compatible server on port 1234)."""

from __future__ import annotations

import logging
from typing import Optional

import requests

from .base import ProposeRequest, ProposeResponse, VisionProvider

log = logging.getLogger(__name__)

DEFAULT_BASE_URL = "http://127.0.0.1:1234"
DEFAULT_MODEL = "local-model"


class LmStudioProvider(VisionProvider):
    name = "lmstudio"

    def __init__(self, model: str = DEFAULT_MODEL, base_url: str = DEFAULT_BASE_URL):
        self.model = model
        self.base_url = base_url.rstrip("/")

    def _chat(self, system: str, user_text: str, image_b64: Optional[str]) -> dict:
        user_content = []
        if image_b64:
            user_content = [
                {"type": "text", "text": user_text},
                {"type": "image_url", "image_url": {"url": f"data:image/png;base64,{image_b64}"}},
            ]
        else:
            user_content = [{"type": "text", "text": user_text}]

        response = requests.post(
            f"{self.base_url}/v1/chat/completions",
            json={
                "model": self.model,
                "messages": [
                    {"role": "system", "content": system},
                    {"role": "user", "content": user_content},
                ],
            },
            timeout=60,
        )
        response.raise_for_status()
        result = response.json()
        content = result["choices"][0]["message"]["content"]
        return self.parse_json_content(content)

    def propose(self, request: ProposeRequest) -> ProposeResponse:
        raw = self._chat(
            self.PROPOSE_SYSTEM,
            self.build_propose_prompt(request),
            request.screenshot_b64,
        )
        return self.normalize_response(raw)

    def extract(self, schema: dict, content: str, purpose: Optional[str]) -> dict:
        import json as jsonlib
        user_text = (
            f"extract structured value from page content.\n"
            f"purpose: {purpose or ''}\n"
            f"schema: {jsonlib.dumps(schema)}\n"
            f"content:\n{content}"
        )
        raw = self._chat(self.EXTRACT_SYSTEM, user_text, None)
        return raw["value"] if "value" in raw else raw
