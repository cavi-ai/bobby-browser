//! cli path diagnostics and probes.
use super::DoctorContext;
use crate::doctor::{onboarding, DoctorReport, Result};

pub(super) fn cli_path(_context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    record_cli_path_check(report);

    Ok(())
}

pub(in crate::doctor) fn record_cli_path_check(report: &mut DoctorReport) {
    let Ok(canonical) = onboarding::canonical_cli_path() else {
        return;
    };
    match onboarding::path_cli() {
        None => report.warn(
            "cli-path",
            format!(
                "bobby is not on PATH; hosts launch {}",
                canonical.display()
            ),
        ),
        Some(path_hit) if onboarding::same_cli(&path_hit, &canonical) => {
            report.ok("cli-path", canonical.display().to_string())
        }
        Some(path_hit) => report.warn(
            "cli-path",
            format!(
                "PATH `bobby` is {}; hosts launch {}. Put the install dir first on PATH or run that binary",
                path_hit.display(),
                canonical.display()
            ),
        ),
    }
}
