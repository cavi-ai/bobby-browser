mod native_host;
mod registry;
mod server;

pub use native_host::{
    encode_native_message, read_native_message, run_native_host, validate_protocol_message,
    write_native_message, NativeConnectRequest, NativeHostConfig, NativeHostError,
    MAX_NATIVE_MESSAGE_BYTES,
};
pub use registry::{
    AttachmentLease, CompanionRegistry, PairedCompanion, PairingInput, RegistryError,
};
pub use server::{
    CompanionServer, CompanionServerConfig, CompanionServerError, CompanionServerHandle,
};
