"""Client-side error type for the Bobby Browser Python SDK.

Mirrors ``RuntimeClientError`` from the TypeScript SDK
(``packages/typescript-sdk/src/errors.ts``): a single exception class
classified by ``kind``, carrying the fields a caller needs to decide whether
to retry, without ever retaining the bearer token.
"""

from __future__ import annotations

from typing import Any, Mapping, Optional

# Classification of RuntimeClientError, matching the TypeScript
# RuntimeClientErrorKind union exactly.
RUNTIME_CLIENT_ERROR_KINDS = ("transport", "protocol", "http", "aborted", "deadline")


class RuntimeClientError(Exception):
    """Raised for every failure the client cannot recover from itself.

    Attributes:
        kind: One of ``"transport"``, ``"protocol"``, ``"http"``,
            ``"aborted"``, ``"deadline"``.
        status: HTTP status code, when the failure followed a response.
        code: Wire ``InterfaceErrorCode``, when the server returned one.
        correlation_id: The request's ``x-correlation-id``, when known.
        command_id: The command id an interface error referenced, if any.
        retryable: Whether the server marked the failure retryable.
        retry_after_ms: Minimum backoff before retrying, when the server
            supplied one (notably on HTTP 429).
        reconciliation_required: Whether the caller must reconcile a
            workflow's checkpoint before retrying.
        required_capability: The capability the caller was missing, if any.
        event_gap: The ``EventGap`` payload, for a 409 on ``events()``.
    """

    def __init__(
        self,
        kind: str,
        *,
        status: Optional[int] = None,
        code: Optional[str] = None,
        message: Optional[str] = None,
        correlation_id: Optional[str] = None,
        command_id: Optional[Any] = None,
        retryable: Optional[bool] = None,
        retry_after_ms: Optional[int] = None,
        reconciliation_required: Optional[bool] = None,
        required_capability: Optional[Any] = None,
        event_gap: Optional[Mapping[str, Any]] = None,
    ) -> None:
        if kind not in RUNTIME_CLIENT_ERROR_KINDS:
            raise ValueError(f"unknown RuntimeClientError kind: {kind!r}")
        resolved_message = message or f"Runtime client {kind} failure"
        super().__init__(resolved_message)
        self.kind = kind
        self.status = status
        self.code = code
        self.correlation_id = correlation_id
        self.command_id = command_id
        self.retryable = retryable
        self.retry_after_ms = retry_after_ms
        self.reconciliation_required = reconciliation_required
        self.required_capability = required_capability
        self.event_gap = dict(event_gap) if event_gap is not None else None

    @classmethod
    def from_interface_error(
        cls, kind: str, status: int, error: Mapping[str, Any]
    ) -> "RuntimeClientError":
        """Build from a wire ``InterfaceError`` (``{"error": {...}}`` body)."""
        return cls(
            kind,
            status=status,
            code=error.get("code"),
            message=f"Runtime request failed: {status} {error.get('code')}",
            correlation_id=error.get("correlationId"),
            command_id=error.get("commandId"),
            retryable=error.get("retryable"),
            retry_after_ms=error.get("retryAfterMs"),
            reconciliation_required=error.get("reconciliationRequired"),
            required_capability=error.get("requiredCapability"),
        )

    def __repr__(self) -> str:  # pragma: no cover - cosmetic
        return f"RuntimeClientError(kind={self.kind!r}, status={self.status!r}, code={self.code!r})"

    def to_dict(self) -> dict:
        """JSON-safe projection for logging."""
        return {
            "kind": self.kind,
            "status": self.status,
            "code": self.code,
            "correlationId": self.correlation_id,
            "commandId": self.command_id,
            "retryable": self.retryable,
            "retryAfterMs": self.retry_after_ms,
            "reconciliationRequired": self.reconciliation_required,
            "requiredCapability": self.required_capability,
            "eventGap": self.event_gap,
        }
