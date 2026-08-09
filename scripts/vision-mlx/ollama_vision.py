#!/usr/bin/env python3
"""
Ollama Vision Provider for Bobby Browser.

Normalizes Ollama model outputs to Bobby's VisionProposal schema.
Supports llava:7b and other Ollama vision models.

Usage:
    python ollama_vision.py [--bind 127.0.0.1:9103] [--model llava:7b]

Environment variables:
    VISION_OLLAMA_MODEL    Model to use (default: llava:7b)
    VISION_OLLAMA_BIND     Bind address (default: 127.0.0.1:9103)
    VISION_OLLAMA_BASE_URL Ollama API URL (default: http://127.0.0.1:11434)
"""

import argparse
import json
import logging
import math
import os
from dataclasses import dataclass
from http.server import HTTPServer, BaseHTTPRequestHandler
from typing import Optional

import requests

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

@dataclass
class Config:
    bind: str = "127.0.0.1:9103"
    model_name: str = "llava:7b"
    base_url: str = "http://127.0.0.1:11434"

    @classmethod
    def from_env(cls) -> "Config":
        return cls(
            bind=os.environ.get("VISION_OLLAMA_BIND", cls.bind),
            model_name=os.environ.get("VISION_OLLAMA_MODEL", cls.model_name),
            base_url=os.environ.get("VISION_OLLAMA_BASE_URL", cls.base_url),
        )

# ---------------------------------------------------------------------------
# Ollama Client
# ---------------------------------------------------------------------------

class OllamaClient:
    """Client for Ollama's vision API."""

    def __init__(self, model_name: str, base_url: str):
        self.model_name = model_name
        self.base_url = base_url
        self._session = requests.Session()

    def propose(self, image_b64: str, purpose: str, intent_kind: str,
                stuck: str, context: Optional[dict] = None) -> dict:
        """Send a vision request and return normalized Bobby proposal."""

        # Build prompt for Bobby's schema
        prompt = self._build_prompt(purpose, intent_kind, stuck, context)

        # Call Ollama API
        response = self._session.post(
            f"{self.base_url}/v1/chat/completions",
            json={
                "model": self.model_name,
                "messages": [
                    {
                        "role": "user",
                        "content": [
                            {"type": "text", "text": prompt},
                            {"type": "image_url", "image_url": {
                                "url": f"data:image/png;base64,{image_b64}"
                            }}
                        ]
                    }
                ],
                "max_tokens": 512,
                "temperature": 0,
                "response_format": {"type": "json_object"},
            },
            timeout=120
        )

        if response.status_code != 200:
            raise RuntimeError(f"Ollama returned {response.status_code}: {response.text[:200]}")

        data = response.json()
        content = data['choices'][0]['message']['content']

        # Extract and normalize JSON
        return self._normalize_response(content)

    def _build_prompt(self, purpose: str, intent_kind: str,
                      stuck: str, context: Optional[dict]) -> str:
        """Build prompt that forces Bobby's exact output schema."""
        system = (
            "You are a vision assistant for a browser automation agent called Bobby. "
            "You MUST return ONLY a valid JSON object with this exact structure - no markdown, "
            "no comments, no extra text:\n"
            '{"confidence": <number 0.0-1.0>, "action": {"kind": "click", "x": <number>, "y": <number>}}\n'
            'OR for text input:\n'
            '{"confidence": <number>, "action": {"kind": "typeText", "text": "<string>"}}\n'
            'OR for value extraction:\n'
            '{"confidence": <number>, "action": {"kind": "extractValue", "value": "<string>"}}\n\n'
            'For click actions, x and y are CSS pixel coordinates relative to the screenshot image origin (top-left).\n\n'
            'IMPORTANT: Return ONLY the JSON object. Do not wrap in code fences. Do not add any text before or after.'
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

    def _normalize_response(self, content: str) -> dict:
        """Normalize Ollama's output to Bobby's schema."""
        # Extract JSON from response
        json_str = self._extract_json(content)

        if not json_str:
            return self._rejected_proposal()

        try:
            result = json.loads(json_str)
        except json.JSONDecodeError:
            return self._rejected_proposal()
        if not isinstance(result, dict) or not isinstance(result.get("action"), dict):
            return self._rejected_proposal()

        try:
            confidence = float(result.get("confidence", 0.0))
        except (TypeError, ValueError):
            return self._rejected_proposal()
        if not math.isfinite(confidence):
            return self._rejected_proposal()
        confidence = max(0.0, min(1.0, confidence))

        action = result["action"]
        kind = action.get("kind")
        if kind is None:
            if "x" in action and "y" in action:
                kind = "click"
            elif "text" in action:
                kind = "typeText"
            elif "value" in action:
                kind = "extractValue"

        if kind == "click":
            try:
                x = float(action["x"])
                y = float(action["y"])
            except (KeyError, TypeError, ValueError):
                return self._rejected_proposal()
            if not math.isfinite(x) or not math.isfinite(y):
                return self._rejected_proposal()
            normalized_action = {"kind": "click", "x": x, "y": y}
        elif kind == "typeText" and isinstance(action.get("text"), str):
            normalized_action = {"kind": "typeText", "text": action["text"]}
        elif kind == "extractValue" and isinstance(action.get("value"), str):
            normalized_action = {"kind": "extractValue", "value": action["value"]}
        else:
            return self._rejected_proposal()

        return {"confidence": confidence, "action": normalized_action}

    @staticmethod
    def _rejected_proposal() -> dict:
        # Bobby rejects this below its confidence floor before executing it.
        return {"confidence": 0.0, "action": {"kind": "click", "x": 0.0, "y": 0.0}}

    def _extract_json(self, text: str) -> Optional[str]:
        """Extract JSON from model output."""
        # Try direct JSON
        start = text.find('{')
        end = text.rfind('}') + 1

        if start >= 0 and end > start:
            return text[start:end]

        return None

# ---------------------------------------------------------------------------
# HTTP Handler
# ---------------------------------------------------------------------------

class VisionHandler(BaseHTTPRequestHandler):
    """HTTP request handler for vision requests."""

    client: Optional[OllamaClient] = None

    def do_POST(self):
        """Handle POST requests."""
        if self.path != "/propose":
            self._send_error(404, "Not found")
            return

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
            if self.client is None:
                config = Config.from_env()
                self.client = OllamaClient(config.model_name, config.base_url)

            result = self.client.propose(
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
    parser = argparse.ArgumentParser(description="Ollama Vision Provider for Bobby")
    parser.add_argument("--bind", default=None, help="Bind address")
    parser.add_argument("--model", default=None, help="Ollama model name")
    parser.add_argument("--base-url", default=None, help="Ollama API URL")
    parser.add_argument("--verbose", action="store_true", help="Verbose logging")
    args = parser.parse_args()

    config = Config.from_env()
    if args.bind:
        config.bind = args.bind
    if args.model:
        config.model_name = args.model
    if args.base_url:
        config.base_url = args.base_url

    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(asctime)s [%(levelname)s] %(message)s",
    )

    logging.info(f"Starting ollama-vision on {config.bind}")
    logging.info(f"Model: {config.model_name}, Base URL: {config.base_url}")

    handler = VisionHandler
    handler.client = None

    bind_parts = config.bind.split(":")
    host = bind_parts[0] if len(bind_parts) > 0 else "127.0.0.1"
    port = int(bind_parts[1]) if len(bind_parts) > 1 else 9103

    server = HTTPServer((host, port), handler)

    logging.info(f"Ollama vision listening on http://{config.bind}")

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        logging.info("Shutting down")
        server.shutdown()


if __name__ == "__main__":
    main()
