"""``bobby-browser`` -- typed HTTP client for a Bobby Browser runtime
(``bobby serve``) speaking the authenticated ``/v1`` interface.

Pair with ``@cavi-ai/bobby-browser`` (TypeScript) or ``bobby-browser-client``
(Rust) for the same surface from other callers. Auth headers on every
request: ``Authorization: Bearer ...``, ``x-interface-version``,
``x-correlation-id``, and ``x-deadline``.
"""

from .client import INTERFACE_VERSION, BrowserRuntimeClient, RequestOptions
from .errors import RuntimeClientError

__all__ = [
    "BrowserRuntimeClient",
    "RequestOptions",
    "RuntimeClientError",
    "INTERFACE_VERSION",
]

__version__ = "0.16.0"
