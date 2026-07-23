mod compiler;
mod stuck;

pub use compiler::{compile_intent, CompileError, IntentPlan};
pub use stuck::{never_escalates, StuckKind};
