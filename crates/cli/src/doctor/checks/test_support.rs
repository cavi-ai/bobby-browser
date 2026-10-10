use super::*;

pub(super) fn context(root: &std::path::Path) -> DoctorContext {
    DoctorContext {
        config_path: root.join("config.toml"),
        bootstrap_cli: None,
        bootstrap_path: None,
        bootstrap_path_for_heal: None,
        config: None,
        selection: None,
        unreachable_bidi: vec![],
        check_health: false,
        profile: None,
    }
}

#[test]
fn prerequisites_do_not_run_health_or_engine_rows_without_required_context() {
    let root = tempfile::tempdir().unwrap();
    let mut context = context(root.path());
    assert!(Needs::Always.satisfied(&context));
    assert!(!Needs::Config.satisfied(&context));
    context.check_health = true;
    context.profile = Some(crate::deployment_profiles::DeploymentProfile::Desktop);
    assert!(!Needs::HealthAndConfig.satisfied(&context));
    assert!(!Needs::ProfileAndConfig.satisfied(&context));
    context.config = Some(AppConfig::default());
    assert!(Needs::Config.satisfied(&context));
    assert!(Needs::HealthAndConfig.satisfied(&context));
    assert!(Needs::ProfileAndConfig.satisfied(&context));
    assert!(!Needs::ConfigAndSelection.satisfied(&context));
    context.selection = Some(config::BrowserSelectionConfig::default());
    assert!(Needs::ConfigAndSelection.satisfied(&context));
    context.check_health = false;
    assert!(!Needs::HealthAndConfig.satisfied(&context));
}
