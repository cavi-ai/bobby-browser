"""HTTP client for the Bobby Browser ``/v1`` runtime interface.

Mirrors ``packages/typescript-sdk/src/client.ts``: every request sends
``Authorization``, ``x-interface-version``, ``x-correlation-id``, and
``x-deadline``; mutating calls accept an idempotency key; failures raise
:class:`~bobby_browser.errors.RuntimeClientError`. Stdlib only
(``urllib``, ``json``, ``dataclasses``, ``typing``) -- no third-party HTTP
client.
"""

from __future__ import annotations

import hashlib
import json
import re
import urllib.error
import urllib.parse
import urllib.request
import uuid
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from typing import Any, Dict, List, Mapping, Optional, Sequence

from .errors import RuntimeClientError

# Interface version negotiated via the `x-interface-version` request header.
# Keep aligned with packages/typescript-sdk/src/contracts.ts INTERFACE_VERSION.
INTERFACE_VERSION = "2026-08-19"

_DEFAULT_TIMEOUT_MS = 30_000
_JSON_CONTENT_TYPE = re.compile(r"^application/json(?:\s*;|$)", re.IGNORECASE)

# CommandOutcome.status -> expected HTTP status, mirroring client.ts's
# commandStatus().
_COMMAND_STATUS_HTTP: Dict[str, int] = {
    "completed": 200,
    "restarted": 200,
    "retryableFailure": 503,
    "needsReconciliation": 409,
    "policyDenied": 403,
    "resourceExhausted": 429,
}

# RecoveryDecision.status -> expected HTTP status, mirroring client.ts's
# `recover()` mapping.
_RECOVERY_NEEDS_RECONCILIATION_STATUS = "needsReconciliation"


@dataclass
class RequestOptions:
    """Per-call overrides. All fields are optional.

    Attributes:
        timeout_ms: Relative timeout in milliseconds (default 30_000).
        deadline: Absolute RFC3339 deadline string; combined with
            ``timeout_ms`` as the earlier of the two, same as the TS client.
        correlation_id: Value for ``x-correlation-id`` (a UUID4 is
            generated when omitted).
        idempotency_key: Value for ``idempotency-key`` on mutating POSTs.
    """

    timeout_ms: Optional[int] = None
    deadline: Optional[str] = None
    correlation_id: Optional[str] = None
    idempotency_key: Optional[str] = None


def _uuid4() -> str:
    return str(uuid.uuid4())


def _deadline_header(options: Optional[RequestOptions], default_timeout_ms: int) -> str:
    if options is not None and options.deadline:
        return options.deadline
    timeout_ms = default_timeout_ms
    if options is not None and options.timeout_ms is not None:
        timeout_ms = options.timeout_ms
    deadline = datetime.now(timezone.utc) + timedelta(milliseconds=timeout_ms)
    return deadline.strftime("%Y-%m-%dT%H:%M:%S.") + f"{deadline.microsecond // 1000:03d}Z"


def _header_get(headers: Mapping[str, str], name: str) -> Optional[str]:
    lowered = name.lower()
    for key, value in headers.items():
        if key.lower() == lowered:
            return value
    return None


def _content_type(headers: Mapping[str, str]) -> str:
    raw = _header_get(headers, "content-type") or ""
    return raw.split(";", 1)[0].strip().lower()


def _media_type_essence(value: str) -> Optional[str]:
    essence = value.split(";", 1)[0].strip().lower()
    if essence and re.match(r"^[!#$%&'*+.^_`|~0-9a-z-]+/[!#$%&'*+.^_`|~0-9a-z-]+$", essence):
        return essence
    return None


class BrowserRuntimeClient:
    """Authenticated HTTP client for a Bobby Browser runtime (``bobby serve``).

    Args:
        base_url: Runtime origin. A trailing slash and a trailing ``/v1``
            are stripped, so either ``http://127.0.0.1:7777`` or
            ``http://127.0.0.1:7777/v1`` works.
        bearer_token: Bearer credential for ``Authorization``. Never sent
            anywhere but that header, never logged, never put in a URL.
        timeout_ms: Default relative timeout for calls that do not pass
            ``options`` (default 30_000).
        opener: Override ``urllib.request`` opener (tests only).
    """

    def __init__(
        self,
        base_url: str,
        bearer_token: str,
        *,
        timeout_ms: int = _DEFAULT_TIMEOUT_MS,
        opener: Optional[urllib.request.OpenerDirector] = None,
    ) -> None:
        if not base_url or not bearer_token:
            raise ValueError("base_url and bearer_token are required")
        stripped = base_url.rstrip("/")
        if stripped.endswith("/v1"):
            stripped = stripped[: -len("/v1")]
        self._base_url = stripped
        self._bearer_token = bearer_token
        self._timeout_ms = timeout_ms
        self._opener = opener or urllib.request.build_opener()

    def __repr__(self) -> str:  # never print the bearer token
        return "BrowserRuntimeClient(bearer_token=[redacted])"

    # ---- sessions ---------------------------------------------------

    def runtime_info(self, options: Optional[RequestOptions] = None) -> Dict[str, Any]:
        """``GET /v1/runtime`` -- version, capabilities, and load counters."""
        return self._json("GET", "/v1/runtime", None, options, expected_status=200)

    def create_session(
        self, input: Mapping[str, Any], options: Optional[RequestOptions] = None
    ) -> Dict[str, Any]:
        """``POST /v1/sessions`` -- create a browser session."""
        return self._json("POST", "/v1/sessions", input, options, expected_status=200)

    def read_session(
        self, session_id: Optional[str] = None, options: Optional[RequestOptions] = None
    ) -> Any:
        """Read session state.

        There is no ``GET /v1/sessions/{id}`` on the wire, only
        ``GET /v1/sessions`` (full array, no pagination). With
        ``session_id`` omitted this returns that full list; with it set,
        this filters client-side and returns the one matching
        :class:`SessionState`, raising :class:`RuntimeClientError` with
        ``kind="protocol"`` if no session with that id is active.
        """
        sessions = self._json("GET", "/v1/sessions", None, options, expected_status=200)
        if not isinstance(sessions, list):
            raise self._protocol("sessions response has an unexpected shape")
        if session_id is None:
            return sessions
        for session in sessions:
            if isinstance(session, dict) and session.get("id") == session_id:
                return session
        raise self._protocol(f"no active session with id {session_id!r}", 200)

    def delete_session(self, session_id: str, options: Optional[RequestOptions] = None) -> None:
        """``DELETE /v1/sessions/{id}`` -- tear down a session (204 on success)."""
        self._empty("DELETE", f"/v1/sessions/{urllib.parse.quote(session_id)}", options)

    # ---- pages --------------------------------------------------------

    def open_page(
        self, input: Mapping[str, Any], options: Optional[RequestOptions] = None
    ) -> Dict[str, Any]:
        """``POST /v1/pages`` -- open a page in a session."""
        return self._json("POST", "/v1/pages", input, options, expected_status=200)

    def read_page(
        self,
        session_id: str,
        page_id: str,
        *,
        max_controls: Optional[int] = None,
        options: Optional[RequestOptions] = None,
    ) -> Dict[str, Any]:
        """``GET /v1/sessions/{session}/pages/{page}/forms`` -- read-only
        ``FormSnapshot`` (the PageRead HTTP surface; same contract as MCP
        ``form_snapshot``). ``max_controls`` is optional, 1 through 512.
        """
        if max_controls is not None and not (1 <= max_controls <= 512):
            raise self._protocol("max_controls must be between 1 and 512")
        query = f"?maxControls={max_controls}" if max_controls is not None else ""
        path = (
            f"/v1/sessions/{urllib.parse.quote(session_id)}"
            f"/pages/{urllib.parse.quote(page_id)}/forms{query}"
        )
        return self._json("GET", path, None, options, expected_status=200)

    # ---- commands -------------------------------------------------------

    def submit_command(
        self, envelope: Mapping[str, Any], options: Optional[RequestOptions] = None
    ) -> Dict[str, Any]:
        """``POST /v1/commands`` -- submit a raw ``CommandEnvelope``.

        Returns the ``CommandOutcome`` body unmodified (its ``status``
        discriminator field is preserved exactly as the server sent it --
        ``completed``, ``retryableFailure``, ``needsReconciliation``,
        ``policyDenied``, ``resourceExhausted``, ``restarted``, or
        ``failed``) after checking the HTTP status matches the documented
        mapping for that status.
        """
        status, payload = self._request("POST", "/v1/commands", envelope, options)
        if not isinstance(payload, dict) or "status" not in payload:
            raise self._response_error(status, payload)
        outcome_status = payload["status"]
        expected = _command_outcome_http_status(payload)
        if expected is None:
            raise self._protocol(f"unknown command outcome status: {outcome_status!r}", status)
        if expected != status:
            raise self._protocol(
                "command outcome status does not match HTTP mapping", status
            )
        return payload

    # ---- checkpoints / recovery ------------------------------------------

    def create_checkpoint(
        self, input: Mapping[str, Any], options: Optional[RequestOptions] = None
    ) -> Dict[str, Any]:
        """``POST /v1/checkpoints`` -- persist a workflow checkpoint."""
        return self._json("POST", "/v1/checkpoints", input, options, expected_status=200)

    def recovery_status(
        self, workflow_id: str, options: Optional[RequestOptions] = None
    ) -> Dict[str, Any]:
        """``GET /v1/recovery/{workflowId}`` -- current recovery status."""
        path = f"/v1/recovery/{urllib.parse.quote(workflow_id)}"
        return self._json("GET", path, None, options, expected_status=200)

    def recover_workflow(
        self, workflow_id: str, options: Optional[RequestOptions] = None
    ) -> Dict[str, Any]:
        """``POST /v1/recovery/{workflowId}`` -- resume, reconcile, or
        restart a workflow. ``needsReconciliation`` maps to HTTP 409;
        every other decision maps to 200.
        """
        path = f"/v1/recovery/{urllib.parse.quote(workflow_id)}"
        status, payload = self._request("POST", path, None, options)
        if not isinstance(payload, dict) or "status" not in payload:
            raise self._response_error(status, payload)
        expected = 409 if payload["status"] == _RECOVERY_NEEDS_RECONCILIATION_STATUS else 200
        if expected != status:
            raise self._protocol(
                "recovery decision status does not match HTTP mapping", status
            )
        return payload

    # ---- context --------------------------------------------------------

    def context_ask(
        self,
        session_id: str,
        page_id: str,
        description: str,
        options: Optional[RequestOptions] = None,
    ) -> Dict[str, Any]:
        """``GET /v1/context/ask`` -- remembered target for a description."""
        encoded = len(description.encode("utf-8"))
        if not (1 <= encoded <= 256):
            raise self._protocol("description must contain between 1 and 256 bytes")
        query = urllib.parse.urlencode(
            {"sessionId": session_id, "pageId": page_id, "description": description}
        )
        return self._json("GET", f"/v1/context/ask?{query}", None, options, expected_status=200)

    def context_site(
        self, site_key: str, options: Optional[RequestOptions] = None
    ) -> Dict[str, Any]:
        """``GET /v1/context/site/{key}`` -- durable per-site context view."""
        if not site_key:
            raise self._protocol("site key must not be empty")
        path = f"/v1/context/site/{urllib.parse.quote(site_key)}"
        return self._json("GET", path, None, options, expected_status=200)

    # ---- artifacts --------------------------------------------------------

    def read_artifact(
        self, reference: Mapping[str, Any], options: Optional[RequestOptions] = None
    ) -> bytes:
        """``GET /v1/artifacts/{artifactId}`` -- verified artifact bytes.

        ``reference`` is an ``ArtifactReference``: ``artifactId``,
        ``sha256``, ``bytes``, and ``mediaType``. Checks ``Content-Type``
        and ``Content-Length`` against the reference, then verifies
        SHA-256 before returning anything -- no bytes are handed back on a
        mismatch.
        """
        artifact_id = reference.get("artifactId")
        if not artifact_id or not isinstance(artifact_id, str):
            raise self._protocol("artifact reference is missing artifactId")
        path = f"/v1/artifacts/{urllib.parse.quote(artifact_id)}"
        status, content_type, headers, raw = self._request_raw("GET", path, None, options)
        if status != 200:
            raise self._response_error(status, self._decode_json_or_none(raw, content_type))
        expected_media_type = _media_type_essence(str(reference.get("mediaType", "")))
        if expected_media_type is None or content_type != expected_media_type:
            raise self._protocol("artifact media type does not match its reference", status)
        expected_bytes = reference.get("bytes")
        content_length = _header_get(headers, "content-length")
        if (
            content_length is None
            or not content_length.isdigit()
            or int(content_length) != expected_bytes
            or len(raw) != expected_bytes
        ):
            raise self._protocol("artifact content length does not match its reference", status)
        digest = hashlib.sha256(raw).hexdigest()
        expected_sha256 = str(reference.get("sha256", "")).lower()
        if digest != expected_sha256:
            raise self._protocol("artifact digest does not match its reference", status)
        return raw

    # ---- jobs --------------------------------------------------------

    def submit_job(
        self, input: Mapping[str, Any], options: Optional[RequestOptions] = None
    ) -> Dict[str, Any]:
        """``POST /v1/jobs`` -- submit a bounded runtime job."""
        return self._json("POST", "/v1/jobs", input, options, expected_status=201)

    def job_status(self, job_id: str, options: Optional[RequestOptions] = None) -> Dict[str, Any]:
        """``GET /v1/jobs/{jobId}`` -- read the authenticated principal's job."""
        path = f"/v1/jobs/{urllib.parse.quote(job_id)}"
        return self._json("GET", path, None, options, expected_status=200)

    def cancel_job(self, job_id: str, options: Optional[RequestOptions] = None) -> None:
        """``DELETE /v1/jobs/{jobId}`` -- cancel the authenticated principal's job."""
        self._empty("DELETE", f"/v1/jobs/{urllib.parse.quote(job_id)}", options)

    # ---- transport --------------------------------------------------------

    def _headers(self, options: Optional[RequestOptions], has_body: bool) -> Dict[str, str]:
        correlation_id = (options.correlation_id if options else None) or _uuid4()
        headers = {
            "Authorization": f"Bearer {self._bearer_token}",
            "x-interface-version": INTERFACE_VERSION,
            "x-correlation-id": correlation_id,
            "x-deadline": _deadline_header(options, self._timeout_ms),
        }
        if options is not None and options.idempotency_key:
            headers["idempotency-key"] = options.idempotency_key
        if has_body:
            headers["content-type"] = "application/json"
        return headers

    def _request_raw(
        self,
        method: str,
        path: str,
        body: Optional[Mapping[str, Any]],
        options: Optional[RequestOptions],
    ) -> tuple:
        """Returns (status, content_type, headers, raw_bytes)."""
        url = f"{self._base_url}{path}"
        headers = self._headers(options, body is not None)
        data = json.dumps(body).encode("utf-8") if body is not None else None
        request = urllib.request.Request(url, data=data, headers=headers, method=method)
        timeout_ms = self._timeout_ms
        if options is not None and options.timeout_ms is not None:
            timeout_ms = options.timeout_ms
        try:
            response = self._opener.open(request, timeout=timeout_ms / 1000)
            try:
                status = response.status
                response_headers = dict(response.headers.items())
                raw = response.read()
            finally:
                response.close()
        except urllib.error.HTTPError as error:
            status = error.code
            response_headers = dict(error.headers.items()) if error.headers else {}
            raw = error.read()
            error.close()
        except TimeoutError as error:
            raise RuntimeClientError("deadline", message="Request deadline exceeded") from error
        except urllib.error.URLError as error:
            raise RuntimeClientError(
                "transport", message=f"Runtime transport request failed: {error.reason}"
            ) from error
        content_type = _content_type(response_headers)
        return status, content_type, response_headers, raw

    def _decode_json_or_none(self, raw: bytes, content_type: str) -> Any:
        if not _JSON_CONTENT_TYPE.match(content_type):
            return None
        try:
            return json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            return None

    def _request(
        self,
        method: str,
        path: str,
        body: Optional[Mapping[str, Any]],
        options: Optional[RequestOptions],
    ) -> tuple:
        """Returns (status, parsed_json_payload)."""
        status, content_type, _headers, raw = self._request_raw(method, path, body, options)
        if not _JSON_CONTENT_TYPE.match(content_type):
            raise self._protocol("response content type must be application/json", status)
        try:
            payload = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise self._protocol("response body is not valid JSON", status) from error
        return status, payload

    def _json(
        self,
        method: str,
        path: str,
        body: Optional[Mapping[str, Any]],
        options: Optional[RequestOptions],
        *,
        expected_status: int,
    ) -> Any:
        status, payload = self._request(method, path, body, options)
        if status != expected_status:
            raise self._response_error(status, payload)
        return payload

    def _empty(self, method: str, path: str, options: Optional[RequestOptions]) -> None:
        status, content_type, _headers, raw = self._request_raw(method, path, None, options)
        if status == 204:
            return
        payload = self._decode_json_or_none(raw, content_type)
        raise self._response_error(status, payload)

    def _response_error(self, status: int, payload: Any) -> RuntimeClientError:
        if isinstance(payload, dict) and set(payload.keys()) == {"error"} and isinstance(
            payload["error"], dict
        ):
            return RuntimeClientError.from_interface_error("http", status, payload["error"])
        return self._protocol("response has an unexpected status or shape", status)

    def _protocol(self, message: str, status: Optional[int] = None) -> RuntimeClientError:
        return RuntimeClientError("protocol", status=status, message=message)


def _command_outcome_http_status(outcome: Mapping[str, Any]) -> Optional[int]:
    """Mirrors client.ts's commandStatus(): CommandOutcome.status -> HTTP status."""
    status = outcome.get("status")
    if status in _COMMAND_STATUS_HTTP:
        return _COMMAND_STATUS_HTTP[status]
    if status == "failed":
        error = outcome.get("error") or {}
        return 422 if error.get("code") == "invalidRequest" else 500
    return None
