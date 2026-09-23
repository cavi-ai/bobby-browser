"""Unit tests for bobby_browser.client against a local fake HTTP server.

No real bobby-browser runtime involved -- see test_live_runtime.py for the
one test that starts the real thing. These cover: the auth header contract,
idempotency-key passthrough, the 429 retryAfterMs surface, the 409
needsReconciliation surface on submit_command, and that the CommandOutcome
status discriminator survives the round trip unmodified.
"""

from __future__ import annotations

import hashlib
import json
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from typing import Callable

# `python3 -m unittest discover -s packages/python-sdk/tests` (the CI /
# check-version-agreement.py-adjacent gate) runs straight from a checkout
# with no `pip install` step first. `packages/python-sdk` (this file's
# grandparent) is the source package root; put it on sys.path so
# `bobby_browser` resolves to the checkout's own source.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from bobby_browser import BrowserRuntimeClient, RequestOptions, RuntimeClientError  # noqa: E402


class _Handler(BaseHTTPRequestHandler):
    def log_message(self, format, *args):  # noqa: A002 - stdlib signature
        pass  # keep test output quiet

    def _dispatch(self) -> None:
        self.server.route_handler(self)  # type: ignore[attr-defined]

    def do_GET(self) -> None:  # noqa: N802 - stdlib naming
        self._dispatch()

    def do_POST(self) -> None:  # noqa: N802
        self._dispatch()

    def do_DELETE(self) -> None:  # noqa: N802
        self._dispatch()

    def read_body(self) -> bytes:
        length = int(self.headers.get("Content-Length", "0") or "0")
        return self.rfile.read(length) if length else b""

    def reply_json(self, status: int, payload) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class _FakeServer:
    def __init__(self, handler: Callable[[_Handler], None]) -> None:
        self.httpd = HTTPServer(("127.0.0.1", 0), _Handler)
        self.httpd.route_handler = handler  # type: ignore[attr-defined]
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.httpd.server_port}"

    def close(self) -> None:
        self.httpd.shutdown()
        self.httpd.server_close()
        self.thread.join(timeout=5)


class ClientUnitTests(unittest.TestCase):
    def _serve(self, handler: Callable[[_Handler], None]) -> _FakeServer:
        server = _FakeServer(handler)
        self.addCleanup(server.close)
        return server

    # ---- auth header contract ------------------------------------------

    def test_every_request_carries_the_documented_headers(self) -> None:
        seen = {}

        def handler(req: _Handler) -> None:
            req.read_body()
            seen["authorization"] = req.headers.get("Authorization")
            seen["interface_version"] = req.headers.get("x-interface-version")
            seen["correlation_id"] = req.headers.get("x-correlation-id")
            seen["deadline"] = req.headers.get("x-deadline")
            req.reply_json(200, {"version": "1.2.3", "capabilities": [], "active_sessions": 0, "queued_jobs": 0, "uptime_ms": 1})

        server = self._serve(handler)
        client = BrowserRuntimeClient(server.base_url, "top-secret-bearer")
        info = client.runtime_info()

        self.assertEqual(info["version"], "1.2.3")
        self.assertEqual(seen["authorization"], "Bearer top-secret-bearer")
        self.assertEqual(seen["interface_version"], "2026-08-19")
        self.assertTrue(seen["correlation_id"])
        self.assertTrue(seen["deadline"] and seen["deadline"].endswith("Z"))
        # Never a credential in the URL.
        self.assertNotIn("top-secret-bearer", server.base_url)

    def test_bearer_token_never_appears_in_repr(self) -> None:
        client = BrowserRuntimeClient("http://127.0.0.1:1", "super-secret")
        self.assertNotIn("super-secret", repr(client))

    # ---- idempotency-key passthrough -------------------------------------

    def test_idempotency_key_passthrough_on_mutating_post(self) -> None:
        seen = {}

        def handler(req: _Handler) -> None:
            body = json.loads(req.read_body() or b"{}")
            seen["idempotency_key"] = req.headers.get("idempotency-key")
            seen["profile"] = body.get("profile")
            req.reply_json(
                200,
                {
                    "id": "session-1",
                    "profile": body.get("profile"),
                    "proxy": None,
                    "page_ids": [],
                    "created_at": "2026-09-23T00:00:00.000Z",
                    "last_used_at": "2026-09-23T00:00:00.000Z",
                    "execution_policy": {
                        "javascriptEvaluation": False,
                        "visionAssist": False,
                        "fingerprint": False,
                        "humanize": False,
                    },
                },
            )

        server = self._serve(handler)
        client = BrowserRuntimeClient(server.base_url, "token")
        session = client.create_session(
            {"profile": "default", "proxy": None},
            RequestOptions(idempotency_key="create-session-1"),
        )

        self.assertEqual(session["id"], "session-1")
        self.assertEqual(seen["idempotency_key"], "create-session-1")
        self.assertEqual(seen["profile"], "default")

    def test_idempotency_key_absent_when_not_supplied(self) -> None:
        seen = {}

        def handler(req: _Handler) -> None:
            req.read_body()
            seen["idempotency_key"] = req.headers.get("idempotency-key")
            req.send_response(204)
            req.end_headers()

        server = self._serve(handler)
        client = BrowserRuntimeClient(server.base_url, "token")
        client.delete_session("11111111-1111-4111-8111-111111111111")

        self.assertIsNone(seen["idempotency_key"])

    # ---- 429 retryAfterMs ------------------------------------------------

    def test_429_surfaces_retry_after_ms_from_interface_error(self) -> None:
        def handler(req: _Handler) -> None:
            req.read_body()
            req.reply_json(
                429,
                {
                    "error": {
                        "code": "resourceExhausted",
                        "layer": "interface",
                        "message": "too many requests",
                        "correlationId": "corr-1",
                        "commandId": None,
                        "retryable": True,
                        "retryAfterMs": 1500,
                        "reconciliationRequired": False,
                        "requiredCapability": None,
                    }
                },
            )

        server = self._serve(handler)
        client = BrowserRuntimeClient(server.base_url, "token")

        with self.assertRaises(RuntimeClientError) as caught:
            client.runtime_info()

        error = caught.exception
        self.assertEqual(error.kind, "http")
        self.assertEqual(error.status, 429)
        self.assertEqual(error.code, "resourceExhausted")
        self.assertEqual(error.retry_after_ms, 1500)
        self.assertTrue(error.retryable)

    # ---- 409 needsReconciliation on submit_command ------------------------

    def test_submit_command_409_needs_reconciliation_is_returned_not_raised(self) -> None:
        def handler(req: _Handler) -> None:
            req.read_body()
            req.reply_json(
                409,
                {
                    "status": "needsReconciliation",
                    "commandId": "cmd-1",
                    "error": {
                        "code": "verificationFailed",
                        "message": "page state diverged",
                        "layer": "page",
                        "retryable": False,
                    },
                    "evidence": [],
                },
            )

        server = self._serve(handler)
        client = BrowserRuntimeClient(server.base_url, "token")
        outcome = client.submit_command({"schemaVersion": 1, "commandId": "cmd-1"})

        self.assertEqual(outcome["status"], "needsReconciliation")
        self.assertEqual(outcome["commandId"], "cmd-1")

    def test_submit_command_mismatched_http_mapping_raises_protocol_error(self) -> None:
        def handler(req: _Handler) -> None:
            req.read_body()
            # needsReconciliation must map to 409; sending 200 is a protocol violation.
            req.reply_json(200, {"status": "needsReconciliation", "commandId": "cmd-2", "error": {}, "evidence": []})

        server = self._serve(handler)
        client = BrowserRuntimeClient(server.base_url, "token")

        with self.assertRaises(RuntimeClientError) as caught:
            client.submit_command({"schemaVersion": 1, "commandId": "cmd-2"})

        self.assertEqual(caught.exception.kind, "protocol")

    # ---- status discriminator preserved -----------------------------------

    def test_submit_command_status_discriminator_preserved(self) -> None:
        for status, http_status, extra in (
            ("completed", 200, {"evidence": []}),
            ("policyDenied", 403, {"error": {"code": "policyDenied", "message": "x", "layer": "interface", "retryable": False}}),
            (
                "resourceExhausted",
                429,
                {
                    "error": {"code": "resourceExhausted", "message": "x", "layer": "interface", "retryable": True},
                    "retryAfterMs": 250,
                },
            ),
        ):
            with self.subTest(status=status):
                payload = {"status": status, "commandId": "cmd-x", **extra}

                def handler(req: _Handler, payload=payload, http_status=http_status) -> None:
                    req.read_body()
                    req.reply_json(http_status, payload)

                server = self._serve(handler)
                client = BrowserRuntimeClient(server.base_url, "token")
                outcome = client.submit_command({"schemaVersion": 1, "commandId": "cmd-x"})
                self.assertEqual(outcome["status"], status)

    # ---- read_session (no single-id GET on the wire) -----------------------

    def test_read_session_filters_the_list_endpoint_client_side(self) -> None:
        def handler(req: _Handler) -> None:
            req.read_body()
            req.reply_json(
                200,
                [
                    {"id": "s1", "profile": "default"},
                    {"id": "s2", "profile": "default"},
                ],
            )

        server = self._serve(handler)
        client = BrowserRuntimeClient(server.base_url, "token")

        self.assertEqual(client.read_session("s2")["id"], "s2")
        self.assertEqual(len(client.read_session()), 2)
        with self.assertRaises(RuntimeClientError):
            client.read_session("does-not-exist")

    # ---- artifact verification ---------------------------------------------

    def test_read_artifact_verifies_sha256_before_returning_bytes(self) -> None:
        data = b"artifact-bytes-for-testing"
        digest = hashlib.sha256(data).hexdigest()

        def handler(req: _Handler) -> None:
            req.read_body()
            req.send_response(200)
            req.send_header("Content-Type", "application/octet-stream")
            req.send_header("Content-Length", str(len(data)))
            req.end_headers()
            req.wfile.write(data)

        server = self._serve(handler)
        client = BrowserRuntimeClient(server.base_url, "token")
        reference = {
            "referenceId": "ref-1",
            "artifactId": "artifact-1",
            "sha256": digest,
            "bytes": len(data),
            "mediaType": "application/octet-stream",
        }

        self.assertEqual(client.read_artifact(reference), data)

    def test_read_artifact_rejects_a_digest_mismatch(self) -> None:
        data = b"tampered-or-wrong-bytes"

        def handler(req: _Handler) -> None:
            req.read_body()
            req.send_response(200)
            req.send_header("Content-Type", "application/octet-stream")
            req.send_header("Content-Length", str(len(data)))
            req.end_headers()
            req.wfile.write(data)

        server = self._serve(handler)
        client = BrowserRuntimeClient(server.base_url, "token")
        reference = {
            "referenceId": "ref-2",
            "artifactId": "artifact-2",
            "sha256": "0" * 64,
            "bytes": len(data),
            "mediaType": "application/octet-stream",
        }

        with self.assertRaises(RuntimeClientError) as caught:
            client.read_artifact(reference)
        self.assertEqual(caught.exception.kind, "protocol")


if __name__ == "__main__":
    unittest.main()
