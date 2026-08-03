//! Phase-scoped toolsets: advertise the tools the current phase needs instead
//! of all 41 at all times.
//!
//! `tools/list` for a principal holding every capability is 126,257 bytes
//! against a 131,072-byte budget — 96% used, with roughly one intent tool of
//! headroom left. Every new primitive is charged to every principal on every
//! connect, whether or not that principal is in a phase where it could call it.
//!
//! # Opt-in, not imposed
//!
//! Narrowing what an agent can see is a behaviour change for every existing
//! client, and one that fails confusingly: a tool the agent used yesterday is
//! simply absent, with nothing saying why. So [`Toolset::Full`] is the default
//! and is exactly today's list. An agent that wants the smaller surface asks
//! for it with `toolset_select`, and the server answers plus emits
//! `notifications/tools/list_changed` so the client re-reads.
//!
//! This is what A6's `tools/list_changed` was built for. Until now nothing
//! changed a principal's tool list mid-session, so the notification existed
//! with no producer.
//!
//! # The phases
//!
//! Drawn from the working loop the runtime already documents
//! (`a11y_snapshot` → intent → evidence check → checkpoint), not invented here:
//!
//! - [`Toolset::Explore`] — open sessions and pages, read the page. No mutation.
//! - [`Toolset::Act`] — the primitives that change the page.
//! - [`Toolset::Intent`] — the `intent_*` family.
//! - [`Toolset::Verify`] — evidence, checkpoints, recovery.
//!
//! `Act` and `Intent` are separate because the eight `intent_*` tools are the
//! largest schemas on the surface (5–6 KB apiece) and an agent driving through
//! intents does not also need the raw primitives, nor the reverse. Folding them
//! into one "do things" phase was the first shape tried and it advertised 35 of
//! 43 tools — a phase that saves nothing is worse than no phase, because it
//! costs a round trip to select.
//!
//! Session and runtime lifecycle tools appear in every phase, because an agent
//! that cannot close what it opened leaks workers regardless of phase.

use std::fmt;

/// Which tools `tools/list` advertises.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Toolset {
    /// Everything the principal's capabilities allow. The default, and
    /// byte-for-byte what the surface advertised before phases existed.
    #[default]
    Full,
    Explore,
    Act,
    Intent,
    Verify,
}

impl Toolset {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "full" => Some(Self::Full),
            "explore" => Some(Self::Explore),
            "act" => Some(Self::Act),
            "intent" => Some(Self::Intent),
            "verify" => Some(Self::Verify),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Explore => "explore",
            Self::Act => "act",
            Self::Intent => "intent",
            Self::Verify => "verify",
        }
    }

    pub const ALL: [Self; 5] = [
        Self::Full,
        Self::Explore,
        Self::Act,
        Self::Intent,
        Self::Verify,
    ];

    /// Every phase that narrows.
    pub const NARROW: [Self; 4] = [Self::Explore, Self::Act, Self::Intent, Self::Verify];

    /// Whether `tool` is advertised in this phase.
    ///
    /// Capability filtering still runs on top of this and is unchanged: a phase
    /// narrows what is *shown*, never what is *allowed*. A tool hidden by the
    /// current phase is still callable — hiding it is a context optimisation,
    /// and turning it into an enforcement boundary would put a second,
    /// weaker authority next to the capability gates.
    pub fn advertises(self, tool: &str) -> bool {
        if self == Self::Full || ALWAYS.contains(&tool) {
            return true;
        }
        match self {
            Self::Full => true,
            Self::Explore => EXPLORE.contains(&tool),
            Self::Act => ACT.contains(&tool),
            Self::Intent => INTENT.contains(&tool),
            Self::Verify => VERIFY.contains(&tool),
        }
    }
}

impl fmt::Display for Toolset {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Present in every phase: lifecycle an agent needs regardless of what it is
/// doing, plus the phase switch itself — without which an agent that narrowed
/// to one phase could never leave it.
const ALWAYS: &[&str] = &[
    "runtime_info",
    "session_create",
    "session_list",
    "session_close",
    "page_open",
    "page_list",
    "page_close",
    "page_activate",
    "toolset_select",
];

const EXPLORE: &[&str] = &[
    "a11y_snapshot",
    "form_snapshot",
    "inspect",
    "screenshot",
    "context_ask",
    "navigate",
    "wait_for",
    "network_log",
    "cookie_get",
];

/// Raw primitives. `command_execute` lives here as the escape hatch for a
/// caller minting its own envelope; it is absent from the other narrow phases
/// because at ~1 KB it is pure overhead to an agent using named tools.
const ACT: &[&str] = &[
    "click",
    "type_text",
    "control_action",
    "upload_files",
    "dialog",
    "emulate",
    "navigate",
    "wait_for",
    "download_url",
    "evaluate_javascript",
    "cookie_set",
    "cookie_delete",
    "command_execute",
    // An agent that acts must be able to see what it did without changing
    // phase; otherwise every check costs two `tools/list` round trips.
    "a11y_snapshot",
    "context_ask",
];

const INTENT: &[&str] = &[
    "intent_locate",
    "intent_fill",
    "intent_complete_form",
    "intent_submit_and_verify",
    "intent_wait_for_state",
    "intent_follow",
    "intent_dismiss_obstruction",
    "intent_extract",
    "extract_structured",
    "navigate",
    "wait_for",
    "a11y_snapshot",
    "form_snapshot",
    "context_ask",
];

const VERIFY: &[&str] = &[
    "checkpoint_save",
    "recovery_status",
    "workflow_recover",
    "events_read",
    "inspect",
    "a11y_snapshot",
    "form_snapshot",
    "screenshot",
    "context_ask",
    "pdf",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tool the gateway advertises. Kept here rather than imported so a
    /// tool added to `list_tools` without a phase fails this file, which is the
    /// only place that decides where it belongs.
    const EVERY_TOOL: &[&str] = &[
        "a11y_snapshot",
        "checkpoint_save",
        "click",
        "command_execute",
        "context_ask",
        "control_action",
        "cookie_delete",
        "cookie_get",
        "cookie_set",
        "dialog",
        "download_url",
        "emulate",
        "evaluate_javascript",
        "events_read",
        "extract_structured",
        "form_snapshot",
        "inspect",
        "intent_complete_form",
        "intent_dismiss_obstruction",
        "intent_extract",
        "intent_fill",
        "intent_follow",
        "intent_locate",
        "intent_submit_and_verify",
        "intent_wait_for_state",
        "navigate",
        "network_log",
        "page_activate",
        "page_close",
        "page_list",
        "page_open",
        "pdf",
        "recovery_status",
        "runtime_info",
        "screenshot",
        "session_close",
        "session_create",
        "session_list",
        "toolset_select",
        "type_text",
        "upload_files",
        "wait_for",
        "workflow_recover",
    ];

    #[test]
    fn full_advertises_everything() {
        for tool in EVERY_TOOL {
            assert!(
                Toolset::Full.advertises(tool),
                "{tool} is missing from the default toolset"
            );
        }
    }

    /// Every tool has to be reachable from some phase. A tool in no phase is
    /// unreachable for any agent that narrows, which is a silent capability
    /// loss rather than a smaller surface.
    #[test]
    fn every_tool_belongs_to_at_least_one_narrow_phase() {
        for tool in EVERY_TOOL {
            let phases: Vec<_> = Toolset::NARROW
                .into_iter()
                .filter(|phase| phase.advertises(tool))
                .collect();
            assert!(
                !phases.is_empty(),
                "{tool} is in no narrow phase, so a narrowed agent can never see it"
            );
        }
    }

    /// An agent that narrows must always be able to un-narrow.
    #[test]
    fn every_phase_advertises_the_phase_switch() {
        for phase in Toolset::ALL {
            assert!(
                phase.advertises("toolset_select"),
                "{phase} cannot leave itself"
            );
        }
    }

    /// Lifecycle is phase-independent: an agent that opened a session must be
    /// able to close it without switching phase first.
    #[test]
    fn every_phase_advertises_session_and_page_lifecycle() {
        for phase in Toolset::ALL {
            for tool in ["session_create", "session_close", "page_open", "page_close"] {
                assert!(phase.advertises(tool), "{phase} does not advertise {tool}");
            }
        }
    }

    /// A mutating tool must not appear in the read-only phase — that is the
    /// one place a phase name would actively mislead.
    #[test]
    fn the_explore_phase_advertises_no_mutating_tool() {
        for tool in [
            "click",
            "type_text",
            "control_action",
            "upload_files",
            "intent_fill",
            "intent_submit_and_verify",
            "cookie_set",
            "cookie_delete",
            "evaluate_javascript",
        ] {
            assert!(
                !Toolset::Explore.advertises(tool),
                "explore advertises the mutating tool {tool}"
            );
        }
    }

    /// The two mutating phases must not overlap into one another's job: an
    /// agent that selected `intent` and finds raw `click` there has learned
    /// nothing about which style it is driving in.
    #[test]
    fn the_act_and_intent_phases_stay_distinct() {
        for tool in [
            "click",
            "type_text",
            "command_execute",
            "evaluate_javascript",
        ] {
            assert!(Toolset::Act.advertises(tool), "act lost {tool}");
            assert!(
                !Toolset::Intent.advertises(tool),
                "intent advertises {tool}"
            );
        }
        for tool in ["intent_fill", "intent_submit_and_verify", "intent_locate"] {
            assert!(Toolset::Intent.advertises(tool), "intent lost {tool}");
            assert!(!Toolset::Act.advertises(tool), "act advertises {tool}");
        }
    }

    #[test]
    fn unknown_phase_names_are_rejected() {
        assert_eq!(Toolset::parse("explore"), Some(Toolset::Explore));
        assert_eq!(Toolset::parse("Explore"), None);
        assert_eq!(Toolset::parse(""), None);
        assert_eq!(Toolset::parse("everything"), None);
    }

    #[test]
    fn names_round_trip() {
        for phase in Toolset::ALL {
            assert_eq!(Toolset::parse(phase.as_str()), Some(phase));
        }
    }
}
