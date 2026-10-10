//! browser binaries diagnostics and probes.
use super::DoctorContext;
use crate::doctor::{DoctorReport, Path, Result};

pub(super) fn browser_binaries(
    _context: &mut DoctorContext,
    report: &mut DoctorReport,
) -> Result<()> {
    let firefox = which_binary(&["firefox", "firefox-esr"])
        || [
            "/Applications/Firefox.app",
            "/Applications/Firefox Developer Edition.app",
            "/Applications/Firefox Nightly.app",
        ]
        .iter()
        .any(|bundle| Path::new(bundle).exists());
    if firefox {
        report.ok("firefox", "found".to_string());
    } else {
        report.warn(
            "firefox",
            "not found on PATH or /Applications (default engine)".to_string(),
        );
    }
    let chromium = which_binary(&["google-chrome", "chromium", "chrome"])
        || Path::new("/Applications/Google Chrome.app").exists()
        || Path::new("/Applications/Chromium.app").exists();
    if chromium {
        report.ok("chromium", "found".to_string());
    } else {
        report.warn(
            "chromium",
            "not found (required for Chromium engine selection)".to_string(),
        );
    }

    Ok(())
}

pub(in crate::doctor) fn which_binary(names: &[&str]) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| {
        names.iter().any(|name| {
            let candidate = dir.join(name);
            candidate.is_file() && is_executable(&candidate)
        })
    })
}

#[cfg(unix)]
pub(in crate::doctor) fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub(in crate::doctor) fn is_executable(_path: &Path) -> bool {
    true
}
