"""V1 wire-contract provider (BOBBY-VISION/1).

Serves the fine-tuned LoRA adapter over the bare-index wire contract:
prompt carries the task + page + numbered candidates; the model replies
with a single integer — the candidate index, or -1 to abstain. The
provider maps the index onto the canonical clickCandidate action and the
runtime resolves the element (runtime owns spatial grounding).

Backend model: text-only (the prompt carries the candidate list as text);
the adapter was trained by scripts/vision-mlx/training/mlx_finetune.py
--schema v1. Images are unused on this path by design (see the V1 wire
contract: selection from candidates, not pixels).

Selection:
    VISION_PROVIDER=v1
    VISION_V1_MODEL=mlx-community/Qwen2.5-7B-Instruct-4bit
    VISION_V1_ADAPTER=models/mlx-lora-bobby-v1/adapters.safetensors
"""

from __future__ import annotations

import logging
import os
from typing import Optional

from .base import ProposeRequest, ProposeResponse, VisionProvider

log = logging.getLogger(__name__)

DEFAULT_MODEL = "mlx-community/Qwen2.5-7B-Instruct-4bit"

V1_PREFIX = """BOBBY-VISION/1
ROLE: element selector for a browser automation runtime
RULES: reply with ONLY the index of the element that satisfies the task. No text, no JSON, no explanation. If nothing fits, reply -1."""


class MlxV1Provider(VisionProvider):
    name = "v1"

    def __init__(self, model_name: str = DEFAULT_MODEL, adapter_path: Optional[str] = None):
        self.model_name = model_name
        self.adapter_path = adapter_path
        self._model = None
        self._tokenizer = None

    def _ensure_loaded(self):
        if self._model is not None:
            return
        from mlx_lm import load
        log.info("Loading %s (adapter: %s) ...", self.model_name, self.adapter_path)
        self._model, self._tokenizer = load(
            self.model_name,
            adapter_path=self.adapter_path,
        )
        log.info("Model loaded.")

    def propose(self, request: ProposeRequest) -> ProposeResponse:
        self._ensure_loaded()
        from mlx_lm import generate

        prompt = self._build_v1_prompt(request)
        from mlx_lm.sample_utils import make_sampler
        response = generate(
            self._model,
            self._tokenizer,
            prompt,
            max_tokens=8,
            sampler=make_sampler(temp=0.0),
            verbose=False,
        )
        text = response if isinstance(response, str) else getattr(response, "text", str(response))
        index = self._parse_index(text.strip())
        n_candidates = len(request.context.candidates) if request.context else 0

        if index is None or index < 0 or index >= max(n_candidates, 1):
            # Abstain (or out-of-range noise treated as abstention, per the
            # wire contract): zero confidence fails the runtime floor, which
            # is the fallback path.
            return ProposeResponse(
                confidence=0.0,
                action={"kind": "clickCandidate", "index": 0},
            )

        return ProposeResponse(
            confidence=0.95,
            action={"kind": "clickCandidate", "index": index},
        )

    def extract(self, schema: dict, content: str, purpose: Optional[str]) -> dict:
        # The V1 contract selects elements; structured extraction stays on
        # the JSON-schema providers (mlx-vlm / ollama / lmstudio).
        raise NotImplementedError("v1 provider does not serve extract")

    def _build_v1_prompt(self, request: ProposeRequest) -> str:
        block = f"TASK: {request.purpose}"
        ctx = request.context
        if ctx and ctx.url:
            block += f"\nPAGE: {ctx.url}"
        if ctx and ctx.candidates:
            rows = "\n".join(
                f"{i}|{c.role}|{c.name}" for i, c in enumerate(ctx.candidates)
            )
            block += f"\nELEMENTS:\n{rows}"
        prompt = f"{V1_PREFIX}\n\n{block}\n\n"
        # Chat-template the prompt for the instruct model.
        tokenizer = self._tokenizer
        if hasattr(tokenizer, "apply_chat_template"):
            return tokenizer.apply_chat_template(
                [{"role": "user", "content": prompt}],
                tokenize=False,
                add_generation_prompt=True,
            )
        return prompt

    @staticmethod
    def _parse_index(text: str) -> Optional[int]:
        """Parse a bare integer from the response, tolerating whitespace."""
        token = text.split()[0] if text.split() else ""
        token = token.strip().strip(".,")
        try:
            return int(token)
        except ValueError:
            return None
