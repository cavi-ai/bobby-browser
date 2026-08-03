//! A node that holds a session's page structure so the driving agent does not
//! have to.
//!
//! Today an agent calls `a11y_snapshot`, receives up to 2048 nodes, and reasons
//! over them in its own context — on top of the whole tool surface. Against a
//! context node it asks "where is the control described as X" and receives a
//! bound target plus a confidence score. Same answer, materially less context.
//!
//! # The failure this module is mostly about
//!
//! Inverting the data flow moves a correctness risk with it. A stale graph does
//! not fail loudly; it answers *confidently and wrongly*, which is worse than
//! not answering, because the caller has no signal to distrust. The spec calls
//! this out as the highest correctness risk in the node program.
//!
//! Three things contain it:
//!
//! 1. [`ContextGraph`] tracks a generation per page. Recording an observation
//!    stamps it with the generation it was taken under, and an answer drawn
//!    from an older generation is not returned.
//! 2. Every navigation and every non-replayable command bumps the generation,
//!    so the default on any state change is to forget rather than to keep.
//! 3. The runtime's existing target-drift detection stays the backstop. A node
//!    answer is a proposal; `intent-engine` verification remains the authority
//!    on whether the action happened.

use std::collections::HashMap;
use std::sync::Mutex;

use types::{AccessibilityNode, AccessibilityTarget, PageId, PrimitiveCommand, RuntimeCommand};

/// A bounded answer from a context node.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextAnswer {
    pub target: AccessibilityTarget,
    pub confidence: f32,
}

/// Below this, a context answer is not worth acting on and is discarded rather
/// than returned with a low score. Matches the vision confidence floor: both
/// are "a proposal the runtime will act on", and having two different floors
/// for the same decision invites picking whichever is convenient.
pub const CONTEXT_CONFIDENCE_FLOOR: f32 = 0.75;

/// Page structure retained for a session, keyed by page.
///
/// In-process by design for now: the contract is what matters, and a graph that
/// lives behind an HTTP hop has exactly the same staleness problem with a
/// network in the middle. Moving it into a separate process is a transport
/// change against this same interface.
#[derive(Default)]
pub struct ContextGraph {
    pages: Mutex<HashMap<PageId, PageContext>>,
}

struct PageContext {
    /// Bumped on every navigation and every non-replayable command.
    generation: u64,
    /// The generation `nodes` was observed under.
    observed_at: u64,
    nodes: Vec<AccessibilityNode>,
}

impl ContextGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an accessibility observation for `page`.
    pub fn record(&self, page: &PageId, nodes: Vec<AccessibilityNode>) {
        let mut pages = self.lock();
        let entry = pages.entry(page.clone()).or_insert(PageContext {
            generation: 0,
            observed_at: 0,
            nodes: Vec::new(),
        });
        entry.observed_at = entry.generation;
        entry.nodes = nodes;
    }

    /// Invalidates everything known about `page`.
    ///
    /// Bumps the generation rather than clearing the nodes: an answer must be
    /// refused because it is *old*, and keeping the observation with its stamp
    /// makes that check total. Clearing would work too, right up until a
    /// concurrent `record` lands between the clear and the next observation.
    pub fn invalidate(&self, page: &PageId) {
        let mut pages = self.lock();
        if let Some(entry) = pages.get_mut(page) {
            entry.generation = entry.generation.saturating_add(1);
        }
    }

    /// Invalidates `page` unless `command` is known to leave page structure
    /// alone.
    ///
    /// `CommandClass` is the obvious signal here and it is the wrong one.
    /// `Navigate` is `Replayable` — replaying it lands on the same URL, so the
    /// classification is right — while replacing the entire document.
    /// `Emulate` is `Replayable` and reflows the page. Classification answers
    /// "is it safe to replay this", which is a different question from "did
    /// the DOM change".
    ///
    /// So this is an explicit allowlist with an invalidating default: a
    /// primitive added later invalidates until someone deliberately adds it
    /// below. The costs are asymmetric — a graph that forgets too eagerly
    /// costs one extra snapshot, one that forgets too late hands out a
    /// confident wrong target.
    pub fn invalidate_for(&self, page: &PageId, command: &RuntimeCommand) {
        if !preserves_page_structure(command) {
            self.invalidate(page);
        }
    }

    /// Drops everything for `page`, on page close.
    pub fn forget(&self, page: &PageId) {
        self.lock().remove(page);
    }

    /// Answers "where is the control described as `description`" from what was
    /// observed, or `None`.
    ///
    /// `None` covers every uncertain case: nothing recorded, the recording is
    /// stale, no node matched, the match was ambiguous, or the score was under
    /// the floor. A context node that guesses is worse than one that declines,
    /// because the caller cannot tell the two apart.
    pub fn ask(&self, page: &PageId, description: &str) -> Option<ContextAnswer> {
        let pages = self.lock();
        let entry = pages.get(page)?;
        if entry.observed_at != entry.generation {
            // Observed before a change that has not been re-observed.
            return None;
        }
        let needle = description.trim().to_lowercase();
        if needle.is_empty() {
            return None;
        }
        let mut best: Option<(f32, AccessibilityTarget)> = None;
        let mut best_is_tied = false;
        for node in &entry.nodes {
            let Some(target) = node.target.as_ref() else {
                continue;
            };
            let Some(score) = score_match(&needle, target, node) else {
                continue;
            };
            match &best {
                Some((current, _)) if *current > score => {}
                Some((current, _)) if (*current - score).abs() < f32::EPSILON => {
                    best_is_tied = true;
                }
                _ => {
                    best = Some((score, target.clone()));
                    best_is_tied = false;
                }
            }
        }
        let (confidence, target) = best?;
        if best_is_tied {
            // Two controls describe themselves the same way. The caller asked
            // "which one" and the honest answer is that this graph cannot say.
            return None;
        }
        (confidence >= CONTEXT_CONFIDENCE_FLOOR).then_some(ContextAnswer { target, confidence })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PageId, PageContext>> {
        // A poisoned lock means a previous holder panicked mid-update, so the
        // map may be inconsistent. Recovering the guard and carrying on is
        // right here: every read re-checks the generation stamp, so a torn
        // entry answers `None` rather than answering wrongly.
        self.pages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Whether `command` is known to leave the page's structure as it was.
///
/// Read-only primitives only. Everything else — including every intent, which
/// compiles to primitives the engine chooses at runtime — invalidates.
fn preserves_page_structure(command: &RuntimeCommand) -> bool {
    let RuntimeCommand::Primitive(primitive) = command else {
        // Intents are compiled to a sequence the engine picks; treating any of
        // them as structure-preserving would mean trusting a decision made
        // after this check.
        return false;
    };
    matches!(
        primitive,
        PrimitiveCommand::Inspect(_)
            | PrimitiveCommand::ListPages(_)
            | PrimitiveCommand::WaitFor(_)
            | PrimitiveCommand::NetworkLog(_)
            | PrimitiveCommand::AccessibilitySnapshot(_)
            | PrimitiveCommand::GetCookies(_)
            | PrimitiveCommand::CaptureScreenshot(_)
    )
}

/// Scores how well a described control matches a node, or `None` if it does not
/// match at all.
///
/// Exact accessible-name equality is the only thing scored at full confidence.
/// Everything softer is scored below the floor on purpose: the point of this
/// node is to remove page material from the agent's context, not to guess on
/// its behalf, and a substring hit on a long label is exactly the kind of
/// plausible-but-wrong answer the module header warns about.
fn score_match(
    needle: &str,
    target: &AccessibilityTarget,
    node: &AccessibilityNode,
) -> Option<f32> {
    let name = target.accessible_name.trim().to_lowercase();
    if name == needle {
        return Some(1.0);
    }
    let role = target.role.trim().to_lowercase();
    if format!("{role} {name}") == needle {
        return Some(0.9);
    }
    if let Some(node_name) = node.name.as_deref() {
        if node_name.trim().to_lowercase() == needle {
            return Some(0.8);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::{
        AccessibilitySnapshotCommand, CaptureScreenshotCommand, EmulateCommand, InspectCommand,
        NavigateCommand, ScreenshotMode,
    };

    fn node(role: &str, name: &str, ordinal: Option<usize>) -> AccessibilityNode {
        AccessibilityNode {
            role: Some(role.to_owned()),
            name: Some(name.to_owned()),
            target: Some(AccessibilityTarget {
                role: role.to_owned(),
                accessible_name: name.to_owned(),
                ordinal,
            }),
            ..AccessibilityNode::default()
        }
    }

    fn graph_with(nodes: Vec<AccessibilityNode>) -> (ContextGraph, PageId) {
        let graph = ContextGraph::new();
        let page = PageId::new();
        graph.record(&page, nodes);
        (graph, page)
    }

    #[test]
    fn it_answers_an_exact_accessible_name() {
        let (graph, page) = graph_with(vec![
            node("textbox", "Email address", Some(1)),
            node("button", "Continue", None),
        ]);
        let answer = graph.ask(&page, "Email address").expect("a match");
        assert_eq!(answer.target.role, "textbox");
        assert_eq!(answer.confidence, 1.0);
    }

    #[test]
    fn an_unobserved_page_answers_nothing() {
        let graph = ContextGraph::new();
        assert_eq!(graph.ask(&PageId::new(), "Email address"), None);
    }

    #[test]
    fn a_navigation_makes_every_prior_answer_unavailable() {
        let (graph, page) = graph_with(vec![node("textbox", "Email address", Some(1))]);
        assert!(graph.ask(&page, "Email address").is_some());
        graph.invalidate(&page);
        assert_eq!(
            graph.ask(&page, "Email address"),
            None,
            "a stale graph answered after the page changed"
        );
    }

    #[test]
    fn re_observing_after_a_change_makes_the_graph_answer_again() {
        let (graph, page) = graph_with(vec![node("textbox", "Email address", Some(1))]);
        graph.invalidate(&page);
        assert_eq!(graph.ask(&page, "Email address"), None);
        graph.record(&page, vec![node("textbox", "Email address", Some(1))]);
        assert!(
            graph.ask(&page, "Email address").is_some(),
            "the graph stayed stale after being re-observed"
        );
    }

    fn primitive(command: PrimitiveCommand) -> RuntimeCommand {
        RuntimeCommand::Primitive(command)
    }

    /// The commands that must NOT invalidate. Anything read-only staying
    /// answerable is what makes the node worth having: a workflow that
    /// inspects between steps would otherwise throw the graph away every time.
    #[test]
    fn read_only_primitives_leave_the_graph_answerable() {
        for command in [
            primitive(PrimitiveCommand::Inspect(InspectCommand {
                selector: None,
                target: None,
                include_html: false,
            })),
            primitive(PrimitiveCommand::AccessibilitySnapshot(
                AccessibilitySnapshotCommand { max_nodes: None },
            )),
            primitive(PrimitiveCommand::CaptureScreenshot(
                CaptureScreenshotCommand {
                    mode: ScreenshotMode::Viewport,
                },
            )),
        ] {
            let (graph, page) = graph_with(vec![node("textbox", "Email address", Some(1))]);
            graph.invalidate_for(&page, &command);
            assert!(
                graph.ask(&page, "Email address").is_some(),
                "a read-only command invalidated the graph: {command:?}"
            );
        }
    }

    /// `Navigate` and `Emulate` are both `CommandClass::Replayable`, and both
    /// change what is on the page. They are the reason this is an allowlist
    /// over commands rather than a check on command class.
    #[test]
    fn replayable_commands_that_change_the_page_still_invalidate() {
        for command in [
            primitive(PrimitiveCommand::Navigate(NavigateCommand {
                url: "https://example.test/next".to_owned(),
                wait_until: types::WaitUntil::DomContentLoaded,
                timeout_ms: 30_000,
            })),
            primitive(PrimitiveCommand::Emulate(EmulateCommand {
                viewport: None,
                geolocation: None,
                mobile: None,
            })),
        ] {
            assert_eq!(
                command.class(),
                types::CommandClass::Replayable,
                "this test is pointless if {command:?} is not replayable"
            );
            let (graph, page) = graph_with(vec![node("textbox", "Email address", Some(1))]);
            graph.invalidate_for(&page, &command);
            assert!(
                graph.ask(&page, "Email address").is_none(),
                "a replayable page-changing command left the graph answerable: {command:?}"
            );
        }
    }

    /// An intent compiles to primitives the engine picks at runtime, so it can
    /// never be treated as structure-preserving.
    #[test]
    fn every_intent_invalidates() {
        let (graph, page) = graph_with(vec![node("textbox", "Email address", Some(1))]);
        let intent = RuntimeCommand::Intent(types::IntentCommand::Locate(types::LocateIntent {
            purpose: "find the email field".to_owned(),
            hints: types::IntentHints::default(),
        }));
        graph.invalidate_for(&page, &intent);
        assert!(
            graph.ask(&page, "Email address").is_none(),
            "an intent left the graph answerable"
        );
    }

    #[test]
    fn two_controls_with_the_same_name_answer_nothing() {
        let (graph, page) = graph_with(vec![
            node("textbox", "Address", Some(1)),
            node("textbox", "Address", Some(2)),
        ]);
        assert_eq!(
            graph.ask(&page, "Address"),
            None,
            "an ambiguous description produced a confident answer"
        );
    }

    #[test]
    fn a_partial_description_does_not_reach_the_floor() {
        let (graph, page) = graph_with(vec![node(
            "textbox",
            "Email address for billing notifications",
            Some(1),
        )]);
        assert_eq!(
            graph.ask(&page, "Email"),
            None,
            "a substring match was returned as a confident target"
        );
    }

    #[test]
    fn an_empty_description_answers_nothing() {
        let (graph, page) = graph_with(vec![node("textbox", "Email address", Some(1))]);
        assert_eq!(graph.ask(&page, "   "), None);
    }

    #[test]
    fn closing_a_page_forgets_it() {
        let (graph, page) = graph_with(vec![node("textbox", "Email address", Some(1))]);
        graph.forget(&page);
        assert_eq!(graph.ask(&page, "Email address"), None);
    }

    /// Answers must agree with what `a11y_snapshot` would return for the same
    /// page: the node is a cheaper route to the same target, not a second
    /// opinion about it.
    #[test]
    fn an_answer_is_a_target_from_the_recorded_snapshot() {
        let snapshot = vec![
            node("textbox", "Email address", Some(1)),
            node("textbox", "Postal code", Some(2)),
        ];
        let (graph, page) = graph_with(snapshot.clone());
        let answer = graph.ask(&page, "Postal code").expect("a match");
        let from_snapshot = snapshot
            .iter()
            .find_map(|node| node.target.clone())
            .into_iter()
            .chain(snapshot.iter().filter_map(|node| node.target.clone()))
            .find(|target| target.accessible_name == "Postal code")
            .expect("the snapshot carries the target");
        assert_eq!(answer.target, from_snapshot);
    }
}
