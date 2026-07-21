mod native_host;
mod registry;
mod server;
mod session;

pub use native_host::{
    encode_native_message, read_native_message, run_native_host, validate_extension_message,
    validate_server_message, write_native_message, NativeConnectRequest, NativeHostConfig,
    NativeHostError, NativeReconnectBackoff, MAX_NATIVE_MESSAGE_BYTES,
};
pub use registry::{
    AttachmentLease, CompanionCredential, CompanionRegistry, PairedCompanion, PairedSession,
    PairingInput, RegistryError,
};
pub use server::{
    CompanionServer, CompanionServerConfig, CompanionServerError, CompanionServerHandle,
};
pub use session::CompanionSessionError;
