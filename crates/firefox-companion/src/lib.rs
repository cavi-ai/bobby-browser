//! Firefox companion adapter with behavioral engine integration.

pub mod bidi;
pub mod bidi_endpoint;
mod fingerprint_host;
pub mod selection;
mod worker;

pub use bidi::{BidiClient, BidiEvent, BidiTransport, SharedBiDiTransport};
pub use bidi_endpoint::{bidi_url_from_endpoint_file, read_bidi_url_from_profile_dir};
pub use fingerprint_host::FirefoxBidiHost;
pub use worker::{
    CompanionExtensionObserver, ExtensionControl, ExtensionObservation, ExtensionObserver,
    ExtensionPageBinding, FirefoxCompanionFactory, FirefoxCompanionWorker, MAX_TRACKED_PAGES,
};

pub use behavioral_engine::{
    compose_typed_text, generate_session_seed, session_pause, BehavioralConfig,
    BezierMouseSimulator, MouseConfig, MousePath, MousePoint, ScrollAction, ScrollConfig,
    ScrollSimulator, SessionRandom, TextConfig, TypingAction, TypingSimulator,
};
pub use fingerprinting::{FingerprintApplyPlan, FingerprintConfig, FingerprintHost};

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
