//! browser selection diagnostics and probes.
use super::DoctorContext;
use crate::doctor::{resolve_browser_selection, DoctorReport, Result, SelectionSource};

pub(super) fn browser_selection(
    context: &mut DoctorContext,
    report: &mut DoctorReport,
) -> Result<()> {
    context.selection = match resolve_browser_selection() {
        Ok((selection, source)) => {
            report.ok(
                "browser-selection",
                match source {
                    SelectionSource::Environment => {
                        "AUTOMATION_RUNTIME_BROWSER_SELECTION parses".to_string()
                    }
                    SelectionSource::Persisted(path) => {
                        format!("persisted selection at {}", path.display())
                    }
                    SelectionSource::Default => "default (Firefox, exact)".to_string(),
                },
            );
            Some(selection)
        }
        Err(error) => {
            report.fail("browser-selection", format!("{error:#}"));
            None
        }
    };

    Ok(())
}
