"""A vision-language model served over an OpenAI-compatible API, such as by
`mlx-agent serve`. The prompt and answer handling match the in-process mlx-vlm
provider; the server holds the weights.
"""

from __future__ import annotations

import base64
import json
import logging
import struct
import urllib.request
from typing import Optional

from .base import ProposeRequest, ProposeResponse, VisionProvider
from .mlx_vlm_provider import DEFAULT_MODEL, rescale_click, uses_normalized_coords

log = logging.getLogger(__name__)

DEFAULT_BASE_URL = "http://127.0.0.1:8083"


def png_size(png_b64: str) -> tuple[int, int]:
    """Width and height from a base64 PNG's IHDR chunk."""
    head = base64.b64decode(png_b64[:44])
    if head[:8] != b"\x89PNG\r\n\x1a\n" or head[12:16] != b"IHDR":
        raise ValueError("screenshot is not a PNG")
    return struct.unpack(">II", head[16:24])


class MlxServerProvider(VisionProvider):
    name = "mlx-server"

    def __init__(self, model_name: str = DEFAULT_MODEL, base_url: str = DEFAULT_BASE_URL, timeout: float = 120):
        self.model_name = model_name
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout

    def _complete(self, system: str, content: list, max_tokens: int) -> str:
        body = {
            "model": self.model_name,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": content},
            ],
            "max_tokens": max_tokens,
            "temperature": 0.1,
        }
        request = urllib.request.Request(
            f"{self.base_url}/v1/chat/completions",
            data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        with urllib.request.urlopen(request, timeout=self.timeout) as response:
            reply = json.loads(response.read())
        return reply["choices"][0]["message"]["content"]

    def propose(self, request: ProposeRequest) -> ProposeResponse:
        if not request.screenshot_b64:
            raise ValueError("this provider needs a screenshot")
        width, height = png_size(request.screenshot_b64)
        text = self._complete(
            self.PROPOSE_SYSTEM,
            [
                {"type": "image_url", "image_url": {"url": f"data:image/png;base64,{request.screenshot_b64}"}},
                {"type": "text", "text": self.build_propose_prompt(request)},
            ],
            max_tokens=256,
        )
        log.debug("model raw output: %s", text)
        response = self.normalize_response(self.parse_json_content(text))
        if uses_normalized_coords(self.model_name):
            response = ProposeResponse(
                confidence=response.confidence,
                action=rescale_click(response.action, width, height),
            )
        return response

    def extract(self, schema: dict, content: str, purpose: Optional[str]) -> dict:
        user_text = (
            f"extract structured value from page content.\n"
            f"purpose: {purpose or ''}\n"
            f"schema: {json.dumps(schema)}\n"
            f"content:\n{content}"
        )
        raw = self.parse_json_content(
            self._complete(self.EXTRACT_SYSTEM, [{"type": "text", "text": user_text}], max_tokens=512)
        )
        return raw["value"] if "value" in raw else raw
