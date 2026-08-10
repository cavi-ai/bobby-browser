#!/usr/bin/env python3
"""Canonical vision server for Bobby Browser.

Serves the vision-proxy wire format over HTTP, backed by any registered
provider. Drop-in replacement for the per-backend servers.

Usage:
    python vision_server.py                      # mlx-vlm (default)
    VISION_PROVIDER=ollama python vision_server.py
    VISION_PROVIDER=lmstudio python vision_server.py
    python vision_server.py --provider mlx-vlm --model mlx-community/Qwen2.5-VL-7B-Instruct-4bit
"""

import argparse
import json
import logging
from http.server import HTTPServer, BaseHTTPRequestHandler

from providers import create_provider

log = logging.getLogger("vision-server")


class VisionHandler(BaseHTTPRequestHandler):
    provider = None

    def do_POST(self):
        if self.path == "/propose":
            self._handle_propose()
        elif self.path == "/extract":
            self._handle_extract()
        else:
            self._send_json(404, {"error": "not found"})

    def _handle_propose(self):
        try:
            request = self._read_json()
            from providers import ProposeRequest
            req = ProposeRequest.from_dict(request)
            result = self.provider.propose(req)
            self._send_json(200, result.to_dict())
        except Exception as e:
            log.exception("propose failed")
            self._send_json(500, {"error": str(e)})

    def _handle_extract(self):
        try:
            request = self._read_json()
            value = self.provider.extract(
                schema=request["schema"],
                content=request["content"],
                purpose=request.get("purpose"),
            )
            self._send_json(200, {"value": value})
        except Exception as e:
            log.exception("extract failed")
            self._send_json(500, {"error": str(e)})

    def _read_json(self) -> dict:
        length = int(self.headers.get("Content-Length", 0))
        return json.loads(self.rfile.read(length))

    def _send_json(self, status: int, data: dict):
        body = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        log.debug(fmt % args)


def main():
    parser = argparse.ArgumentParser(description="Bobby canonical vision server")
    parser.add_argument("--bind", default="127.0.0.1:9101", help="bind address")
    parser.add_argument("--provider", default=None, help="mlx-vlm | ollama | lmstudio")
    parser.add_argument("--model", default=None, help="model override")
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(asctime)s [%(levelname)s] %(message)s",
    )

    provider = create_provider(args.provider)
    log.info("provider: %s", provider.name)

    host, port = args.bind.split(":")
    VisionHandler.provider = provider
    server = HTTPServer((host, int(port)), VisionHandler)
    log.info("vision server listening on http://%s", args.bind)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        server.shutdown()


if __name__ == "__main__":
    main()
