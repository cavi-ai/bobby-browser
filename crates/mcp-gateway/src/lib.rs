#![recursion_limit = "256"]

mod annotations;
pub mod protocol;
mod resources;
mod schema;
mod server;

#[doc(hidden)]
pub fn schema_for_test(name: &str) -> serde_json::Value {
    schema::tool_schema(name)
}

pub use resources::{ArtifactCatalogFull, ArtifactResources};
pub use server::Server;
