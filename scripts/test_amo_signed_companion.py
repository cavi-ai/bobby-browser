import base64
import hashlib
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import zipfile

ROOT = Path(__file__).resolve().parent.parent
HELPER = ROOT / "scripts" / "amo_signed_companion.py"
GUID = "companion@example.test"
VERSION = "1.2.3"
ISSUER = "user:12345:67"
SECRET = "test-secret"


def xpi(signed):
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        archive.writestr("manifest.json", "{}")
        if signed:
            archive.writestr("META-INF/mozilla.rsa", b"signature")
    return buffer.getvalue()


def valid_jwt(header):
    if not header or not header.startswith("JWT "):
        return False
    token = header[len("JWT "):]
    try:
        head, body, signature = token.split(".")
    except ValueError:
        return False
    expected = hmac.new(SECRET.encode(), f"{head}.{body}".encode(), hashlib.sha256).digest()
    padded = signature + "=" * (-len(signature) % 4)
    if not hmac.compare_digest(base64.urlsafe_b64decode(padded), expected):
        return False
    claims = json.loads(base64.urlsafe_b64decode(body + "=" * (-len(body) % 4)))
    return claims.get("iss") == ISSUER and claims.get("exp", 0) > time.time()


class StubAmo:
    """Answers the version detail and signed download for one add-on version."""

    def __init__(self, statuses, payload):
        self.statuses = list(statuses)
        self.payload = payload
        self.version_requests = 0
        stub = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def reply(self, code, body, content_type="application/json"):
                self.send_response(code)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                if not valid_jwt(self.headers.get("Authorization")):
                    self.reply(401, b'{"detail":"Incorrect authentication credentials."}')
                    return
                if self.path == f"/api/v5/addons/addon/{GUID}/versions/v{VERSION}/":
                    stub.version_requests += 1
                    index = min(stub.version_requests, len(stub.statuses)) - 1
                    file = {"status": stub.statuses[index], "url": f"{stub.base}/downloads/companion.xpi"}
                    self.reply(200, json.dumps({"version": VERSION, "file": file}).encode())
                elif self.path == "/downloads/companion.xpi":
                    self.reply(200, stub.payload, "application/x-xpinstall")
                else:
                    self.reply(404, b'{"detail":"Not found."}')

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.base = f"http://127.0.0.1:{self.server.server_port}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class AmoSignedCompanionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.manifest = self.root / "manifest.json"
        self.manifest.write_text(json.dumps({
            "version": VERSION,
            "browser_specific_settings": {"gecko": {"id": GUID}},
        }))
        self.out = self.root / "signed.xpi"

    def serve(self, statuses, payload):
        stub = StubAmo(statuses, payload)
        self.addCleanup(stub.close)
        return stub

    def run_helper(self, stub, *args, secret=SECRET):
        env = dict(os.environ, AMO_JWT_ISSUER=ISSUER, AMO_JWT_SECRET=secret)
        return subprocess.run(
            [sys.executable, str(HELPER),
             "--manifest", str(self.manifest),
             "--out", str(self.out),
             "--api-url", f"{stub.base}/api/v5/",
             "--interval", "0.05",
             *args],
            env=env, capture_output=True, text=True, timeout=30,
        )

    def test_approved_version_writes_the_signed_file(self):
        payload = xpi(signed=True)
        stub = self.serve(["public"], payload)
        result = self.run_helper(stub)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.out.read_bytes(), payload)

    def test_pending_version_exits_pending_without_a_file(self):
        stub = self.serve(["unreviewed"], xpi(signed=True))
        result = self.run_helper(stub, "--wait", "0")
        self.assertEqual(result.returncode, 3, result.stderr)
        self.assertFalse(self.out.exists())

    def test_waits_until_the_version_is_approved(self):
        payload = xpi(signed=True)
        stub = self.serve(["unreviewed", "unreviewed", "public"], payload)
        result = self.run_helper(stub, "--wait", "10")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.out.read_bytes(), payload)
        self.assertEqual(stub.version_requests, 3)

    def test_rejected_version_fails(self):
        stub = self.serve(["disabled"], xpi(signed=True))
        result = self.run_helper(stub, "--wait", "10")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertFalse(self.out.exists())

    def test_download_without_a_mozilla_signature_fails(self):
        stub = self.serve(["public"], xpi(signed=False))
        result = self.run_helper(stub)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertFalse(self.out.exists())

    def test_wrong_secret_is_refused(self):
        stub = self.serve(["public"], xpi(signed=True))
        result = self.run_helper(stub, secret="wrong")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertFalse(self.out.exists())


if __name__ == "__main__":
    unittest.main()
