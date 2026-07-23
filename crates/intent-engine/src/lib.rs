mod compiler;
mod engine;
mod stuck;
mod verify;

pub use compiler::{compile_intent, CompileError, IntentPlan};
pub use engine::{IntentBrowser, IntentEngine, IntentOutcome, VisionContext};
pub use stuck::{never_escalates, StuckKind};
pub use verify::{execution_record, summarize_target};
