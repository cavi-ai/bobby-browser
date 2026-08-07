use std::{
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
};

use tokio::sync::oneshot;
use uuid::Uuid;

pub(crate) const MAX_WORKFLOW_HANDLES: usize = 64;
pub(crate) const MAX_WORKFLOW_RESERVATIONS: usize = 64;

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
    }

    pub(crate) fn resolve(&self, handle: &str) -> Result<WorkflowBinding, WorkflowHandleError> {
        if !is_well_formed_handle(handle) {
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
            .filter_map(|session| {
                bound_session_ids
                    .contains(&session.id)
                    .then(|| session.id.clone())
            })
            .collect::<HashSet<_>>();
        let removed_handles = state
            .bindings
            .iter()
            .filter_map(|(handle, binding)| {
                (!visible_session_ids.contains(&binding.session_id)).then(|| handle.clone())
            })
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
            .filter_map(|(handle, binding)| predicate(binding).then(|| handle.clone()))
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

    pub(crate) fn publish_with_supervisor(
        mut self,
        binding: WorkflowBinding,
        published_sender: oneshot::Sender<()>,
    ) -> Result<(), WorkflowHandleError> {
        let mut state = self.registry.lock_state();
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

fn is_well_formed_handle(handle: &str) -> bool {
    handle.len() == 35
        && handle.starts_with("wf_")
        && handle[3..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Arc};

    use chrono::Utc;
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

        let failed_start = registry.reserve().unwrap();
        drop(failed_start);
        assert!(handles
            .iter()
            .all(|handle| registry.resolve(handle).is_ok()));
        assert_eq!(lru(&registry), lru_before);

        let reservation = registry.reserve().unwrap();
        let binding = binding(
            reservation.generation,
            session_id(700),
            page_id(701),
            workflow_id(702),
        );
        let (sender, receiver) = oneshot::channel();
        drop(receiver);

        assert_eq!(
            reservation.publish_with_supervisor(binding, sender),
            Err(WorkflowHandleError::SupervisorLost)
        );
        assert_eq!(lru(&registry), lru_before);
        assert!(handles
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
}
