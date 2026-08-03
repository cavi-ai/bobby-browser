//! Holds a session's page structure so the driving agent does not have to.
//! A caller asks "where is the control described as X" and gets a bound target
//! plus a confidence score instead of a full accessibility snapshot.
//!
//! A stale graph does not fail loudly, it answers confidently and wrongly.
//! Three things contain that:
//!
//! 1. [`ContextGraph`] tracks a generation per page. An observation is stamped
//!    with the generation it was taken under, and an answer drawn from an older
//!    generation is not returned.
//! 2. Every navigation and every non-replayable command bumps the generation,
//!    so the default on any state change is to forget.
//! 3. A node answer is a proposal; `intent-engine` verification stays the
//!    authority on whether the action happened.

use std::collections::HashMap;
use std::sync::Mutex;

use types::{
    AccessibilityNode, AccessibilityTarget, CommandId, ContextAnswer, PageId, PrimitiveCommand,
    RuntimeCommand,
};

/// Answers scoring below this are discarded, not returned with a low score.
/// Kept equal to the vision confidence floor: same decision, one threshold.
pub const CONTEXT_CONFIDENCE_FLOOR: f32 = 0.75;

/// The most pages the graph retains at once.
///
/// Pages are normally evicted on close or session delete. This bound covers
/// what those miss (a worker killed mid-session, a client that never closes),
/// which would otherwise leak page text for the life of the process.
pub const MAX_RETAINED_PAGES: usize = 256;

/// Command ids retained per page. Bounded because this grows with workflow
/// length, not with anything self-limiting.
pub const MAX_RETAINED_COMMANDS: usize = 64;

/// Page structure retained for a session, keyed by page.
#[derive(Default)]
pub struct ContextGraph {
    pages: Mutex<HashMap<PageId, PageContext>>,
    /// Monotonic recording counter used to pick an eviction victim. Not a
    /// timestamp: same-tick recordings tie and the clock can move backwards.
    sequence: std::sync::atomic::AtomicU64,
}

struct PageContext {
    /// Bumped on every navigation and every non-replayable command.
    generation: u64,
    /// The generation `nodes` was observed under.
    observed_at: u64,
    /// When this page was last recorded, for eviction ordering.
    recorded_seq: u64,
    /// Command ids whose evidence the runtime recorded against this page,
    /// newest last, capped at [`MAX_RETAINED_COMMANDS`].
    ///
    /// Ids only, never the evidence: the journal stays the single authority on
    /// what happened, and a copy here could disagree with it.
    commands: Vec<CommandId>,
    nodes: Vec<AccessibilityNode>,
}

impl ContextGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an accessibility observation for `page`.
    pub fn record(&self, page: &PageId, nodes: Vec<AccessibilityNode>) {
        let seq = self
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut pages = self.lock();
        if !pages.contains_key(page) && pages.len() >= MAX_RETAINED_PAGES {
            // Evict the least-recently-recorded entry rather than refusing the
            // new page: refusing would leave every later page unanswerable.
            if let Some(stalest) = pages
                .iter()
                .min_by_key(|(_, context)| context.recorded_seq)
                .map(|(id, _)| id.clone())
            {
                pages.remove(&stalest);
            }
        }
        let entry = pages.entry(page.clone()).or_insert(PageContext {
            generation: 0,
            observed_at: 0,
            recorded_seq: seq,
            commands: Vec::new(),
            nodes: Vec::new(),
        });
        entry.observed_at = entry.generation;
        entry.recorded_seq = seq;
        entry.nodes = nodes;
    }

    /// Records that `command` produced evidence against `page`.
    ///
    /// Unlike [`Self::record`], this does not require the page to have been
    /// observed, and it survives invalidation: what happened on a page stays
    /// true after the page changes.
    pub fn record_command(&self, page: &PageId, command: CommandId) {
        let seq = self
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut pages = self.lock();
        if !pages.contains_key(page) && pages.len() >= MAX_RETAINED_PAGES {
            return;
        }
        let entry = pages.entry(page.clone()).or_insert(PageContext {
            generation: 0,
            observed_at: 0,
            recorded_seq: seq,
            commands: Vec::new(),
            nodes: Vec::new(),
        });
        if entry.commands.contains(&command) {
            return;
        }
        if entry.commands.len() >= MAX_RETAINED_COMMANDS {
            entry.commands.remove(0);
        }
        entry.commands.push(command);
    }

    /// Command ids whose evidence the runtime recorded against `page`, oldest
    /// first. Resolve them through `checkpoint_save`'s `evidenceRefs` or the
    /// journal; this answers *which* commands, never *what* they produced.
    pub fn commands_for(&self, page: &PageId) -> Vec<CommandId> {
        self.lock()
            .get(page)
            .map(|entry| entry.commands.clone())
            .unwrap_or_default()
    }

    /// Drops every page in `pages`, on session close. Retention is bounded by
    /// session lifetime.
    pub fn forget_all(&self, pages: &[PageId]) {
        let mut retained = self.lock();
        for page in pages {
            retained.remove(page);
        }
    }

    /// Number of pages currently retained.
    pub fn retained_pages(&self) -> usize {
        self.lock().len()
    }

    /// Invalidates everything known about `page`.
    ///
    /// Bumps the generation rather than clearing the nodes. Keeping the stamped
    /// observation makes the staleness check total; clearing would race a
    /// concurrent `record` landing before the next observation.
    pub fn invalidate(&self, page: &PageId) {
        let mut pages = self.lock();
        if let Some(entry) = pages.get_mut(page) {
            entry.generation = entry.generation.saturating_add(1);
        }
    }

    /// Invalidates `page` unless `command` is known to leave page structure
    /// alone.
    ///
    /// Do not switch this to `CommandClass`: `Navigate` and `Emulate` are both
    /// `Replayable` yet both change the DOM. Classification answers "safe to
    /// replay", not "did the DOM change".
    ///
    /// Explicit allowlist with an invalidating default, so a primitive added
    /// later invalidates until someone deliberately adds it below.
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
    /// the floor.
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
            // Two controls describe themselves the same way; which one is
            // unanswerable.
            return None;
        }
        (confidence >= CONTEXT_CONFIDENCE_FLOOR).then_some(ContextAnswer { target, confidence })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PageId, PageContext>> {
        // Poison recovery is safe here: every read re-checks the generation
        // stamp, so a torn entry answers `None` rather than answering wrongly.
        self.pages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Whether `command` is known to leave the page's structure as it was.
///
/// Read-only primitives only. Everything else invalidates, including every
/// intent.
fn preserves_page_structure(command: &RuntimeCommand) -> bool {
    let RuntimeCommand::Primitive(primitive) = command else {
        // An intent compiles to a sequence the engine picks after this check.
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
/// Only exact accessible-name equality scores at full confidence. Softer
/// matches score below the floor on purpose, so a substring hit on a long
/// label cannot become a confident wrong answer.
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

    /// The commands that must NOT invalidate.
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

    /// `Navigate` and `Emulate` are both `Replayable` and both change the page,
    /// which is why invalidation allowlists commands, not command classes.
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

    /// An intent compiles to primitives picked at runtime, so it is never
    /// structure-preserving.
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
    fn deleting_a_session_forgets_every_page_it_owned() {
        let graph = ContextGraph::new();
        let owned: Vec<PageId> = (0..3).map(|_| PageId::new()).collect();
        let other = PageId::new();
        for page in owned.iter().chain(std::iter::once(&other)) {
            graph.record(page, vec![node("textbox", "Email address", Some(1))]);
        }
        graph.forget_all(&owned);
        for page in &owned {
            assert_eq!(
                graph.ask(page, "Email address"),
                None,
                "a deleted session's page structure survived"
            );
        }
        assert!(
            graph.ask(&other, "Email address").is_some(),
            "forgetting one session's pages took another session's with it"
        );
    }

    /// The bound covers what close and session delete miss, so the map cannot
    /// grow for the life of the process.
    #[test]
    fn retention_is_bounded() {
        let graph = ContextGraph::new();
        for _ in 0..(MAX_RETAINED_PAGES + 50) {
            graph.record(
                &PageId::new(),
                vec![node("textbox", "Email address", Some(1))],
            );
        }
        assert_eq!(graph.retained_pages(), MAX_RETAINED_PAGES);
    }

    /// Eviction must drop the stalest entry, not refuse the new one.
    #[test]
    fn eviction_drops_the_stalest_page_not_the_newest() {
        let graph = ContextGraph::new();
        let first = PageId::new();
        graph.record(&first, vec![node("textbox", "Email address", Some(1))]);
        for _ in 0..MAX_RETAINED_PAGES {
            graph.record(&PageId::new(), vec![node("textbox", "Other", Some(1))]);
        }
        assert_eq!(
            graph.ask(&first, "Email address"),
            None,
            "the stalest page survived eviction"
        );
        let newest = PageId::new();
        graph.record(&newest, vec![node("textbox", "Newest", Some(1))]);
        assert!(
            graph.ask(&newest, "Newest").is_some(),
            "the graph stopped learning once it hit the bound"
        );
    }

    #[test]
    fn closing_a_page_forgets_it() {
        let (graph, page) = graph_with(vec![node("textbox", "Email address", Some(1))]);
        graph.forget(&page);
        assert_eq!(graph.ask(&page, "Email address"), None);
    }

    /// Answers must be a target from the recorded snapshot, matching what
    /// `a11y_snapshot` would return for the same page.
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
