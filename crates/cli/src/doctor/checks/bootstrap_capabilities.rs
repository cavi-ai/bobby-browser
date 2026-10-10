//! bootstrap capabilities diagnostics and probes.
use super::vision_and_gateway_configuration::vision_route_configured;
use super::DoctorContext;
use crate::doctor::{
    bootstrap_local, push_doctor_check, resolve_bootstrap_path, AppConfig, DoctorCheck,
    DoctorReport, DoctorStatus, Path, Result,
};

pub(super) fn bootstrap_capabilities(
    context: &mut DoctorContext,
    report: &mut DoctorReport,
) -> Result<()> {
    let config = &context.config;
    // The bootstrap expiry is pinned into MCP client config and the stdio
    // gateway refuses to start once it passes, which a host reports only as a
    // dead server. Warn while there is still time to run `bobby init`.
    context.bootstrap_path_for_heal = resolve_bootstrap_path(context.bootstrap_cli.clone()).ok();
    let bootstrap_path_for_heal = context.bootstrap_path_for_heal.clone();
    if let Some(path) = bootstrap_path_for_heal.as_ref() {
        if path.exists() {
            match bootstrap_local::load_bootstrap_capabilities_csv(path) {
                Ok(caps) => {
                    let preset = bootstrap_local::read_bootstrap_preset(Some(path));
                    let floor = bootstrap_local::capabilities_for_preset(preset);
                    match bootstrap_local::union_capabilities_csv_with(&caps, floor) {
                        Ok((_, added)) if added.is_empty() => {
                            report.ok(
                                "bootstrap-capabilities",
                                "current (matches defaults)".to_string(),
                            );
                        }
                        Ok((_, added)) => {
                            report.warn(
                                "bootstrap-capabilities",
                                format!(
                                    "missing default(s): {}; run `bobby doctor --fix`",
                                    added.join(", ")
                                ),
                            );
                        }
                        Err(error) => {
                            report.warn(
                                "bootstrap-capabilities",
                                format!("could not read capabilities ({error:#})"),
                            );
                        }
                    }
                    if preset
                        .capability_preset()
                        .contains(types::Capability::BrowserFingerprint)
                        && !caps.split(',').any(|c| c.trim() == "browser:fingerprint")
                    {
                        report.warn(
                            "bootstrap-capabilities",
                            "bootstrap lacks browser:fingerprint; run `bobby doctor --fix`"
                                .to_string(),
                        );
                    }
                }
                Err(error) => {
                    report.warn(
                        "bootstrap-capabilities",
                        format!("could not read capabilities ({error:#})"),
                    );
                }
            }
        }
    } else if let Ok(caps) = std::env::var("AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES") {
        let preset = bootstrap_local::read_bootstrap_preset(None);
        let floor = bootstrap_local::capabilities_for_preset(preset);
        if let Ok((_, added)) = bootstrap_local::union_capabilities_csv_with(&caps, floor) {
            if added.is_empty() {
                report.ok(
                    "bootstrap-capabilities",
                    "current (matches defaults)".to_string(),
                );
            } else {
                report.warn(
                    "bootstrap-capabilities",
                    format!(
                        "missing default(s): {}; run `bobby doctor --fix`",
                        added.join(", ")
                    ),
                );
            }
        }
    }

    let holds_vision_assist = {
        let from_file = bootstrap_path_for_heal
            .as_ref()
            .filter(|path| path.exists())
            .and_then(|path| bootstrap_local::load_bootstrap_capabilities_csv(path).ok());
        let from_env = std::env::var("AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES").ok();
        from_file
            .or(from_env)
            .is_some_and(|caps| bootstrap_csv_holds(&caps, "vision:assist"))
    };
    let holds_javascript_evaluate = {
        let from_file = bootstrap_path_for_heal
            .as_ref()
            .filter(|path| path.exists())
            .and_then(|path| bootstrap_local::load_bootstrap_capabilities_csv(path).ok());
        let from_env = std::env::var("AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES").ok();
        from_file
            .or(from_env)
            .is_some_and(|caps| bootstrap_csv_holds(&caps, "javascript:evaluate"))
    };
    if let Some(config) = &config {
        if let Some(check) = check_vision_route_for_assist(config, holds_vision_assist) {
            push_doctor_check(report, check);
        }
    }
    if let Some(check) = check_vision_session_gate(holds_vision_assist) {
        push_doctor_check(report, check);
    }
    if let Some(check) = check_javascript_session_gate(holds_javascript_evaluate) {
        push_doctor_check(report, check);
    }
    push_doctor_check(report, check_builtin_job_handlers());

    let caps_for_preset = bootstrap_path_for_heal
        .as_ref()
        .filter(|path| path.exists())
        .and_then(|path| bootstrap_local::load_bootstrap_capabilities_csv(path).ok())
        .or_else(|| std::env::var("AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES").ok());
    if bootstrap_path_for_heal
        .as_ref()
        .is_some_and(|path| path.exists())
        || caps_for_preset.is_some()
    {
        push_doctor_check(
            report,
            check_bootstrap_preset(
                bootstrap_path_for_heal.as_deref(),
                caps_for_preset.as_deref(),
            ),
        );
    }

    Ok(())
}

pub(in crate::doctor) fn engine_composition_finding(
    error: &anyhow::Error,
    running_owner: Option<String>,
    firefox_unenrolled: bool,
) -> (DoctorStatus, &'static str, String) {
    let held_by_a_runtime = error
        .chain()
        .any(|cause| cause.is::<firefox_companion::selection::ProfileOwned>());
    match running_owner {
        Some(origin) if held_by_a_runtime => (
            DoctorStatus::Ok,
            "engine-satisfiability",
            format!("the running runtime at {origin} holds this scope's browser registrations"),
        ),
        _ if firefox_unenrolled && format!("{error:#}").contains("Firefox") => (
            DoctorStatus::Warn,
            "firefox-enrollment",
            "Firefox is not paired yet. Run `bobby install --companion`, then `make firefox-start`, and click Pair in the Bobby companion toolbar popup. Re-run `bobby doctor` afterward."
                .to_string(),
        ),
        _ => (
            DoctorStatus::Fail,
            "engine-satisfiability",
            format!("{error:#}"),
        ),
    }
}

pub(in crate::doctor) fn bootstrap_csv_holds(caps_csv: &str, capability: &str) -> bool {
    caps_csv.split(',').any(|entry| entry.trim() == capability)
}

pub(in crate::doctor) fn check_vision_route_for_assist(
    config: &AppConfig,
    holds_vision_assist: bool,
) -> Option<DoctorCheck> {
    if !holds_vision_assist {
        return None;
    }
    if vision_route_configured(config) {
        Some(DoctorCheck {
            status: DoctorStatus::Ok,
            name: "vision-route".to_string(),
            detail: "vision:assist has a configured route".to_string(),
        })
    } else {
        Some(DoctorCheck {
            status: DoctorStatus::Warn,
            name: "vision-route".to_string(),
            detail: "vision:assist is granted but no vision route is configured; run `bobby vision connect`".to_string(),
        })
    }
}

pub(in crate::doctor) fn check_vision_session_gate(
    holds_vision_assist: bool,
) -> Option<DoctorCheck> {
    if !holds_vision_assist {
        return None;
    }
    Some(DoctorCheck {
        status: DoctorStatus::Ok,
        name: "vision-session-gate".to_string(),
        detail: "vision:assist is held; sessions still need executionPolicy.visionAssist=true (cap alone is not enough)".to_string(),
    })
}

pub(in crate::doctor) fn check_javascript_session_gate(
    holds_javascript_evaluate: bool,
) -> Option<DoctorCheck> {
    if !holds_javascript_evaluate {
        return None;
    }
    Some(DoctorCheck {
        status: DoctorStatus::Ok,
        name: "javascript-session-gate".to_string(),
        detail: "javascript:evaluate is held; sessions still need executionPolicy.javascriptEvaluation=true (cap alone is not enough)".to_string(),
    })
}

pub(in crate::doctor) fn check_builtin_job_handlers() -> DoctorCheck {
    DoctorCheck {
        status: DoctorStatus::Ok,
        name: "job-handlers".to_string(),
        detail: format!(
            "builtin job handlers: {} (job_submit name=…)",
            broker::BUILTIN_JOB_HANDLERS.join(", ")
        ),
    }
}

pub(in crate::doctor) fn check_bootstrap_preset(
    path: Option<&Path>,
    caps_csv: Option<&str>,
) -> DoctorCheck {
    let preset = bootstrap_local::read_bootstrap_preset(path);
    let floor = preset.capability_preset();
    let held: Vec<&str> = caps_csv
        .map(|caps| {
            caps.split(',')
                .map(str::trim)
                .filter(|capability| !capability.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let beyond: Vec<&str> = held
        .iter()
        .copied()
        .filter(|capability| {
            !floor
                .capabilities()
                .iter()
                .any(|allowed| allowed.as_str() == *capability)
        })
        .collect();
    if !beyond.is_empty() {
        return DoctorCheck {
            status: DoctorStatus::Warn,
            name: "bootstrap-preset".to_string(),
            detail: format!(
                "preset is {} but the capability list also holds {}; re-run `bobby init --preset {} --force`",
                preset.as_str(),
                beyond.join(", "),
                preset.as_str()
            ),
        };
    }
    let holds_admin = held.contains(&"authority:admin");
    DoctorCheck {
        status: DoctorStatus::Ok,
        name: "bootstrap-preset".to_string(),
        detail: match preset {
            bootstrap_local::BootstrapPreset::Unrestricted if holds_admin => {
                "unrestricted (includes authority:admin)".to_string()
            }
            bootstrap_local::BootstrapPreset::Unrestricted => {
                "unrestricted (authority:admin not present; heal will add it)".to_string()
            }
            _ => format!("{} ({})", preset.as_str(), floor.summary()),
        },
    }
}
