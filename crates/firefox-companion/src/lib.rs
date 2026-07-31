//! Firefox companion adapter.

pub mod bidi;
mod worker;

pub use bidi::{BidiClient, BidiEvent, BidiTransport, SharedBiDiTransport};
pub use worker::{
    CompanionExtensionObserver, ExtensionControl, ExtensionObservation, ExtensionObserver,
    ExtensionPageBinding, FirefoxCompanionFactory, FirefoxCompanionWorker, MAX_TRACKED_PAGES,
};

pub fn required_extension_capabilities() -> worker_pool::RequiredCapabilities {
    worker_pool::RequiredCapabilities {
        observe: true,
        navigate: true,
        native_input: false,
        tabs: true,
        frames: true,
        native_dialogs: false,
    }
}
