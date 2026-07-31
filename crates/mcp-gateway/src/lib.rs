#![recursion_limit = "256"]

pub mod protocol;
mod resources;
mod schema;
mod server;

pub use resources::{ArtifactCatalogFull, ArtifactResources};
pub use server::Server;
