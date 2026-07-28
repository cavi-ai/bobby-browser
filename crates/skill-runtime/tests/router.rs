use std::{collections::BTreeMap, sync::Arc};

use chrono::{Duration, Utc};
use skill_runtime::{
    SkillCommandRouter, SkillContext, SkillRegistry, SkillStateStore, SkillZigZagZigController,
};
use types::{SessionId, SkillBrowserEngine, SkillCapability, SkillOutcome, SkillSessionState};

fn session_state(session_id: SessionId) -> SkillSessionState {
    SkillSessionState::new(
        session_id,
        BTreeMap::new(),
        None,
        None,
        None,
        None,
        None,
        Vec::new(),
        Vec::new(),
        Utc::now() + Duration::minutes(1),
    )
    .unwrap()
}

#[tokio::test]
async fn public_router_parses_registers_and_activates_zigzagzig_durably() {
    let session_id = SessionId::new();
    let store = Arc::new(SkillStateStore::new());
    store.insert(session_state(session_id.clone())).unwrap();
    let zigzagzig = Arc::new(SkillZigZagZigController::new(
        Arc::clone(&store),
        1_000,
        [SkillBrowserEngine::Chromium],
    ));
    let mut registry = SkillRegistry::new();
    registry.register(zigzagzig.clone()).unwrap();
    let router = SkillCommandRouter::new(registry);
    let context = SkillContext::new([
        SkillCapability::EngineSelection,
        SkillCapability::ProfilePersistence,
    ])
    .with_session(session_id.clone());

    let receipt = router.execute(" /zigzagzig\trun ", &context).await.unwrap();

    assert_eq!(receipt.alias, "/zigzagzig");
    assert_eq!(receipt.skill_name, "SkillZigZagZig");
    assert_eq!(receipt.skill_version, "1.0.0");
    assert!(matches!(receipt.outcome, SkillOutcome::Applied { .. }));
    assert_eq!(
        store
            .get(&session_id)
            .unwrap()
            .active_versions
            .get("SkillZigZagZig")
            .map(String::as_str),
        Some("1.0.0")
    );
    let strategy = zigzagzig.strategy(&session_id).await.unwrap();
    assert_eq!(strategy.session_state().session_id, session_id);
    assert_eq!(
        strategy.compatible_engines(),
        &[SkillBrowserEngine::Chromium]
    );
}

#[tokio::test]
async fn public_router_rejects_unknown_commands_before_registry_execution() {
    let router = SkillCommandRouter::new(SkillRegistry::new());
    let error = router
        .execute("/not-a-bobby-skill on", &SkillContext::new([]))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "unknown skill command: /not-a-bobby-skill"
    );
}
