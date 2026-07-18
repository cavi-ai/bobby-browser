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
