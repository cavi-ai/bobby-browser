//! Firefox companion adapter.

pub mod bidi;
mod worker;

pub use bidi::{BidiClient, BidiEvent, BidiTransport};
pub use worker::{
    CompanionExtensionObserver, ExtensionObservation, ExtensionObserver, FirefoxCompanionFactory,
    FirefoxCompanionWorker, MAX_TRACKED_PAGES,
};
