//! Firefox companion adapter.

pub mod bidi;
mod worker;

pub use bidi::{BidiClient, BidiEvent, BidiTransport};
pub use worker::{
    CompanionExtensionObserver, ExtensionControl, ExtensionObservation, ExtensionObserver,
    ExtensionPageBinding, FirefoxCompanionFactory, FirefoxCompanionWorker, MAX_TRACKED_PAGES,
};
