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
