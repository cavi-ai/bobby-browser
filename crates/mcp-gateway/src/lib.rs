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

/// The ceiling on a `tools/list` response for a principal holding every
/// capability — an eighth of the 1 MiB frame cap.
///
/// Each tool schema has to be self-contained (MCP gives clients no way to
/// resolve a `$ref` across tools), so a catalog this size is genuine cost
/// rather than duplication. The budget leaves room to grow while still
/// catching a reintroduction of the shared-`$defs` regression, which ran an
/// order of magnitude past this.
///
/// This lives on the crate rather than in a test file because two separate
/// test binaries gate on it. Two constants with the same value never both
/// bind — the looser one is dead weight, and a drift between them is a silent
/// hole — so there is exactly one definition and both tests import it.
pub const TOOLS_LIST_BYTE_BUDGET: usize = 128 * 1024;

/// No single tool may approach the whole type system on its own.
pub const PER_TOOL_BYTE_BUDGET: usize = 32 * 1024;

pub use notify::{NotificationSink, NotificationStream};
pub use resources::{ArtifactCatalogFull, ArtifactResources};
pub use server::Server;
