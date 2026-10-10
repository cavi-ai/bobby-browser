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
