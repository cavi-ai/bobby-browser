//! Firefox companion adapter with behavioral engine integration.

pub mod bidi;
mod worker;

pub use bidi::{BidiClient, BidiEvent, BidiTransport, SharedBiDiTransport};
pub use worker::{
    CompanionExtensionObserver, ExtensionControl, ExtensionObservation, ExtensionObserver,
    ExtensionPageBinding, FirefoxCompanionFactory, FirefoxCompanionWorker, MAX_TRACKED_PAGES,
};

pub use behavioral_engine::{
    BezierMouseSimulator, BehavioralConfig, MouseConfig, MousePath, MousePoint, ScrollAction,
    ScrollConfig, ScrollSimulator, SessionRandom, TextConfig, TypingAction, TypingSimulator,
    generate_session_seed,
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
