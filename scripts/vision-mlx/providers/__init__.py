"""Provider registry — opt-in backend selection for Bobby vision assist.

Canonical usage:

    from providers import create_provider
    provider = create_provider()                     # env-driven default
    provider = create_provider("mlx-vlm")            # direct local MLX
    provider = create_provider("ollama")             # Ollama HTTP
    provider = create_provider("lmstudio")           # LM Studio HTTP

Selection precedence: explicit argument > VISION_PROVIDER env > config default.
Model and endpoint overrides via env:

    VISION_PROVIDER=mlx-vlm
    VISION_MLX_MODEL=mlx-community/Qwen2.5-VL-7B-Instruct-4bit
    VISION_OLLAMA_MODEL=llava:7b
    VISION_OLLAMA_BASE_URL=http://127.0.0.1:11434
    VISION_LMSTUDIO_MODEL=local-model
    VISION_LMSTUDIO_BASE_URL=http://127.0.0.1:1234
"""

from __future__ import annotations

import logging
import os

from .base import ProposeRequest, ProposeResponse, VisionProvider
from .lmstudio_provider import LmStudioProvider, DEFAULT_BASE_URL as LMSTUDIO_URL, DEFAULT_MODEL as LMSTUDIO_MODEL
from .mlx_vlm_provider import MlxVlmProvider, DEFAULT_MODEL as MLX_MODEL
from .ollama_provider import OllamaProvider, DEFAULT_BASE_URL as OLLAMA_URL, DEFAULT_MODEL as OLLAMA_MODEL

log = logging.getLogger(__name__)

DEFAULT_PROVIDER = "mlx-vlm"

_PROVIDERS = {
    "mlx-vlm": MlxVlmProvider,
    "ollama": OllamaProvider,
    "lmstudio": LmStudioProvider,
}


def create_provider(kind: str | None = None) -> VisionProvider:
    """Build a vision provider from explicit choice, env, or default."""
    kind = (kind or os.environ.get("VISION_PROVIDER") or DEFAULT_PROVIDER).strip()

    if kind == "mlx-vlm":
        return MlxVlmProvider(model_name=os.environ.get("VISION_MLX_MODEL", MLX_MODEL))
    if kind == "ollama":
        return OllamaProvider(
            model=os.environ.get("VISION_OLLAMA_MODEL", OLLAMA_MODEL),
            base_url=os.environ.get("VISION_OLLAMA_BASE_URL", OLLAMA_URL),
        )
    if kind == "lmstudio":
        return LmStudioProvider(
            model=os.environ.get("VISION_LMSTUDIO_MODEL", LMSTUDIO_MODEL),
            base_url=os.environ.get("VISION_LMSTUDIO_BASE_URL", LMSTUDIO_URL),
        )

    available = ", ".join(sorted(_PROVIDERS))
    raise ValueError(f"unknown vision provider {kind!r}; expected one of: {available}")


__all__ = [
    "create_provider",
    "VisionProvider",
    "ProposeRequest",
    "ProposeResponse",
    "MlxVlmProvider",
    "OllamaProvider",
    "LmStudioProvider",
]
