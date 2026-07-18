use cdp_gateway::{IdentifierMap, RuntimeGeneration};

#[test]
fn mappings_are_connection_scoped_and_invalidated_on_worker_replacement() {
    let mut first = IdentifierMap::new();
    let second = IdentifierMap::new();
    let target = first.bind_target("session-a", "page-a", RuntimeGeneration(1));
    first.bind_cdp_session("session-a", "worker-session", RuntimeGeneration(1));
    assert!(second.resolve_target(&target).is_none());
    let events = first.invalidate_generation("session-a", RuntimeGeneration(2));
    assert_eq!(events[0].method, "Target.detachedFromTarget");
    assert_eq!(events.last().unwrap().method, "Target.targetDestroyed");
    assert!(first.resolve_target(&target).is_none());
}

#[test]
fn every_identifier_family_is_opaque_and_connection_local() {
    let mut ids = IdentifierMap::new();
    let generation = RuntimeGeneration(4);
    let values = [
        ids.bind_browser_context("s", "ctx", generation),
        ids.bind_target("s", "p", generation),
        ids.bind_cdp_session("s", "worker-session", generation),
        ids.bind_execution_context("s", "execution", generation),
        ids.bind_frame("s", "frame", generation),
        ids.bind_network_request("s", "request", generation),
        ids.bind_download("s", "download", generation),
    ];
    assert!(values.iter().all(|value| value.len() >= 32));
    assert_eq!(
        values
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        values.len()
    );
}
