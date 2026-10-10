//! host configuration diagnostics and probes.
use super::DoctorContext;
use crate::doctor::{onboarding, DoctorReport, Path, Result};
#[cfg(test)]
use crate::doctor::{repair_host_configs, DoctorFixStatus, DoctorStatus};

pub(super) fn host_configuration(
    _context: &mut DoctorContext,
    report: &mut DoctorReport,
) -> Result<()> {
    record_host_config_checks(report, &std::env::current_dir()?);
    Ok(())
}

pub(in crate::doctor) fn record_host_config_checks(report: &mut DoctorReport, project_root: &Path) {
    match onboarding::configured_host_statuses(project_root) {
        Ok(statuses) => {
            for (kind, path, status) in statuses {
                let name = format!("host-{}", kind.name());
                match status {
                    onboarding::HostConfigStatus::Missing => {}
                    onboarding::HostConfigStatus::Current => {
                        report.ok(&name, path.display().to_string())
                    }
                    onboarding::HostConfigStatus::Drifted => {
                        report.fail(&name, onboarding::host_config_drift_detail(kind, &path))
                    }
                    onboarding::HostConfigStatus::Invalid => {
                        report.fail(&name, format!("{} is not valid host JSON", path.display()))
                    }
                }
            }
        }
        Err(error) => report.fail("host-config", format!("{error:#}")),
    }
}

#[cfg(test)]
mod host_config_tests {
    use super::*;

    #[test]
    fn doctor_reports_and_repairs_drifted_host_entries() {
        let _lock = onboarding::INSTALL_ENV_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let path =
            onboarding::merge_host_config(onboarding::HostKind::Claude, root.path()).unwrap();
        let mut config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        config["mcpServers"]["bobby-browser"]["args"] = serde_json::json!(["serve"]);
        std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();

        let mut report = DoctorReport::default();
        record_host_config_checks(&mut report, root.path());
        assert_eq!(
            report.check("host-claude").unwrap().status,
            DoctorStatus::Fail
        );
        assert!(
            report
                .check("host-claude")
                .unwrap()
                .detail
                .contains("fix: bobby doctor --fix"),
            "{}",
            report.check("host-claude").unwrap().detail
        );

        let actions = repair_host_configs(root.path());
        let action = actions
            .iter()
            .find(|action| action.name == "host-claude")
            .unwrap();
        assert_eq!(action.status, DoctorFixStatus::Fixed);

        let mut repaired = DoctorReport::default();
        record_host_config_checks(&mut repaired, root.path());
        assert_eq!(
            repaired.check("host-claude").unwrap().status,
            DoctorStatus::Ok
        );
    }
}
