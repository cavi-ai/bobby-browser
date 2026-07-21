mod registry;
mod server;

pub use registry::{
    AttachmentLease, CompanionRegistry, PairedCompanion, PairingInput, RegistryError,
};
pub use server::{
    CompanionServer, CompanionServerConfig, CompanionServerError, CompanionServerHandle,
};
