//! health diagnostics and probes.
use super::storage::{record_jobs_queue, record_operational_slos};
use super::vision_and_gateway_configuration::{check_cdp_port, CdpPortState};
use super::DoctorContext;
use crate::doctor::{push_doctor_check, DoctorReport, Duration, Result};

use anyhow::Context;
pub(super) fn health(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    let bootstrap_path_for_heal = context.bootstrap_path_for_heal.clone();
    let config = &context.config;
    let check_health = context.check_health;
    let unreachable_bidi = &context.unreachable_bidi;
    if check_health {
        if let Some(config) = &config {
            let url = format!(
                "http://{}:{}/healthz",
                config.server.host, config.server.port
            );
            match probe_healthz(&url) {
                Ok(()) => {
                    report.ok("healthz", format!("{url} responded"));
                    let info =
                        record_jobs_queue(report, config, bootstrap_path_for_heal.as_deref());
                    record_operational_slos(report, config, info.as_ref());
                }
                Err(_) => {
                    report.ok("healthz", "not running".to_string());
                }
            }
            let (cdp_check, cdp_state) = check_cdp_port(&config.cdp);
            push_doctor_check(report, cdp_check);
            // Two warnings, one fault: something else holds the CDP port and the
            // enrolled BiDi endpoint answers nothing. A companion launched on the
            // CDP port instead of its enrolled one produces exactly this pair,
            // and it leaves every browser call dead -- so it fails, not warns.
            if cdp_state == CdpPortState::Occupied && !unreachable_bidi.is_empty() {
                report.fail(
                    "firefox-bidi-port-mismatch",
                    format!(
                        "{}:{} is held by another service while enrolled BiDi endpoint(s) {} accept \
                         nothing -- a Firefox companion launched on the CDP port rather than its \
                         enrolled port matches this exactly. Relaunch the companion on the enrolled \
                         port (`make firefox-start`); do not re-enroll onto {}:{}, `bobby cdp` \
                         needs that port free.",
                        config.cdp.host,
                        config.cdp.port,
                        unreachable_bidi.join(", "),
                        config.cdp.host,
                        config.cdp.port,
                    ),
                );
            }
        }
    }

    Ok(())
}

pub(in crate::doctor) fn probe_healthz(url: &str) -> Result<()> {
    let url = url.to_owned();
    match std::thread::spawn(move || probe_healthz_blocking(&url)).join() {
        Ok(result) => result,
        Err(_) => anyhow::bail!("healthz probe thread panicked"),
    }
}

pub(in crate::doctor) fn probe_healthz_blocking(url: &str) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .no_proxy()
        .build()
        .context("failed to build healthz HTTP client")?;
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?;
    if !response.status().is_success() {
        anyhow::bail!("unexpected status {}", response.status());
    }
    Ok(())
}
