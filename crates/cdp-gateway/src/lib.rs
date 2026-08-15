//! Chrome DevTools Protocol adapter over the shared runtime interface.
//!
//! Exposes CDP discovery (`/json/version`, `/json/list`) and a WebSocket
//! debugger transport that maps CDP methods onto the same
//! [`interface_core::RuntimeInterface`] HTTP and MCP already drive. Capability,
//! idempotency, evidence, and outcome semantics cannot drift from the other
//! surfaces.
//!
//! # Scope: a pinned client shim, not a CDP backend
//!
//! This adapter recognizes a fixed set of call shapes. `Runtime.evaluate`
//! accepts only Playwright's own injected-script bootstraps, matched by length
//! and SHA-256 (see [`domains`]); `Runtime.callFunctionOn` accepts one pinned
//! Puppeteer declaration and a closed list of operations. There is no `DOM`
//! domain: an expression the pins do not cover is refused, never run.
//!
//! That is a gap in this adapter, not a rule of the runtime. The runtime does
//! evaluate JavaScript on request — `evaluate_javascript` over HTTP, MCP, and
//! the SDKs, gated by the `javascript:evaluate` capability plus the session's
//! `executionPolicy.javascriptEvaluation`. Reaching it from here needs both a
//! route to that evaluator and remote-object handle lifetimes, which this
//! adapter does not have; until then `page.evaluate`, `page.content`, and
//! `locator` queries that go through a client's own injected script are out of
//! scope, and adding a pin for a new client release is what extends coverage.
//! `docs/cdp-support.json` publishes the exact allowlist.
//!
//! Start from [`CdpGateway::new`] and mount [`CdpGateway::router`] on the host
//! HTTP server.

mod domains;
mod manifest;
mod mapping;
mod protocol;
mod server;

pub use manifest::{EventMetadata, MethodMetadata, MethodRegistry};
pub use mapping::{IdentifierFamily, IdentifierMap, RuntimeGeneration};
pub use protocol::{
    parse_frame, CdpError, CdpErrorCode, CdpEvent, CdpRequest, CdpResponse, MAX_FRAME_BYTES,
    MAX_IN_FLIGHT_REQUESTS, MAX_QUEUED_EVENTS,
};
pub use server::{
    CdpConnection, CdpGateway, DiscoveryError, TargetDescription, VersionDescription,
};
