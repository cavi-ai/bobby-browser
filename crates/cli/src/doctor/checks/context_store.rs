//! context store diagnostics and probes.
use super::DoctorContext;
use crate::doctor::{default_context_dir, DoctorReport, PathBuf, Result};

pub(super) fn context_store(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    let config = &context.config;
    // Context store: reported without claiming the single-writer lock, so
    // doctor is safe against a live runtime. Lockfile present means a writer
    // holds it (or one crashed); that is lock health, not an error.
    {
        let limits = config
            .as_ref()
            .map(|config| config.context.limits)
            .unwrap_or_default();
        let root = config
            .as_ref()
            .and_then(|config| config.context.dir.clone())
            .or_else(|| default_context_dir().ok());
        match root {
            Some(root) if root.is_dir() => {
                let mut sites = 0_u64;
                let mut bytes = 0_u64;
                let mut locked = false;
                let mut invalid_json: Option<(PathBuf, String)> = None;
                if let Ok(mut entries) = std::fs::read_dir(&root) {
                    while let Some(Ok(profile)) = entries.next() {
                        if let Ok(mut files) = std::fs::read_dir(profile.path()) {
                            while let Some(Ok(file)) = files.next() {
                                let name = file.file_name();
                                let name = name.to_string_lossy();
                                if name == ".context-store.lock" {
                                    locked = true;
                                } else if name.ends_with(".json") {
                                    sites += 1;
                                    bytes += file.metadata().map(|m| m.len()).unwrap_or(0);
                                    if invalid_json.is_none() {
                                        if let Err(reason) =
                                            context_store::inspect_site_file(&file.path(), limits)
                                        {
                                            invalid_json = Some((
                                                file.path(),
                                                reason.chars().take(512).collect(),
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some((path, reason)) = invalid_json {
                    if reason.contains("limit") {
                        report.warn("context-store", format!("{}: {reason}; file preserved; increase [context.limits] only if appropriate", path.display()));
                    } else {
                        report.fail(
                            "context-store",
                            format!("invalid JSON in {} ({reason})", path.display()),
                        );
                    }
                } else {
                    let lock = if locked { "lock held" } else { "lock free" };
                    report.ok(
                        "context-store",
                        format!(
                            "{} · {} site files · {} bytes · {lock} · cache limits: {} sites / {} accounted bytes; site limits: {} bytes / {} records",
                            root.display(),
                            sites,
                            bytes, limits.max_resident_sites, limits.max_resident_bytes,
                            limits.max_file_bytes, limits.max_site_records
                        ),
                    );
                }
            }
            Some(root) => report.ok(
                "context-store",
                format!("{} · no store yet (first run creates it)", root.display()),
            ),
            None => report.warn(
                "context-store",
                "no [context].dir and config directory unavailable".to_string(),
            ),
        }
    }

    Ok(())
}
