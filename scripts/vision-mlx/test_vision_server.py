import base64
import http.server
import json
import os
import pathlib
import socket
import struct
import subprocess
import sys
import threading
import time
import unittest
import urllib.error
import urllib.request
import zlib


SERVER = pathlib.Path(__file__).parent / "vision_server.py"


def start_server(bind: str, provider: str = "v1", model: str | None = None, env: dict | None = None) -> subprocess.Popen:
    command = [sys.executable, str(SERVER), "--provider", provider, "--bind", bind, "--exit-with-stdin"]
    if model:
        command += ["--model", model]
    return subprocess.Popen(
        command,
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        cwd=SERVER.parent,
        env={**os.environ, **(env or {})},
    )


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def white_png(width: int, height: int) -> bytes:
    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    rows = b"".join(b"\x00" + b"\xff" * width * 3 for _ in range(height))
    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")


class ModelServer(http.server.ThreadingHTTPServer):
    """An OpenAI-compatible chat server answering every request with `answer`."""

    def __init__(self, answer: str):
        self.answer = answer
        self.requests: list[dict] = []
        super().__init__(("127.0.0.1", 0), ModelServerHandler)


class ModelServerHandler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.requests.append({"path": self.path, "body": body})
        reply = json.dumps({"choices": [{"message": {"role": "assistant", "content": self.server.answer}}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(reply)))
        self.end_headers()
        self.wfile.write(reply)

    def log_message(self, *args):
        pass


def stop_server(server: subprocess.Popen) -> None:
    if server.poll() is None:
        server.kill()
    server.wait()


def post_status(port: int, timeout: float) -> int | None:
    """HTTP status the server at `port` answers an unknown path with, or None."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        request = urllib.request.Request(f"http://127.0.0.1:{port}/unknown", data=b"{}", method="POST")
        try:
            with urllib.request.urlopen(request, timeout=1) as response:
                return response.status
        except urllib.error.HTTPError as error:
            return error.code
        except OSError:
            time.sleep(0.2)
    return None


class VisionServerLifelineTests(unittest.TestCase):
    def test_a_managed_server_exits_when_its_stdin_closes(self):
        server = start_server("127.0.0.1:0")
        try:
            with self.assertRaises(subprocess.TimeoutExpired):
                server.wait(timeout=1)
            server.stdin.close()
            self.assertEqual(server.wait(timeout=10), 0)
        finally:
            stop_server(server)

    def test_a_server_serves_once_the_previous_holder_releases_its_address(self):
        holder = socket.socket()
        holder.bind(("127.0.0.1", 0))
        holder.listen()
        port = holder.getsockname()[1]
        server = start_server(f"127.0.0.1:{port}")
        try:
            time.sleep(1.0)
            holder.close()
            self.assertEqual(post_status(port, timeout=15), 404)
        finally:
            holder.close()
            stop_server(server)


def propose(port: int, body: dict, timeout: float) -> dict:
    """The server's answer to `body`, retried while the server starts."""
    deadline = time.monotonic() + timeout
    while True:
        request = urllib.request.Request(
            f"http://127.0.0.1:{port}/propose",
            data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                return json.loads(response.read())
        except OSError:
            if time.monotonic() > deadline:
                raise
            time.sleep(0.2)


class ModelServerProviderTests(unittest.TestCase):
    def test_a_served_model_answers_with_bobbys_prompt_and_its_click_in_frame_pixels(self):
        model_server = ModelServer('{"confidence": 0.9, "action": {"kind": "click", "x": 500, "y": 250}}')
        threading.Thread(target=model_server.serve_forever, daemon=True).start()
        port = free_port()
        model = "cavi-ai/Qwen3-VL-8B-Instruct-MLX-4bit"
        server = start_server(
            f"127.0.0.1:{port}",
            provider="mlx-server",
            model=model,
            env={"VISION_MLX_SERVER_URL": f"http://127.0.0.1:{model_server.server_address[1]}"},
        )
        frame = base64.b64encode(white_png(200, 100)).decode()
        try:
            answer = propose(
                port,
                {"purpose": "Continue", "intentKind": "locate", "stuck": "targetMissing", "screenshotPng": frame},
                timeout=15,
            )
        finally:
            stop_server(server)
            model_server.shutdown()

        # Qwen3-VL answers in [0, 1000) of each axis; the server returns frame pixels.
        self.assertEqual(answer, {"confidence": 0.9, "action": {"kind": "click", "x": 100.0, "y": 25.0}})
        [sent] = model_server.requests
        self.assertEqual(sent["path"], "/v1/chat/completions")
        self.assertEqual(sent["body"]["model"], model)
        system, user = sent["body"]["messages"]
        self.assertEqual(system["role"], "system")
        self.assertIn("browser automation", system["content"])
        image, text = user["content"]
        self.assertEqual(image, {"type": "image_url", "image_url": {"url": f"data:image/png;base64,{frame}"}})
        self.assertIn("purpose: Continue", text["text"])


if __name__ == "__main__":
    unittest.main()
