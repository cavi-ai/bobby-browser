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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::DoctorStatus;

    fn context(root: &std::path::Path) -> DoctorContext {
        let mut context = super::super::test_support::context(root);
        context.config = Some(config::AppConfig {
            context: config::ContextConfig {
                dir: Some(root.join("context")),
                ..Default::default()
            },
            ..Default::default()
        });
        context
    }

    #[test]
    fn doctor_reports_context_store_without_claiming_its_lock() {
        let root = tempfile::tempdir().unwrap();
        let mut context = context(root.path());
        let profile = root.path().join("context").join("profile-a");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(
            profile.join("68747470733a2f2f6578616d706c652e636f6d.json"),
            br#"{"schema":1,"site_key":"https://example.com","site":{"pages":{}}}"#,
        )
        .unwrap();
        std::fs::write(profile.join(".context-store.lock"), b"1\n").unwrap();

        let mut report = DoctorReport::default();
        context_store(&mut context, &mut report).unwrap();
        let check = report
            .checks
            .iter()
            .find(|check| check.name == "context-store")
            .expect("a context-store check");
        assert!(check.detail.contains("1 site files"), "{check:?}");
        assert!(check.detail.contains("lock held"), "{check:?}");
        assert_eq!(check.status, DoctorStatus::Ok);
    }

    #[test]
    fn doctor_reports_mismatched_context_identity_without_changing_its_bytes() {
        let root = tempfile::tempdir().unwrap();
        let mut context = context(root.path());
        let profile = root.path().join("context/70726f66696c652d61");
        std::fs::create_dir_all(&profile).unwrap();
        let path = profile.join("6f6e65.json");
        let bytes = br#"{"schema":1,"site_key":"two","site":{"pages":{}}}"#;
        std::fs::write(&path, bytes).unwrap();

        let mut report = DoctorReport::default();
        context_store(&mut context, &mut report).unwrap();
        let check = report.check("context-store").expect("context-store");
        assert_eq!(check.status, DoctorStatus::Fail);
        assert!(check.detail.contains("identity"), "{check:?}");
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert!(!profile.join(".context-store.lock").exists());
    }

    #[test]
    fn doctor_fails_context_store_on_invalid_json() {
        let root = tempfile::tempdir().unwrap();
        let mut context = context(root.path());
        let profile = root.path().join("context").join("profile-a");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("https___example.com.json"), b"not-json").unwrap();

        let mut report = DoctorReport::default();
        context_store(&mut context, &mut report).unwrap();
        let check = report.check("context-store").expect("context-store");
        assert_eq!(check.status, DoctorStatus::Fail);
        assert!(check.detail.contains("invalid JSON"), "{check:?}");
    }

    #[test]
    fn doctor_reports_oversized_context_without_claiming_the_store_lock() {
        let root = tempfile::tempdir().unwrap();
        let mut context = context(root.path());
        let profile = root.path().join("context/profile-a");
        std::fs::create_dir_all(&profile).unwrap();
        let mut bytes = br#"{"schema":1,"site_key":"site","site":{"pages":{}}}"#.to_vec();
        bytes.extend(vec![b' '; 2 * 1024 * 1024]);
        let path = profile.join("site.json");
        std::fs::write(&path, &bytes).unwrap();
        let mut report = DoctorReport::default();
        context_store(&mut context, &mut report).unwrap();
        let check = report.check("context-store").unwrap();
        assert_eq!(check.status, DoctorStatus::Warn);
        assert!(check.detail.contains("limit"), "{check:?}");
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert!(!profile.join(".context-store.lock").exists());
    }
}
