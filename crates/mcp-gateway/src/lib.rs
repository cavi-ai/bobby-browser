#![recursion_limit = "256"]

mod annotations;
pub mod notify;
mod prompts;
pub mod protocol;
mod resources;
mod schema;
mod server;

#[doc(hidden)]
pub fn schema_for_test(name: &str) -> serde_json::Value {
    schema::tool_schema(name)
}

/// Exposes the full, unpatched `definitions()` table (the source of truth
/// `tool_schema`/`tool_output_schema` both narrow from) so tests can guard
/// its internal consistency directly — Task 3's original breakage
/// (`recovery_decisions()` `$ref`-ing a since-deleted `Evidence` entry) lived
/// entirely inside this table, reachable from no emitted tool schema, so a
/// guard that only walks emitted schemas is blind to exactly that class of
/// bug.
#[doc(hidden)]
pub fn definitions_for_test() -> serde_json::Value {
    schema::definitions_for_test()
}

pub use notify::{NotificationSink, NotificationStream};
pub use resources::{ArtifactCatalogFull, ArtifactResources};
pub use server::Server;
