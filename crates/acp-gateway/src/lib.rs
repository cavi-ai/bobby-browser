//! ACP as a fourth adapter over the same runtime the other three drive.
//!
//! The runtime already has three adapters — HTTP, MCP, and CDP — held to
//! identical capability, idempotency, evidence, checkpoint, and event
//! semantics by `crates/interface-conformance`. A fourth that drifted from
//! them would be a second-class surface with its own security story, which is
//! the failure this crate is arranged to avoid: the parts that could drift are
//! decisions, not plumbing, and they live in modules with their own tests.
//!
//! [`server::AcpServer`] is the stdio transport: `initialize`, `session/new`,
//! `session/prompt`, `session/cancel`, and agent→client `session/update` plus
//! `session/request_permission`. Every call goes through the same
//! [`sdk_core::AuthenticatedRuntime`] the other adapters use.
//!
//! [`escalation`] maps ACP's `session/request_permission` onto the runtime's
//! capability model. That is the piece the design puts the weight on, because
//! it is where an editor's "allow" button could quietly become authority.

pub mod escalation;
pub mod server;

pub use escalation::{decide, Escalation, EscalationRequest, SessionPolicyGates};
pub use server::AcpServer;
