//! openshell diagnostics and probes.
use super::DoctorContext;
use crate::doctor::{resolve_bootstrap_path, resolve_browser_selection, DoctorReport, Result};

pub(super) fn openshell(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    let bootstrap_cli = context.bootstrap_cli.clone();
    let config = &context.config;
    if let Ok(cwd) = std::env::current_dir() {
        if let Some((ok, detail)) = crate::openshell::doctor_pack_detail(&cwd) {
            if ok {
                report.ok("openshell-pack", detail);
            } else {
                report.warn("openshell-pack", detail);
            }

            let firefox_enrolled = match resolve_browser_selection() {
                Ok((selection, _)) => !selection.firefox.is_empty(),
                Err(_) => false,
            };
            let bootstrap_for_openshell = resolve_bootstrap_path(bootstrap_cli.clone()).ok();
            let extras = crate::openshell::doctor_openshell_extras(
                &cwd,
                bootstrap_for_openshell.as_deref(),
                config.as_ref(),
                firefox_enrolled,
            );
            let mut record = |name: &str, (ok, detail): (bool, String)| {
                if ok {
                    report.ok(name, detail);
                } else {
                    report.warn(name, detail);
                }
            };
            record("openshell-admin", extras.admin);
            record("openshell-companion", extras.companion);
            if let Some(mcp) = extras.mcp_url {
                record("openshell-mcp-url", mcp);
            }
            if let Some(cleartext) = extras.cleartext {
                record("openshell-cleartext", cleartext);
            }
            record("openshell-sandboxes", extras.local_sandboxes);
        }
    }

    Ok(())
}
