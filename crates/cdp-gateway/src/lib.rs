//! Chrome DevTools Protocol adapter over the shared runtime interface.
//!
//! Exposes CDP discovery (`/json/version`, `/json/list`) and a WebSocket
//! debugger transport that maps CDP methods onto the same
//! [`interface_core::RuntimeInterface`] HTTP and MCP already drive. Capability,
//! idempotency, evidence, and outcome semantics cannot drift from the other
//! surfaces.
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
