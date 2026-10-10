//! deployment profile diagnostics and probes.
use super::DoctorContext;
use crate::doctor::{DoctorReport, Path, Result};

pub(super) fn deployment_profile(
    context: &mut DoctorContext,
    report: &mut DoctorReport,
) -> Result<()> {
    let bootstrap_path = context.bootstrap_path.clone();
    let config = &context.config;
    let profile = context.profile;
    if let (Some(profile), Some(config)) = (profile, config.as_ref()) {
        let profile_name = profile.contract().name;
        report.ok("deployment-profile", profile_name.to_string());
        for check in crate::deployment_profiles::evaluate(
            profile,
            config,
            bootstrap_path.as_deref().is_some_and(Path::exists),
        ) {
            let name = format!("deployment-{}", check.name);
            if check.ok {
                report.ok(&name, check.detail);
            } else {
                report.fail(&name, check.detail);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::context;
    use super::*;
    use crate::{deployment_profiles::DeploymentProfile, doctor::DoctorStatus};

    #[test]
    fn desktop_row_reports_missing_auth_and_public_bind_then_accepts_valid_context() {
        let root = tempfile::tempdir().unwrap();
        let mut context = context(root.path());
        let mut report = DoctorReport::default();
        deployment_profile(&mut context, &mut report).unwrap();
        assert!(report.checks.is_empty());
        let mut config = config::AppConfig::default();
        config.server.host = "0.0.0.0".into();
        context.config = Some(config);
        context.profile = Some(DeploymentProfile::Desktop);
        deployment_profile(&mut context, &mut report).unwrap();
        assert_eq!(report.checks[0].name, "deployment-profile");
        assert_eq!(report.checks[0].detail, "desktop");
        for name in ["deployment-auth", "deployment-bind"] {
            assert_eq!(
                report
                    .checks
                    .iter()
                    .find(|check| check.name == name)
                    .unwrap()
                    .status,
                DoctorStatus::Fail
            );
        }
        let credential = root.path().join("bootstrap.env");
        std::fs::write(&credential, "fixture").unwrap();
        context.bootstrap_path = Some(credential);
        context.config.as_mut().unwrap().server.host = "127.0.0.1".into();
        let mut report = DoctorReport::default();
        deployment_profile(&mut context, &mut report).unwrap();
        assert_eq!(report.failures(), 0);
        assert_eq!(report.checks.len(), 6);
    }
}
