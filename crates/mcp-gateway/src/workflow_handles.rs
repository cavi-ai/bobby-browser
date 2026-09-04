use std::{
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
};

use serde_json::Value;
use tokio::sync::oneshot;
use uuid::Uuid;

pub(crate) const MAX_WORKFLOW_HANDLES: usize = 64;
pub(crate) const MAX_WORKFLOW_RESERVATIONS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkflowScope {
    SessionPage,
    SessionPageWorkflow,
}

/// Tools whose advertised scope can be replaced by a retained workflow handle.
/// Keep this literal table sorted: later call-time normalization uses this same
/// allowlist, so a tool can never advertise a handle form it cannot accept.
pub(crate) const WORKFLOW_SCOPE_TOOLS: &[(&str, WorkflowScope)] = &[
    ("a11y_snapshot", WorkflowScope::SessionPageWorkflow),
    ("click", WorkflowScope::SessionPageWorkflow),
    (
        "click_and_wait_for_popup",
        WorkflowScope::SessionPageWorkflow,
    ),
    ("context_ask", WorkflowScope::SessionPage),
    ("context_neighbors", WorkflowScope::SessionPage),
    ("control_action", WorkflowScope::SessionPageWorkflow),
    ("cookie_delete", WorkflowScope::SessionPageWorkflow),
    ("cookie_get", WorkflowScope::SessionPageWorkflow),
    ("cookie_set", WorkflowScope::SessionPageWorkflow),
    ("dialog", WorkflowScope::SessionPageWorkflow),
    ("download_url", WorkflowScope::SessionPageWorkflow),
    ("emulate", WorkflowScope::SessionPageWorkflow),
    ("evaluate_javascript", WorkflowScope::SessionPageWorkflow),
    ("extract_structured", WorkflowScope::SessionPageWorkflow),
    ("form_snapshot", WorkflowScope::SessionPage),
    ("inspect", WorkflowScope::SessionPageWorkflow),
    ("intent_complete_form", WorkflowScope::SessionPageWorkflow),
    (
        "intent_detect_challenge",
        WorkflowScope::SessionPageWorkflow,
    ),
    (
        "intent_dismiss_obstruction",
        WorkflowScope::SessionPageWorkflow,
    ),
    ("intent_extract", WorkflowScope::SessionPageWorkflow),
    ("intent_fill", WorkflowScope::SessionPageWorkflow),
    ("intent_follow", WorkflowScope::SessionPageWorkflow),
    ("intent_locate", WorkflowScope::SessionPageWorkflow),
    ("intent_solve_challenge", WorkflowScope::SessionPageWorkflow),
    (
        "intent_submit_and_verify",
        WorkflowScope::SessionPageWorkflow,
    ),
    ("intent_wait_for_state", WorkflowScope::SessionPageWorkflow),
    ("navigate", WorkflowScope::SessionPageWorkflow),
    ("network_log", WorkflowScope::SessionPageWorkflow),
    ("page_activate", WorkflowScope::SessionPageWorkflow),
    ("page_close", WorkflowScope::SessionPageWorkflow),
    ("pdf", WorkflowScope::SessionPageWorkflow),
    ("screenshot", WorkflowScope::SessionPageWorkflow),
    ("type_text", WorkflowScope::SessionPageWorkflow),
    ("upload_files", WorkflowScope::SessionPageWorkflow),
    ("wait_for", WorkflowScope::SessionPageWorkflow),
];

pub(crate) fn workflow_scope_for_tool(name: &str) -> Option<WorkflowScope> {
    WORKFLOW_SCOPE_TOOLS
        .binary_search_by_key(&name, |(tool, _)| *tool)
        .ok()
        .map(|index| WORKFLOW_SCOPE_TOOLS[index].1)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkflowBinding {
    pub(crate) generation: u64,
    pub(crate) session_id: types::SessionId,
    pub(crate) page_id: types::PageId,
    pub(crate) workflow_id: types::WorkflowId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkflowHandleError {
    CapacityExhausted,
    GenerationChanged,
    SupervisorLost,
    Unknown,
    BindingConflict,
    Malformed,
}

#[derive(Default)]
struct WorkflowHandleState {
    generation: u64,
    bindings: BTreeMap<String, WorkflowBinding>,
    lru: VecDeque<String>,
    reservations: BTreeSet<String>,
    #[cfg(test)]
    test_hooks: WorkflowHandleTestHooks,
}

/// Test-only critical-section controls used to make the reset/publication
/// interleavings observable without changing the release build's behavior.
#[cfg(test)]
#[derive(Default)]
struct WorkflowHandleTestHooks {
    before_publish: Option<Arc<dyn Fn() + Send + Sync>>,
    before_reset: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub(crate) struct WorkflowHandles {
    state: Mutex<WorkflowHandleState>,
}

pub(crate) struct WorkflowHandleReservation {
    registry: Arc<WorkflowHandles>,
    generation: u64,
    handle: String,
    active: bool,
}

impl Default for WorkflowHandles {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkflowHandles {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(WorkflowHandleState::default()),
        }
    }

    /// The current handle generation. Callers key per-workflow bookkeeping
    /// (boundary-submit ledgers) on it: a re-`initialize` bumps the
    /// generation, so records from the previous connection's workflows go
    /// stale in one comparison instead of needing their own invalidation walk.
    pub(crate) fn generation(&self) -> u64 {
        self.lock_state().generation
    }

    pub(crate) fn reserve(
        self: &Arc<Self>,
    ) -> Result<WorkflowHandleReservation, WorkflowHandleError> {
        let mut state = self.lock_state();
        if state.reservations.len() >= MAX_WORKFLOW_RESERVATIONS {
            return Err(WorkflowHandleError::CapacityExhausted);
        }

        let handle = loop {
            let handle = format!("wf_{}", Uuid::new_v4().simple());
            if !state.reservations.contains(&handle) && !state.bindings.contains_key(&handle) {
                break handle;
            }
        };
        state.reservations.insert(handle.clone());
        Ok(WorkflowHandleReservation {
            registry: Arc::clone(self),
            generation: state.generation,
            handle,
            active: true,
        })
    }

    pub(crate) fn reset(&self) {
        let mut state = self.lock_state();
        state.generation = state
            .generation
            .checked_add(1)
            .expect("workflow handle generation overflow");
        state.bindings.clear();
        state.lru.clear();
        #[cfg(test)]
        if let Some(hook) = &state.test_hooks.before_reset {
            hook();
        }
    }

    pub(crate) fn resolve(&self, handle: &str) -> Result<WorkflowBinding, WorkflowHandleError> {
        if !parse_workflow_handle(handle) {
            return Err(WorkflowHandleError::Malformed);
        }

        let mut state = self.lock_state();
        let binding = state
            .bindings
            .get(handle)
            .cloned()
            .ok_or(WorkflowHandleError::Unknown)?;
        let position = state
            .lru
            .iter()
            .position(|current| current == handle)
            .expect("every workflow binding has an LRU entry");
        state.lru.remove(position);
        state.lru.push_back(handle.to_owned());
        Ok(binding)
    }

    /// Replaces the advertised opaque workflow scope with its retained ids.
    ///
    /// Only tools in `WORKFLOW_SCOPE_TOOLS` opt into this transformation. All
    /// other calls stay untouched so their existing schema remains the sole
    /// source of rejection for an unexpected `workflowHandle` field.
    pub(crate) fn normalize_arguments(
        &self,
        tool: &str,
        arguments: &Value,
    ) -> Result<Value, WorkflowHandleError> {
        let Some(scope) = workflow_scope_for_tool(tool) else {
            return Ok(arguments.clone());
        };
        let Some(object) = arguments.as_object() else {
            return Ok(arguments.clone());
        };
        let Some(handle) = object.get("workflowHandle") else {
            return Ok(arguments.clone());
        };

        if ["sessionId", "pageId", "workflowId"]
            .iter()
            .any(|key| object.contains_key(*key))
        {
            return Err(WorkflowHandleError::BindingConflict);
        }

        let handle = handle.as_str().ok_or(WorkflowHandleError::Unknown)?;
        let binding = self.resolve(handle).map_err(|error| match error {
            WorkflowHandleError::BindingConflict => WorkflowHandleError::BindingConflict,
            WorkflowHandleError::CapacityExhausted
            | WorkflowHandleError::GenerationChanged
            | WorkflowHandleError::SupervisorLost
            | WorkflowHandleError::Unknown
            | WorkflowHandleError::Malformed => WorkflowHandleError::Unknown,
        })?;

        let mut normalized = object.clone();
        normalized.remove("workflowHandle");
        normalized.insert(
            "sessionId".to_owned(),
            serde_json::json!(binding.session_id),
        );
        normalized.insert("pageId".to_owned(), serde_json::json!(binding.page_id));
        if scope == WorkflowScope::SessionPageWorkflow {
            normalized.insert(
                "workflowId".to_owned(),
                serde_json::json!(binding.workflow_id),
            );
        }
        Ok(Value::Object(normalized))
    }

    pub(crate) fn remove_session(&self, session_id: &types::SessionId) -> usize {
        self.remove_bindings(|binding| &binding.session_id == session_id)
    }

    pub(crate) fn remove_page(
        &self,
        session_id: &types::SessionId,
        page_id: &types::PageId,
    ) -> usize {
        self.remove_bindings(|binding| {
            &binding.session_id == session_id && &binding.page_id == page_id
        })
    }

    pub(crate) fn reconcile_sessions(&self, sessions: &[types::SessionState]) -> usize {
        let mut state = self.lock_state();
        // At most one id for each retained binding enters this set. Filtering
        // the authoritative response against those candidates keeps this
        // reconciliation allocation bounded even if the response is large.
        let bound_session_ids = state
            .bindings
            .values()
            .map(|binding| binding.session_id.clone())
            .collect::<HashSet<_>>();
        let visible_session_ids = sessions
            .iter()
            .filter(|session| bound_session_ids.contains(&session.id))
            .map(|session| session.id.clone())
            .collect::<HashSet<_>>();
        let removed_handles = state
            .bindings
            .iter()
            .filter(|(_, binding)| !visible_session_ids.contains(&binding.session_id))
            .map(|(handle, _)| handle.clone())
            .collect::<BTreeSet<_>>();
        if removed_handles.is_empty() {
            return 0;
        }
        state
            .bindings
            .retain(|handle, _| !removed_handles.contains(handle));
        state.lru.retain(|handle| !removed_handles.contains(handle));
        removed_handles.len()
    }

    fn remove_bindings(&self, predicate: impl Fn(&WorkflowBinding) -> bool) -> usize {
        let mut state = self.lock_state();
        let removed_handles = state
            .bindings
            .iter()
            .filter(|(_, binding)| predicate(binding))
            .map(|(handle, _)| handle.clone())
            .collect::<BTreeSet<_>>();
        if removed_handles.is_empty() {
            return 0;
        }
        state
            .bindings
            .retain(|handle, _| !removed_handles.contains(handle));
        state.lru.retain(|handle| !removed_handles.contains(handle));
        removed_handles.len()
    }

    fn lock_state(&self) -> MutexGuard<'_, WorkflowHandleState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl WorkflowHandleReservation {
    pub(crate) fn handle(&self) -> &str {
        &self.handle
    }

    pub(crate) fn generation_is_current(&self) -> bool {
        self.registry.lock_state().generation == self.generation
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn publish_with_supervisor(
        mut self,
        binding: WorkflowBinding,
        published_sender: oneshot::Sender<()>,
    ) -> Result<(), WorkflowHandleError> {
        let mut state = self.registry.lock_state();
        #[cfg(test)]
        if let Some(hook) = &state.test_hooks.before_publish {
            hook();
        }
        if state.generation != self.generation {
            state.reservations.remove(&self.handle);
            self.active = false;
            return Err(WorkflowHandleError::GenerationChanged);
        }
        if binding.generation != self.generation {
            state.reservations.remove(&self.handle);
            self.active = false;
            return Err(WorkflowHandleError::BindingConflict);
        }
        if !state.reservations.remove(&self.handle) {
            self.active = false;
            return Err(WorkflowHandleError::Unknown);
        }
        self.active = false;
        if state.bindings.contains_key(&self.handle) {
            return Err(WorkflowHandleError::BindingConflict);
        }

        let evicted = if state.bindings.len() == MAX_WORKFLOW_HANDLES {
            let handle = state
                .lru
                .pop_front()
                .expect("a full workflow binding map has an LRU entry");
            let binding = state
                .bindings
                .remove(&handle)
                .expect("every workflow LRU entry has a binding");
            Some((handle, binding))
        } else {
            None
        };
        state.bindings.insert(self.handle.clone(), binding);
        state.lru.push_back(self.handle.clone());

        if published_sender.send(()).is_ok() {
            return Ok(());
        }

        state.bindings.remove(&self.handle);
        let provisional = state
            .lru
            .pop_back()
            .expect("published workflow handle has an LRU entry");
        debug_assert_eq!(provisional, self.handle);
        if let Some((handle, binding)) = evicted {
            state.bindings.insert(handle.clone(), binding);
            state.lru.push_front(handle);
        }
        Err(WorkflowHandleError::SupervisorLost)
    }
}

impl Drop for WorkflowHandleReservation {
    fn drop(&mut self) {
        if self.active {
            self.registry.lock_state().reservations.remove(&self.handle);
        }
    }
}

/// Allocation-free validation for the opaque workflow-handle wire form.
///
/// Callers intentionally collapse malformed and unknown handles into the same
/// public response, so this helper exposes no information about registry state.
pub(crate) fn parse_workflow_handle(handle: &str) -> bool {
    handle.len() == 35
        && handle.starts_with("wf_")
        && handle[3..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        sync::{mpsc, Arc, Barrier},
        thread,
    };

    use chrono::Utc;
    use serde_json::json;
    use tokio::sync::oneshot;

    use super::*;

    fn registry() -> Arc<WorkflowHandles> {
        Arc::new(WorkflowHandles::new())
    }

    fn binding(
        generation: u64,
        session_id: types::SessionId,
        page_id: types::PageId,
        workflow_id: types::WorkflowId,
    ) -> WorkflowBinding {
        WorkflowBinding {
            generation,
            session_id,
            page_id,
            workflow_id,
        }
    }

    fn session_id(value: u128) -> types::SessionId {
        types::SessionId(uuid::Uuid::from_u128(value))
    }

    fn page_id(value: u128) -> types::PageId {
        types::PageId(uuid::Uuid::from_u128(value))
    }

    fn workflow_id(value: u128) -> types::WorkflowId {
        types::WorkflowId(uuid::Uuid::from_u128(value))
    }

    fn publish(
        reservation: WorkflowHandleReservation,
        binding: WorkflowBinding,
    ) -> (String, oneshot::Receiver<()>) {
        let handle = reservation.handle().to_owned();
        let (published_sender, published_receiver) = oneshot::channel();
        reservation
            .publish_with_supervisor(binding, published_sender)
            .unwrap();
        (handle, published_receiver)
    }

    fn session(id: types::SessionId) -> types::SessionState {
        types::SessionState {
            id,
            profile: "test".into(),
            proxy: None,
            page_ids: Vec::new(),
            created_at: Utc::now(),
            last_used_at: Utc::now(),
            execution_policy: types::ExecutionPolicy::default(),
            zigzagzig: false,
        }
    }

    fn lru(registry: &WorkflowHandles) -> Vec<String> {
        registry.lock_state().lru.iter().cloned().collect()
    }

    fn assert_lru_matches_bindings(registry: &WorkflowHandles) {
        let state = registry.lock_state();
        let lru_handles = state.lru.iter().cloned().collect::<BTreeSet<_>>();
        let binding_handles = state.bindings.keys().cloned().collect::<BTreeSet<_>>();

        assert_eq!(state.lru.len(), state.bindings.len());
        assert_eq!(lru_handles, binding_handles);
    }

    #[test]
    fn normalize_arguments_substitutes_the_exact_retained_scope_for_every_allowlisted_tool() {
        let registry = registry();
        let reservation = registry.reserve().unwrap();
        let expected = binding(
            reservation.generation,
            session_id(1),
            page_id(2),
            workflow_id(3),
        );
        let (handle, _) = publish(reservation, expected.clone());

        for (tool, scope) in WORKFLOW_SCOPE_TOOLS {
            let normalized = registry
                .normalize_arguments(tool, &json!({"workflowHandle": handle}))
                .unwrap_or_else(|error| panic!("{tool}: {error:?}"));
            assert_eq!(normalized["sessionId"], json!(expected.session_id));
            assert_eq!(normalized["pageId"], json!(expected.page_id));
            assert_eq!(normalized.get("workflowHandle"), None);
            assert_eq!(
                normalized.get("workflowId"),
                matches!(scope, WorkflowScope::SessionPageWorkflow)
                    .then(|| json!(expected.workflow_id))
                    .as_ref(),
                "{tool}"
            );
        }
    }

    #[test]
    fn normalize_arguments_rejects_mixed_handle_and_explicit_scope_before_validation() {
        let registry = registry();
        let reservation = registry.reserve().unwrap();
        let generation = reservation.generation;
        let (handle, _) = publish(
            reservation,
            binding(generation, session_id(1), page_id(2), workflow_id(3)),
        );

        for explicit_scope in [
            json!({"sessionId": session_id(4)}),
            json!({"pageId": page_id(5)}),
            json!({"workflowId": workflow_id(6)}),
        ] {
            let mut arguments = serde_json::Map::new();
            arguments.insert("workflowHandle".to_owned(), json!(handle));
            arguments.extend(explicit_scope.as_object().unwrap().clone());
            assert_eq!(
                registry.normalize_arguments("navigate", &json!(arguments)),
                Err(WorkflowHandleError::BindingConflict)
            );
        }
    }

    #[test]
    fn normalize_arguments_collapses_unknown_reset_and_malformed_handles_to_unknown() {
        let registry = registry();
        let reservation = registry.reserve().unwrap();
        let generation = reservation.generation;
        let (reset_handle, _) = publish(
            reservation,
            binding(generation, session_id(1), page_id(2), workflow_id(3)),
        );
        registry.reset();

        for handle in [
            "wf_ffffffffffffffffffffffffffffffff",
            reset_handle.as_str(),
            "wf_0123456789abcdef0123456789abcde",
            "xx_0123456789abcdef0123456789abcdef",
            "wf_0123456789abcdef0123456789abcdeF",
            "wf_0123456789abcdef0123456789abcdeg",
        ] {
            assert_eq!(
                registry.normalize_arguments("navigate", &json!({"workflowHandle": handle})),
                Err(WorkflowHandleError::Unknown),
                "{handle}"
            );
        }
    }

    #[test]
    fn normalize_arguments_leaves_explicit_and_non_allowlisted_calls_unchanged() {
        let registry = registry();
        let explicit = json!({
            "sessionId": session_id(1),
            "pageId": page_id(2),
            "workflowId": workflow_id(3),
            "url": "https://example.test/"
        });
        assert_eq!(
            registry.normalize_arguments("navigate", &explicit),
            Ok(explicit.clone())
        );

        let not_allowlisted = json!({"workflowHandle":"wf_0123456789abcdef0123456789abcdef"});
        assert_eq!(
            registry.normalize_arguments("session_create", &not_allowlisted),
            Ok(not_allowlisted)
        );
    }

    #[test]
    fn workflow_scope_tools_are_the_sorted_unique_normalization_set() {
        let names = WORKFLOW_SCOPE_TOOLS
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        assert!(names.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(names.len(), names.iter().collect::<BTreeSet<_>>().len());
        assert!(names
            .iter()
            .all(|name| workflow_scope_for_tool(name).is_some()));
    }

    #[test]
    fn reservation_commits_an_opaque_handle_to_its_exact_binding() {
        let registry = registry();
        let reservation = registry.reserve().unwrap();
        let expected = binding(
            reservation.generation,
            session_id(1),
            page_id(2),
            workflow_id(3),
        );

        let (handle, mut published) = publish(reservation, expected.clone());

        assert!(handle.starts_with("wf_"));
        assert_eq!(handle.len(), 35);
        assert_eq!(registry.resolve(&handle), Ok(expected));
        assert_eq!(published.try_recv(), Ok(()));
    }

    #[test]
    fn handle_parser_accepts_only_the_lowercase_wire_form() {
        assert!(parse_workflow_handle("wf_0123456789abcdef0123456789abcdef"));
        for malformed in [
            "wf_0123456789abcdef0123456789abcde",
            "wf_0123456789abcdef0123456789abcdef0",
            "wf_0123456789abcdef0123456789abcdeF",
            "wf_0123456789abcdef0123456789abcdeg",
            "xx_0123456789abcdef0123456789abcdef",
            "wf_0123456789abcdef0123456789abcdefé",
        ] {
            assert!(!parse_workflow_handle(malformed), "{malformed}");
        }
    }

    #[test]
    fn dropping_an_uncommitted_reservation_frees_its_slot() {
        let registry = registry();
        let reservation = registry.reserve().unwrap();
        let handle = reservation.handle().to_owned();
        drop(reservation);

        assert!(!registry.lock_state().reservations.contains(&handle));
        assert!(registry.reserve().is_ok());
    }

    #[test]
    fn outstanding_reservations_hit_the_bound_before_any_binding_is_committed() {
        let registry = registry();
        let reservations = (0..MAX_WORKFLOW_RESERVATIONS)
            .map(|_| registry.reserve().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            reservations
                .iter()
                .map(|reservation| reservation.handle().to_owned())
                .collect::<BTreeSet<_>>()
                .len(),
            MAX_WORKFLOW_RESERVATIONS
        );
        assert!(matches!(
            registry.reserve(),
            Err(WorkflowHandleError::CapacityExhausted)
        ));
        assert!(registry.lock_state().bindings.is_empty());
        drop(reservations);

        for index in 0..MAX_WORKFLOW_HANDLES {
            let reservation = registry.reserve().unwrap();
            let generation = reservation.generation;
            publish(
                reservation,
                binding(
                    generation,
                    session_id(100 + index as u128),
                    page_id(200 + index as u128),
                    workflow_id(300 + index as u128),
                ),
            );
        }
        let reservations = (0..MAX_WORKFLOW_RESERVATIONS)
            .map(|_| registry.reserve().unwrap())
            .collect::<Vec<_>>();

        assert!(matches!(
            registry.reserve(),
            Err(WorkflowHandleError::CapacityExhausted)
        ));
        assert_eq!(registry.lock_state().bindings.len(), MAX_WORKFLOW_HANDLES);
        drop(reservations);
    }

    #[test]
    fn reset_invalidates_committed_handles_but_keeps_outstanding_reservations_counted() {
        let registry = registry();
        let committed = registry.reserve().unwrap();
        let expected = binding(
            committed.generation,
            session_id(10),
            page_id(11),
            workflow_id(12),
        );
        let (handle, _) = publish(committed, expected);
        let outstanding = registry.reserve().unwrap();

        registry.reset();

        assert_eq!(registry.resolve(&handle), Err(WorkflowHandleError::Unknown));
        assert!(lru(&registry).is_empty());
        assert!(registry
            .lock_state()
            .reservations
            .contains(outstanding.handle()));
    }

    #[test]
    fn stale_reservation_cannot_publish_after_reset_and_releases_only_its_own_slot() {
        let registry = registry();
        let stale = registry.reserve().unwrap();
        let fresh = registry.reserve().unwrap();
        let stale_handle = stale.handle().to_owned();
        let fresh_handle = fresh.handle().to_owned();
        let stale_binding = binding(
            stale.generation,
            session_id(20),
            page_id(21),
            workflow_id(22),
        );

        registry.reset();
        let (sender, _receiver) = oneshot::channel();

        assert!(!stale.generation_is_current());
        assert_eq!(
            stale.publish_with_supervisor(stale_binding, sender),
            Err(WorkflowHandleError::GenerationChanged)
        );
        let state = registry.lock_state();
        assert!(!state.reservations.contains(&stale_handle));
        assert!(state.reservations.contains(&fresh_handle));
        drop(state);
        drop(fresh);
    }

    #[test]
    fn handle_text_is_not_an_authority_source() {
        let registry = registry();
        let reservation = registry.reserve().unwrap();
        let expected = binding(
            reservation.generation,
            session_id(30),
            page_id(31),
            workflow_id(32),
        );
        let (handle, _) = publish(reservation, expected.clone());
        let fabricated = format!("wf_{}", expected.workflow_id.0.simple());

        assert_eq!(registry.resolve(&handle), Ok(expected));
        assert_eq!(
            registry.resolve(&fabricated),
            Err(WorkflowHandleError::Unknown)
        );
    }

    #[test]
    fn removing_sessions_and_pages_evicts_only_the_matching_bindings() {
        let registry = registry();
        let session_a = session_id(40);
        let session_b = session_id(41);
        let page_a = page_id(50);
        let page_b = page_id(51);
        let (a_page_a, _) = {
            let reservation = registry.reserve().unwrap();
            let generation = reservation.generation;
            publish(
                reservation,
                binding(
                    generation,
                    session_a.clone(),
                    page_a.clone(),
                    workflow_id(60),
                ),
            )
        };
        let (a_page_b, _) = {
            let reservation = registry.reserve().unwrap();
            let generation = reservation.generation;
            publish(
                reservation,
                binding(
                    generation,
                    session_a.clone(),
                    page_b.clone(),
                    workflow_id(61),
                ),
            )
        };
        let (b_page_a, _) = {
            let reservation = registry.reserve().unwrap();
            let generation = reservation.generation;
            publish(
                reservation,
                binding(
                    generation,
                    session_b.clone(),
                    page_a.clone(),
                    workflow_id(62),
                ),
            )
        };

        assert_eq!(registry.remove_page(&session_a, &page_a), 1);
        assert_eq!(
            registry.resolve(&a_page_a),
            Err(WorkflowHandleError::Unknown)
        );
        assert_lru_matches_bindings(&registry);
        assert!(registry.resolve(&a_page_b).is_ok());
        assert!(registry.resolve(&b_page_a).is_ok());
        assert_eq!(registry.remove_session(&session_a), 1);
        assert_eq!(
            registry.resolve(&a_page_b),
            Err(WorkflowHandleError::Unknown)
        );
        assert_lru_matches_bindings(&registry);
        assert!(registry.resolve(&b_page_a).is_ok());
    }

    #[test]
    fn reconciliation_uses_authoritative_sessions_without_trusting_stale_page_ids() {
        let registry = registry();
        let live_session = session_id(70);
        let gone_session = session_id(71);
        let live_page = page_id(80);
        let stale_page = page_id(81);
        let (live_handle, _) = {
            let reservation = registry.reserve().unwrap();
            let generation = reservation.generation;
            publish(
                reservation,
                binding(generation, live_session.clone(), live_page, workflow_id(90)),
            )
        };
        let (stale_page_handle, _) = {
            let reservation = registry.reserve().unwrap();
            let generation = reservation.generation;
            publish(
                reservation,
                binding(
                    generation,
                    live_session.clone(),
                    stale_page.clone(),
                    workflow_id(91),
                ),
            )
        };
        let (gone_handle, _) = {
            let reservation = registry.reserve().unwrap();
            let generation = reservation.generation;
            publish(
                reservation,
                binding(generation, gone_session, page_id(82), workflow_id(92)),
            )
        };
        let mut authoritative = session(live_session);
        authoritative.page_ids.push(stale_page);

        assert_eq!(registry.reconcile_sessions(&[authoritative]), 1);
        assert!(registry.resolve(&live_handle).is_ok());
        assert!(registry.resolve(&stale_page_handle).is_ok());
        assert_eq!(
            registry.resolve(&gone_handle),
            Err(WorkflowHandleError::Unknown)
        );
        assert_lru_matches_bindings(&registry);
    }

    #[test]
    fn failed_or_dropped_start_does_not_evict_committed_handles_and_supervisor_failure_rolls_back_lru(
    ) {
        let registry = registry();
        let mut handles = Vec::new();
        for index in 0..MAX_WORKFLOW_HANDLES {
            let reservation = registry.reserve().unwrap();
            let expected = binding(
                reservation.generation,
                session_id(400 + index as u128),
                page_id(500 + index as u128),
                workflow_id(600 + index as u128),
            );
            handles.push(publish(reservation, expected).0);
        }
        let lru_before = lru(&registry);

        let outstanding_start = registry.reserve().unwrap();
        assert!(handles
            .iter()
            .all(|handle| registry.resolve(handle).is_ok()));
        assert_eq!(lru(&registry), lru_before);
        drop(outstanding_start);

        let reservation = registry.reserve().unwrap();
        let expected = binding(
            reservation.generation,
            session_id(690),
            page_id(691),
            workflow_id(692),
        );
        let (new_handle, _) = publish(reservation, expected.clone());

        assert_eq!(
            registry.resolve(&handles[0]),
            Err(WorkflowHandleError::Unknown)
        );
        assert_eq!(registry.resolve(&new_handle), Ok(expected));
        let mut lru_after_success = lru_before[1..].to_vec();
        lru_after_success.push(new_handle);
        assert_eq!(lru(&registry), lru_after_success);
        let mut current_handles = handles[1..].to_vec();
        current_handles.push(lru_after_success.last().unwrap().clone());

        let reservation = registry.reserve().unwrap();
        let publish_binding = binding(
            reservation.generation,
            session_id(700),
            page_id(701),
            workflow_id(702),
        );
        let (sender, receiver) = oneshot::channel();
        drop(receiver);

        assert_eq!(
            reservation.publish_with_supervisor(publish_binding, sender),
            Err(WorkflowHandleError::SupervisorLost)
        );
        assert_eq!(lru(&registry), lru_after_success);
        assert!(current_handles
            .iter()
            .all(|handle| registry.resolve(handle).is_ok()));
    }

    #[test]
    fn resolving_and_eviction_keep_the_lru_queue_in_sync_with_bindings() {
        let registry = registry();
        let mut handles = Vec::new();
        for index in 0..3 {
            let reservation = registry.reserve().unwrap();
            let expected = binding(
                reservation.generation,
                session_id(800 + index as u128),
                page_id(810 + index as u128),
                workflow_id(820 + index as u128),
            );
            handles.push(publish(reservation, expected).0);
        }

        assert_eq!(lru(&registry), handles);
        registry.resolve(&handles[0]).unwrap();
        assert_eq!(
            lru(&registry),
            vec![handles[1].clone(), handles[2].clone(), handles[0].clone()]
        );
        let binding = registry.resolve(&handles[1]).unwrap();
        assert_eq!(
            registry.remove_page(&binding.session_id, &binding.page_id),
            1
        );
        assert_eq!(lru(&registry), vec![handles[2].clone(), handles[0].clone()]);
        assert_lru_matches_bindings(&registry);
        registry.reset();
        assert!(lru(&registry).is_empty());
        assert!(registry.lock_state().bindings.is_empty());
        assert_lru_matches_bindings(&registry);
    }

    #[test]
    fn reset_cannot_bypass_the_concurrent_reservation_bound() {
        let registry = registry();
        let mut reservations = (0..MAX_WORKFLOW_RESERVATIONS)
            .map(|_| registry.reserve().unwrap())
            .collect::<Vec<_>>();

        for _ in 0..3 {
            registry.reset();
            assert!(matches!(
                registry.reserve(),
                Err(WorkflowHandleError::CapacityExhausted)
            ));
        }
        drop(reservations.pop());
        let replacement = registry.reserve();
        assert!(replacement.is_ok());
        drop(replacement);
        drop(reservations);
    }

    #[test]
    fn reset_before_publication_prevents_commit_signal_and_old_binding_resurrection() {
        let registry = registry();
        let reservation = registry.reserve().unwrap();
        let handle = reservation.handle().to_owned();
        let binding = binding(
            reservation.generation,
            session_id(900),
            page_id(901),
            workflow_id(902),
        );
        let (sender, mut receiver) = oneshot::channel();

        registry.reset();
        assert_eq!(
            reservation.publish_with_supervisor(binding, sender),
            Err(WorkflowHandleError::GenerationChanged)
        );
        assert!(receiver.try_recv().is_err());
        assert_eq!(registry.resolve(&handle), Err(WorkflowHandleError::Unknown));
    }

    #[test]
    fn publication_and_reset_serialize_in_both_mutex_orders() {
        let publish_first = registry();
        let reservation = publish_first.reserve().unwrap();
        let handle = reservation.handle().to_owned();
        let publish_binding = binding(
            reservation.generation,
            session_id(910),
            page_id(911),
            workflow_id(912),
        );
        let (publish_entered_tx, publish_entered_rx) = mpsc::channel();
        let publish_release = Arc::new(Barrier::new(2));
        {
            let mut state = publish_first.lock_state();
            let publish_release = Arc::clone(&publish_release);
            state.test_hooks.before_publish = Some(Arc::new(move || {
                publish_entered_tx.send(()).unwrap();
                publish_release.wait();
            }));
        }
        let (published_sender, mut published_receiver) = oneshot::channel();
        let publisher = thread::spawn(move || {
            reservation.publish_with_supervisor(publish_binding, published_sender)
        });
        publish_entered_rx.recv().unwrap();
        let reset_registry = Arc::clone(&publish_first);
        let (reset_started_tx, reset_started_rx) = mpsc::channel();
        let resetter = thread::spawn(move || {
            reset_started_tx.send(()).unwrap();
            reset_registry.reset();
        });
        reset_started_rx.recv().unwrap();

        publish_release.wait();
        assert_eq!(publisher.join().unwrap(), Ok(()));
        resetter.join().unwrap();
        assert_eq!(published_receiver.try_recv(), Ok(()));
        assert_eq!(
            publish_first.resolve(&handle),
            Err(WorkflowHandleError::Unknown)
        );

        let reset_first = registry();
        let reservation = reset_first.reserve().unwrap();
        let handle = reservation.handle().to_owned();
        let reset_binding = binding(
            reservation.generation,
            session_id(920),
            page_id(921),
            workflow_id(922),
        );
        let (reset_entered_tx, reset_entered_rx) = mpsc::channel();
        let reset_release = Arc::new(Barrier::new(2));
        {
            let mut state = reset_first.lock_state();
            let reset_release = Arc::clone(&reset_release);
            state.test_hooks.before_reset = Some(Arc::new(move || {
                reset_entered_tx.send(()).unwrap();
                reset_release.wait();
            }));
        }
        let reset_registry = Arc::clone(&reset_first);
        let resetter = thread::spawn(move || reset_registry.reset());
        reset_entered_rx.recv().unwrap();
        let (published_sender, mut published_receiver) = oneshot::channel();
        let (publish_started_tx, publish_started_rx) = mpsc::channel();
        let publisher = thread::spawn(move || {
            publish_started_tx.send(()).unwrap();
            reservation.publish_with_supervisor(reset_binding, published_sender)
        });
        publish_started_rx.recv().unwrap();

        reset_release.wait();
        resetter.join().unwrap();
        assert_eq!(
            publisher.join().unwrap(),
            Err(WorkflowHandleError::GenerationChanged)
        );
        assert!(published_receiver.try_recv().is_err());
        assert_eq!(
            reset_first.resolve(&handle),
            Err(WorkflowHandleError::Unknown)
        );
    }
}
