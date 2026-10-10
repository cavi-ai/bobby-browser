//! configuration diagnostics and probes.
use super::DoctorContext;
use crate::doctor::{AppConfig, DoctorReport, Result};

pub(super) fn configuration(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    let config_path = context.config_path.clone();
    context.config = match AppConfig::load(&config_path) {
        Ok(mut config) => {
            crate::runtime_scopes::use_owner_address(&mut config);
            let source = if config_path.exists() {
                config_path.display().to_string()
            } else {
                "built-in defaults (no config file)".to_string()
            };
            report.ok("config", source);
            Some(config)
        }
        Err(error) => {
            report.fail("config", format!("{error:#}"));
            None
        }
    };

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::context;
    use super::*;
    use crate::doctor::DoctorStatus;

    #[test]
    fn missing_configuration_uses_defaults_and_invalid_configuration_clears_stale_context() {
        let root = tempfile::tempdir().unwrap();
        let mut context = context(root.path());
        let mut report = DoctorReport::default();
        configuration(&mut context, &mut report).unwrap();
        assert!(context.config.is_some());
        assert_eq!(report.checks[0].status, DoctorStatus::Ok);
        assert!(report.checks[0].detail.contains("built-in defaults"));
        std::fs::write(&context.config_path, "[invalid toml").unwrap();
        let mut report = DoctorReport::default();
        configuration(&mut context, &mut report).unwrap();
        assert!(context.config.is_none());
        assert_eq!(report.checks[0].name, "config");
        assert_eq!(report.failures(), 1);
    }
}
