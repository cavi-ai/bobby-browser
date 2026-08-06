#!/usr/bin/env python3
"""
Local MLX Vision Inference Service for Bobby Browser.

A lightweight HTTP server that runs vision-language models on Apple Silicon
using MLX for the text decoder and PyTorch/transformers for the vision encoder.

Usage:
    python vision_mlx.py [--bind 127.0.0.1:9101] [--model qwen2-vl:7b]

Environment variables:
    VISION_MLX_MODEL    Model to load (default: qwen2-vl:7b)
    VISION_MLX_BIND     Bind address (default: 127.0.0.1:9101)
    VISION_MLX_PRELOAD  Preload model at startup (default: false)
"""

import argparse
import base64
import io
import json
import logging
import sys
import time
from dataclasses import dataclass, field
from http.server import HTTPServer, BaseHTTPRequestHandler
from typing import Any, Optional
from urllib.parse import urlparse

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

@dataclass
class Config:
    bind: str = "127.0.0.1:9101"
    model_name: str = "qwen2-vl:7b"
    preload: bool = False
    max_image_size: int = 4096
    max_text_tokens: int = 512

    @classmethod
    def from_env(cls) -> "Config":
        return cls(
            bind=next(iter(os.environ.get("VISION_MLX_BIND", cls.bind).split(","))),
            model_name=os.environ.get("VISION_MLX_MODEL", cls.model_name),
            preload=os.environ.get("VISION_MLX_PRELOAD", "false").lower() == "true",
        )

# ---------------------------------------------------------------------------
# Vision Model Loader
# ---------------------------------------------------------------------------

class VisionModel:
    """Loads a vision-language model and runs inference."""

    def __init__(self, model_name: str):
        self.model_name = model_name
        self.model = None
        self.processor = None
        self._loaded = False

    def load(self) -> None:
        """Load the model and processor."""
        logging.info(f"Loading model: {self.model_name}")

        # Try MLX first (if vision support is available)
        try:
            self._load_mlx()
            return
        except Exception as e:
            logging.debug(f"MLX loading failed: {e}")

        # Fallback to transformers + PyTorch
        self._load_transformers()

    def _load_mlx(self) -> None:
        """Attempt to load with MLX (future-proof when vision support lands)."""
        import mlx.core as mx
        from mlx_lm import load

        logging.info("MLX available, attempting to load model...")

        # When MLX vision support is available, this will work:
        # self.model, self.processor = load(self.model_name)
        # For now, mark as not loaded
        raise RuntimeError("MLX vision support not yet available in this version")

    def _load_transformers(self) -> None:
        """Load with transformers (PyTorch) as fallback."""
        try:
            from transformers import AutoProcessor, Qwen2VLForConditionalGeneration
            import torch

            logging.info("Loading Qwen2-VL with transformers...")

            self.model = Qwen2VLForConditionalGeneration.from_pretrained(
                self.model_name,
                torch_dtype=torch.float16,
                device_map="mps" if hasattr(torch.backends, "mps") and torch.backends.mps.is_available() else "cpu",
            )
            self.processor = AutoProcessor.from_pretrained(self.model_name)

            if hasattr(torch.backends, "mps") and torch.backends.mps.is_available():
                logging.info("Using MPS (Metal Performance Shaders)")
            else:
                logging.warning("MPS not available, using CPU (slow)")

            self._loaded = True
            logging.info(f"Model loaded: {self.model_name}")

        except ImportError as e:
            raise RuntimeError(f"Transformers not installed: {e}")

    def propose(self, image_b64: str, purpose: str, intent_kind: str,
                stuck: str, context: Optional[dict] = None) -> dict:
        """Run vision inference and return a proposal."""
        if not self._loaded:
            self.load()

        # Decode image
        image = self._decode_image(image_b64)

        # Build prompt
        prompt = self._build_prompt(purpose, intent_kind, stuck, context)

        # Run inference
        result = self._infer(image, prompt)

        # Parse response
        return self._parse_response(result)

    def _decode_image(self, image_b64: str) -> Any:
        """Decode base64 PNG to PIL Image."""
        from PIL import Image

        png_data = base64.b64decode(image_b64)
        image = Image.open(io.BytesIO(png_data)).convert("RGB")

        # Resize if too large
        max_size = 1024
        if image.width > max_size or image.height > max_size:
            image.thumbnail((max_size, max_size))

        return image

    def _build_prompt(self, purpose: str, intent_kind: str,
                      stuck: str, context: Optional[dict]) -> str:
        """Build the system + user prompt."""
        system = (
            "You are a vision assistant for a browser automation agent. "
            "Analyze the screenshot and return ONLY valid JSON matching this schema: "
            '{"confidence": 0.0..1.0, "action": {"kind": "click" | "typeText" | "extractValue", ...}}. '
            "Click coordinates are CSS pixels relative to the screenshot image. "
            "Do not include markdown fences, comments, or any text outside the JSON object."
        )

        user_text = f"purpose: {purpose}\nintentKind: {intent_kind}\nstuck: {stuck}"

        if context:
            if context.get("url"):
                user_text += f"\nurl: {context['url']}"
            if context.get("candidates"):
                user_text += "\ncandidates:"
                for c in context["candidates"]:
                    user_text += f"\n- {c['role']} \"{c['name']}\""
                    if c.get("ordinal"):
                        user_text += f" (#{c['ordinal']})"
            if context.get("recent_command_kinds"):
                user_text += f"\nrecentCommands: {', '.join(context['recent_command_kinds'])}"

        return f"{system}\n\n{user_text}"

    def _infer(self, image: Any, prompt: str) -> str:
        """Run model inference."""
        from transformers import GenerationConfig

        # Prepare inputs
        inputs = self.processor(
            text=prompt,
            images=image,
            return_tensors="pt"
        )

        # Move to device
        inputs = {k: v.to(self.model.device) for k, v in inputs.items()}

        # Generate
        with torch.no_grad():
            outputs = self.model.generate(
                **inputs,
                max_new_tokens=256,
                temperature=0.1,
                do_sample=False,
            )

        # Decode
        response = self.processor.decode(outputs[0], skip_special_tokens=True)

        # Extract just the JSON part
        return self._extract_json(response)

    def _extract_json(self, text: str) -> str:
        """Extract JSON from model output."""
        # Try to find JSON in the response
        start = text.find("{")
        end = text.rfind("}") + 1

        if start >= 0 and end > start:
            return text[start:end]

        # Try markdown fences
        if "```json" in text:
            parts = text.split("```json")
            if len(parts) > 1:
                parts = parts[1].split("```")
                if len(parts) > 0:
                    return parts[0].strip()

        return text.strip()

    def _parse_response(self, response: str) -> dict:
        """Parse the model's JSON response."""
        try:
            result = json.loads(response)

            # Validate structure
            if "confidence" not in result or "action" not in result:
                raise ValueError("Missing confidence or action")

            # Ensure confidence is in [0, 1]
            result["confidence"] = max(0.0, min(1.0, float(result["confidence"])))

            return result

        except json.JSONDecodeError as e:
            raise ValueError(f"Model returned invalid JSON: {e}")

# ---------------------------------------------------------------------------
# HTTP Handler
# ---------------------------------------------------------------------------

class VisionHandler(BaseHTTPRequestHandler):
    """HTTP request handler for vision requests."""

    model: Optional[VisionModel] = None

    def do_POST(self):
        """Handle POST requests."""
        if self.path != "/propose":
            self._send_error(404, "Not found")
            return

        # Read body
        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length)

        try:
            request = json.loads(body)
        except json.JSONDecodeError as e:
            self._send_error(400, f"Invalid JSON: {e}")
            return

        # Validate required fields
        required = ["purpose", "intentKind", "stuck", "screenshotPng"]
        for field in required:
            if field not in request:
                self._send_error(400, f"Missing required field: {field}")
                return

        # Run inference
        try:
            if VisionModel is None or self.model is None:
                self.model = VisionModel(
                    os.environ.get("VISION_MLX_MODEL", "qwen2-vl:7b")
                )
                self.model.load()

            result = self.model.propose(
                image_b64=request["screenshotPng"],
                purpose=request["purpose"],
                intent_kind=request["intentKind"],
                stuck=request["stuck"],
                context=request.get("context"),
            )

            self._send_json(200, result)

        except Exception as e:
            logging.error(f"Inference error: {e}", exc_info=True)
            self._send_error(500, f"Inference failed: {e}")

    def _send_json(self, status: int, data: dict):
        """Send JSON response."""
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(data).encode())

    def _send_error(self, status: int, message: str):
        """Send error response."""
        self._send_json(status, {"error": message})

    def log_message(self, format, *args):
        """Log HTTP requests."""
        logging.debug(format % args)

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="Local MLX Vision Service")
    parser.add_argument("--bind", default=None, help="Bind address (default: 127.0.0.1:9101)")
    parser.add_argument("--model", default=None, help="Model name")
    parser.add_argument("--verbose", action="store_true", help="Verbose logging")
    args = parser.parse_args()

    # Apply CLI overrides
    config = Config.from_env()
    if args.bind:
        config.bind = args.bind
    if args.model:
        config.model_name = args.model

    # Setup logging
    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(asctime)s [%(levelname)s] %(message)s",
    )

    logging.info(f"Starting vision-mlx on {config.bind}")
    logging.info(f"Model: {config.model_name}")

    # Create handler with config
    VisionHandler.model = None  # Lazy load

    # Start server
    server = HTTPServer(
        config.bind.split(":") if ":" in config.bind else ("127.0.0.1", 9101),
        VisionHandler,
    )

    logging.info(f"Vision-mlx listening on http://{config.bind}")

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        logging.info("Shutting down")
        server.shutdown()


if __name__ == "__main__":
    import os
    main()
