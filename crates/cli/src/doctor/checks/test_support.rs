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

#[cfg(unix)]
#[test]
fn storage_checks_report_legacy_skill_permissions_and_observe_the_repair() {
    use crate::doctor::{skill_permissions, DoctorFixStatus, DoctorStatus};
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let path = root
        .path()
        .join(format!("{}.skill-issuance.json", uuid::Uuid::new_v4()));
    std::fs::write(&path, b"legacy decision evidence").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut context = context(root.path());
    let mut config = AppConfig::default();
    config.storage.journal_path = root.path().join("commands.jsonl");
    config.storage.scheduler_journal_path = root.path().join("jobs.jsonl");
    config.storage.checkpoints_dir = root.path().to_path_buf();
    config.browser.artifacts_dir = root.path().to_path_buf();
    context.config = Some(config);

    let mut before = DoctorReport::default();
    storage(&mut context, &mut before).unwrap();
    let check = before
        .checks
        .iter()
        .find(|check| check.name == "storage-skill-issuance-permissions")
        .unwrap();
    assert_eq!(check.status, DoctorStatus::Fail);
    assert_eq!(
        skill_permissions::repair(root.path()).status,
        DoctorFixStatus::Fixed
    );
    let mut after = DoctorReport::default();
    storage(&mut context, &mut after).unwrap();
    let check = after
        .checks
        .iter()
        .find(|check| check.name == "storage-skill-issuance-permissions")
        .unwrap();
    assert_eq!(check.status, DoctorStatus::Ok);
    assert_eq!(std::fs::read(path).unwrap(), b"legacy decision evidence");
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
