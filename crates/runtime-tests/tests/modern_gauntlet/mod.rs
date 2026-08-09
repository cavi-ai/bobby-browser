// The corpus collector is live only in modern_gauntlet_collect; the release
// gate compiles this module tree without using it.
#[allow(dead_code)]
pub mod collector;
pub mod driver;
pub mod evidence;
pub mod scorecard;
pub use gauntlet_server as scenario;
