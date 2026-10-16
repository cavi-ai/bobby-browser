import copy
import json
from pathlib import Path
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bobby_browser import BrowserRuntimeClient, RuntimeClientError

FIXTURE = json.loads((Path(__file__).resolve().parents[3] / "tests/fixtures/session-capabilities.json").read_text())

class SessionCapabilitiesTests(unittest.TestCase):
    def test_endpoint_contract_and_invalid_responses(self):
        state = {"payload": copy.deepcopy(FIXTURE), "paths": []}
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass
            def do_GET(self):
                state["paths"].append(self.path)
                body = json.dumps(state["payload"]).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("x-interface-version", "2026-08-19")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            client = BrowserRuntimeClient(f"http://127.0.0.1:{server.server_port}", "test-token")
            self.assertEqual(client.session_capabilities(FIXTURE["sessionId"]), FIXTURE)
            self.assertEqual(state["paths"][0], f'/v1/sessions/{FIXTURE["sessionId"]}/capabilities')
            invalids = [dict(FIXTURE, schemaVersion=2), dict(FIXTURE, connection="fabricated"),
                dict(FIXTURE, features=[]), dict(FIXTURE, secret="unrecognized"),
                dict(FIXTURE, sessionId="00000000-0000-4000-8000-000000000006"),
                dict(FIXTURE, features=[FIXTURE["features"][0], FIXTURE["features"][0]])]
            for invalid in invalids:
                state["payload"] = invalid
                with self.assertRaises(RuntimeClientError):
                    client.session_capabilities(FIXTURE["sessionId"])
            before = len(state["paths"])
            with self.assertRaises(RuntimeClientError):
                client.session_capabilities("invalid/session")
            self.assertEqual(len(state["paths"]), before)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
