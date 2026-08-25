//! Typed HTTP client and `/v1` wire types for Bobby Browser.
//!
//! The single crates.io Rust SDK package. Mirrors
//! [`@cavi-ai/bobby-browser`](https://www.npmjs.com/package/@cavi-ai/bobby-browser)
//! for Rust callers.
//!
//! # Auth headers
//!
//! Every request includes:
//! - `Authorization: Bearer <token>`
//! - `x-interface-version`
//! - `x-correlation-id`
//! - `x-deadline`
//!
//! # Example
//!
//! ```rust,no_run
//! use bobby_browser_client::{BrowserRuntimeClient, CreateSessionRequest};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let client = BrowserRuntimeClient::new(
//!     "http://127.0.0.1:7777",
//!     std::env::var("AUTOMATION_RUNTIME_TOKEN")?,
//! )?;
//! let info = client.runtime_info(None).await?;
//! let session = client
//!     .create_session(
//!         &CreateSessionRequest {
//!             profile: "default".into(),
//!             proxy: None,
//!             execution_policy: Default::default(),
//!             zigzagzig: false,
//!         },
//!         None,
//!     )
//!     .await?;
//! client.delete_session(&session.id, None).await?;
//! let _ = info;
//! # Ok(()) }
//! ```
//!
//! # Features
//!
//! - `schema` — derive `schemars::JsonSchema` on wire types.

mod auth;
mod challenges;
mod commands;
mod forms;
mod http;
mod ids;
mod interface;
mod outcomes;
mod recovery;
mod skills;
mod state;

pub use auth::*;
pub use challenges::*;
pub use commands::*;
pub use forms::*;
pub use http::*;
pub use ids::*;
pub use interface::*;
pub use outcomes::*;
pub use recovery::*;
pub use skills::*;
pub use state::*;
