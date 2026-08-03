//! Phase-scoped toolsets: `tools/list` advertises only the tools a phase needs,
//! keeping the response inside the 131,072-byte budget.
//!
//! [`Toolset::Full`] is the default and lists everything the principal's
//! capabilities allow. An agent opts into a narrower phase with
//! `toolset_select`, which also emits `notifications/tools/list_changed` so the
//! client re-reads.
//!
//! Phases follow the runtime's working loop:
//!
//! - [`Toolset::Explore`]: open sessions and pages, read the page. No mutation.
//! - [`Toolset::Act`]: the primitives that change the page.
//! - [`Toolset::Intent`]: the `intent_*` family.
//! - [`Toolset::Verify`]: evidence, checkpoints, recovery.
//!
//! `Act` and `Intent` stay separate because the `intent_*` schemas are the
//! largest on the surface (5-6 KB apiece); merging them advertises most of the
//! list, which saves nothing and still costs a round trip to select.
//!
//! Session and runtime lifecycle tools appear in every phase, because an agent
//! that cannot close what it opened leaks workers regardless of phase.

use std::fmt;

/// Which tools `tools/list` advertises.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Toolset {
    /// Everything the principal's capabilities allow.
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
    /// A phase narrows what is shown, never what is allowed: a hidden tool is
    /// still callable, and capability gates stay the only enforcement boundary.
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

/// Present in every phase: session and page lifecycle, plus `toolset_select`
/// so a narrowed agent can always leave its phase.
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

/// Raw primitives. `command_execute` is the escape hatch for a caller minting
/// its own envelope, and is absent from the other narrow phases.
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
    // Verification of an action without leaving the phase.
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

    /// Every tool the gateway advertises. Duplicated rather than imported so a
    /// tool added to `list_tools` without a phase fails here.
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

    /// A tool in no phase is unreachable for any agent that narrows.
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

    /// Lifecycle is phase-independent: a session opened in one phase must be
    /// closable without switching phase first.
    #[test]
    fn every_phase_advertises_session_and_page_lifecycle() {
        for phase in Toolset::ALL {
            for tool in ["session_create", "session_close", "page_open", "page_close"] {
                assert!(phase.advertises(tool), "{phase} does not advertise {tool}");
            }
        }
    }

    /// A mutating tool must not appear in the read-only phase.
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

    /// The two mutating phases must not overlap: each names one driving style.
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
