//! Internal workspace crate for `/v1` wire types (`publish = false`).
//!
//! Application code should depend on [`bobby-browser-client`](https://docs.rs/bobby-browser-client),
//! which embeds and re-exports these types for crates.io.
//!
//! Source of truth: `crates/bobby-browser-client/src/{auth,commands,…}.rs`.

#[path = "../../bobby-browser-client/src/auth.rs"]
mod auth;
#[path = "../../bobby-browser-client/src/challenges.rs"]
mod challenges;
#[path = "../../bobby-browser-client/src/commands.rs"]
mod commands;
#[path = "../../bobby-browser-client/src/forms.rs"]
mod forms;
#[path = "../../bobby-browser-client/src/ids.rs"]
mod ids;
#[path = "../../bobby-browser-client/src/interface.rs"]
mod interface;
#[path = "../../bobby-browser-client/src/outcomes.rs"]
mod outcomes;
#[path = "../../bobby-browser-client/src/recovery.rs"]
mod recovery;
#[path = "../../bobby-browser-client/src/skills.rs"]
mod skills;
#[path = "../../bobby-browser-client/src/state.rs"]
mod state;

pub use auth::*;
pub use challenges::*;
pub use commands::*;
pub use forms::*;
pub use ids::*;
pub use interface::*;
pub use outcomes::*;
pub use recovery::*;
pub use skills::*;
pub use state::*;
