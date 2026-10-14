"""Exercise model selection through the real CLI in an isolated terminal."""

import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class ModelApi(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/api/tags":
            body = {"models": [
                {"name": "chosen-vision:custom", "capabilities": ["completion", "vision"]},
                {"name": "text-only:8b", "capabilities": ["completion"]},
            ]}
        elif self.path == "/v1/models":
            body = {"data": [{"id": "chosen-vision:custom"}]}
        else:
            self.send_error(404)
            return
        data = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *_args):
        pass


@unittest.skipUnless(sys.platform != "win32", "requires a Unix pseudo-terminal")
class VisionModelSelection(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="bobby-model-selection-")
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name)
        self.config = self.path / "config.toml"
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), ModelApi)
        thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.endpoint = f"http://127.0.0.1:{self.server.server_port}"

    def choose_model(self, args):
        import fcntl
        import pty
        import struct
        import termios

        root = Path(__file__).resolve().parent.parent
        target = Path(os.environ.get("CARGO_TARGET_DIR", "target"))
        binary = root / target / "debug" / "bobby"
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))
        env = os.environ.copy()
        env.update({
            "TERM": "xterm-256color",
            "BOBBY_BROWSER_BOOTSTRAP_ENV": str(self.path / "bootstrap.env"),
            "BOBBY_VISION_TOKEN": "model-selection-test-token-0123456789",
        })
        child = subprocess.Popen([str(binary), *args], cwd=root, env=env,
                                 stdin=slave, stdout=slave, stderr=slave)
        os.close(slave)
        transcript = bytearray()
        entered = False
        deadline = time.monotonic() + 15
        try:
            while child.poll() is None and time.monotonic() < deadline:
                readable, _, _ = select.select([master], [], [], 0.1)
                if readable:
                    try:
                        data = os.read(master, 65536)
                    except OSError:
                        break
                    transcript.extend(data)
                    if not entered and b"chosen-vision:custom" in transcript:
                        os.write(master, b"chosen-vision:custom\r")
                        entered = True
            child.wait(timeout=3)
            self.assertEqual(child.returncode, 0, transcript.decode(errors="replace"))
        finally:
            if child.poll() is None:
                child.terminate()
                child.wait(timeout=3)
            os.close(master)
        self.assertNotIn(b"text-only:8b", transcript)
        saved = tomllib.loads(self.config.read_text())["vision"]["providers"]["ollama"]
        self.assertEqual(saved["model"], "chosen-vision:custom")
        self.assertEqual(saved["base_url"], self.endpoint)

    def test_vision_connect_saves_the_model_selected_from_the_provider_list(self):
        self.choose_model(["vision", "connect", "--provider", "ollama",
                           "--base-url", self.endpoint, "--config", str(self.config)])

    def test_install_saves_the_model_selected_from_the_provider_list(self):
        self.config.write_text(
            '[vision]\nprovider = "ollama"\n[vision.providers.ollama]\n'
            f'base_url = "{self.endpoint}"\nmodel = "previous-vision:old"\n'
        )
        self.choose_model(["install", "--vision", "--config", str(self.config),
                           "--training-data-dir", str(self.path / "training")])


if __name__ == "__main__":
    unittest.main()
