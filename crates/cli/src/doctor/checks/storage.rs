//! storage diagnostics and probes.
use super::DoctorContext;
#[cfg(test)]
use crate::doctor::DoctorStatus;
use crate::doctor::{
    idempotency_ledgers, AppConfig, DoctorReport, Duration, Path, PathBuf, Result,
};

pub(super) fn storage(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    let config = &context.config;
    if let Some(config) = &config {
        for (name, dir) in configured_storage_dirs(config) {
            if dir.is_dir() {
                report.ok(name, dir.display().to_string());
            } else {
                report.fail(
                    name,
                    format!("{} missing; run `bobby doctor --fix`", dir.display()),
                );
            }
        }
        record_command_journal(report, &config.storage.journal_path);
        record_scheduler_journal(report, &config.storage.scheduler_journal_path);
        record_idempotency_ledgers(report, config);
        #[cfg(unix)]
        crate::doctor::skill_permissions::record(report, &config.storage.checkpoints_dir);
        if let Some(dir) = &config.vision.corpus_dir {
            record_vision_corpus(report, &dir.join("vision-corpus.jsonl"));
        }
    }

    Ok(())
}

pub(in crate::doctor) fn record_idempotency_ledgers(report: &mut DoctorReport, config: &AppConfig) {
    for (name, path) in idempotency_ledgers(config) {
        let target = path.clone();
        match block_on_inspect(
            async move { interface_core::inspect_idempotency_ledger(target).await },
        ) {
            Ok(health) if !health.exists => report.ok(name, "no ledger yet".into()),
            Ok(health) if health.integrity_issue.is_some() => report.fail(
                name,
                format!(
                    "{} requires repair; preserve the ledger and its reservations",
                    path.display()
                ),
            ),
            Ok(health) => report.ok(
                name,
                format!(
                    "{} · v{} · {} keys",
                    path.display(),
                    health.format.unwrap_or(0),
                    health.entries
                ),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => report.warn(
                name,
                "ledger in use; stop Bobby for offline inspection or downgrade".into(),
            ),
            Err(error) => report.fail(name, format!("{}: {error}", path.display())),
        }
    }
}

pub(in crate::doctor) fn configured_storage_dirs(
    config: &AppConfig,
) -> [(&'static str, PathBuf); 4] {
    [
        (
            "storage-journal-dir",
            config
                .storage
                .journal_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
        ),
        (
            "storage-scheduler-journal-dir",
            config
                .storage
                .scheduler_journal_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
        ),
        (
            "storage-checkpoints-dir",
            config.storage.checkpoints_dir.clone(),
        ),
        ("artifacts-dir", config.browser.artifacts_dir.clone()),
    ]
}

pub(in crate::doctor) fn block_on_inspect<T, F>(fut: F) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("inspect runtime")
                .block_on(fut)
        })
        .join()
        .expect("inspect thread panicked"),
    }
}

pub(in crate::doctor) fn record_jsonl_health(
    report: &mut DoctorReport,
    name: &str,
    path: &Path,
    health: JsonlHealth,
) {
    if !health.exists {
        report.ok(name, format!("{} · not created yet", path.display()));
        return;
    }
    if let Some(line) = health.corrupt_line {
        report.fail(name, format!("corrupt line {line} in {}", path.display()));
        return;
    }
    if health.torn_tail {
        report.warn(
            name,
            format!(
                "torn tail · {} records · {} bytes; run `bobby doctor --fix`",
                health.records, health.bytes
            ),
        );
        return;
    }
    if health.incompatible_records > 0 && name == "scheduler-journal" {
        report.fail(
            name,
            format!(
                "{} unreadable records; scheduler history is read-only and requires repair",
                health.incompatible_records
            ),
        );
        return;
    }
    if health.incompatible_records > 0 {
        report.warn(
            name,
            format!(
                "{} unreadable records skipped in {}",
                health.incompatible_records,
                path.display()
            ),
        );
        return;
    }
    report.ok(
        name,
        format!(
            "{} · {} records · {} bytes",
            path.display(),
            health.records,
            health.bytes
        ),
    );
}

pub(in crate::doctor) fn record_command_journal(report: &mut DoctorReport, path: &Path) {
    let path_buf = path.to_path_buf();
    match block_on_inspect(async move { workflow_journal::JsonlJournal::inspect(path_buf).await }) {
        Ok(health) => record_jsonl_health(
            report,
            "command-journal",
            path,
            JsonlHealth {
                exists: health.exists,
                records: health.records,
                bytes: health.bytes,
                torn_tail: health.torn_tail,
                incompatible_records: health.incompatible_records,
                corrupt_line: None,
            },
        ),
        Err(error) => report.fail("command-journal", format!("{error:#}")),
    }
}

pub(in crate::doctor) fn record_scheduler_journal(report: &mut DoctorReport, path: &Path) {
    let path_buf = path.to_path_buf();
    match block_on_inspect(async move { task_scheduler::JournalJobStore::inspect(path_buf).await })
    {
        Ok(health) => record_jsonl_health(
            report,
            "scheduler-journal",
            path,
            JsonlHealth {
                exists: health.exists,
                records: health.records,
                bytes: health.bytes,
                torn_tail: health.torn_tail,
                incompatible_records: health.incompatible_records,
                corrupt_line: None,
            },
        ),
        Err(error) => report.fail("scheduler-journal", format!("{error:#}")),
    }
}

pub(in crate::doctor) fn record_vision_corpus(report: &mut DoctorReport, path: &Path) {
    match intent_engine::VisionCorpus::inspect(path) {
        Ok(health) => record_jsonl_health(
            report,
            "vision-corpus",
            path,
            JsonlHealth {
                exists: health.exists,
                records: health.records,
                bytes: health.bytes,
                torn_tail: health.torn_tail,
                incompatible_records: 0,
                corrupt_line: health.corrupt_line,
            },
        ),
        Err(error) => report.fail("vision-corpus", format!("{error}")),
    }
}

pub(in crate::doctor) fn record_jobs_queue(
    report: &mut DoctorReport,
    config: &AppConfig,
    bootstrap: Option<&Path>,
) -> Option<types::RuntimeInfo> {
    let bootstrap = bootstrap.unwrap_or_else(|| Path::new(""));
    let bearer = match crate::jobs_client::resolve_jobs_auth(None, bootstrap) {
        Ok(bearer) => bearer,
        Err(error) => {
            report.warn(
                "jobs-queue",
                format!("no bearer to read runtime ({error:#})"),
            );
            return None;
        }
    };
    let url = match crate::v1_client::v1_url(
        &format!("http://{}:{}", config.server.host, config.server.port),
        "/v1/runtime",
    ) {
        Ok(url) => url,
        Err(error) => {
            report.warn("jobs-queue", format!("{error:#}"));
            return None;
        }
    };
    match crate::v1_client::v1_request_with_limits(
        crate::v1_client::V1Request {
            method: reqwest::Method::GET,
            url,
            bearer,
            body: None,
            idempotency_key: None,
        },
        Duration::from_secs(1),
        chrono::Duration::seconds(5),
    ) {
        Ok(response) if response.status.as_u16() == 401 => {
            report.warn(
                "jobs-queue",
                "credential cannot read runtime (HTTP 401)".to_string(),
            );
            None
        }
        Ok(response) if response.status.is_success() => {
            match serde_json::from_str::<types::RuntimeInfo>(&response.body) {
                Ok(info) => {
                    report.ok(
                        "jobs-queue",
                        format!(
                            "queued_jobs={} · sessions={} · uptime_ms={}",
                            info.queued_jobs, info.active_sessions, info.uptime_ms
                        ),
                    );
                    Some(info)
                }
                Err(error) => {
                    report.warn("jobs-queue", format!("GET /v1/runtime: {error:#}"));
                    None
                }
            }
        }
        Ok(response) => {
            report.warn(
                "jobs-queue",
                format!("GET /v1/runtime HTTP {}", response.status),
            );
            None
        }
        Err(error) => {
            report.warn("jobs-queue", format!("{error:#}"));
            None
        }
    }
}

pub(in crate::doctor) fn record_operational_slos(
    report: &mut DoctorReport,
    config: &AppConfig,
    info: Option<&types::RuntimeInfo>,
) {
    let Some(info) = info else {
        report.warn(
            "provider-health",
            "runtime unreadable; provider health and SLOs not evaluated".to_string(),
        );
        return;
    };
    if info.storage_integrity.is_empty() {
        report.ok(
            "storage-integrity",
            "runtime storage has no reported integrity issue".into(),
        );
    } else {
        report.fail(
            "storage-integrity",
            "durable history requires repair; affected mutations are disabled".into(),
        );
    }
    match &info.provider_health {
        None => report.ok(
            "provider-health",
            "no vision provider configured".to_string(),
        ),
        Some(modes) if modes.is_empty() => report.ok(
            "provider-health",
            "no provider calls recorded yet".to_string(),
        ),
        Some(modes) => {
            let unhealthy: Vec<&str> = modes
                .iter()
                .filter(|mode| mode.status == types::ProviderHealthStatus::Unhealthy)
                .map(|mode| mode.provider_mode.as_str())
                .collect();
            let degraded: Vec<&str> = modes
                .iter()
                .filter(|mode| mode.status == types::ProviderHealthStatus::Degraded)
                .map(|mode| mode.provider_mode.as_str())
                .collect();
            if !unhealthy.is_empty() {
                report.fail(
                    "provider-health",
                    format!(
                        "unhealthy: {} (consecutive failures reached threshold)",
                        unhealthy.join(", ")
                    ),
                );
            } else if !degraded.is_empty() {
                report.warn(
                    "provider-health",
                    format!(
                        "degraded: {} (consecutive propose-budget violations reached threshold)",
                        degraded.join(", ")
                    ),
                );
            } else {
                report.ok(
                    "provider-health",
                    modes
                        .iter()
                        .map(|mode| {
                            format!(
                                "{} ok ({} ok / {} failed)",
                                mode.provider_mode, mode.successes, mode.failures
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" · "),
                );
            }
        }
    }
    let violations: u64 = info
        .provider_health
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|mode| mode.budget_violations)
        .sum();
    if let Some(budget_ms) = config.vision.propose_budget_ms {
        if violations > 0 {
            report.warn(
                "slo-vision-latency-budget",
                format!("{violations} propose round-trips exceeded the {budget_ms}ms budget"),
            );
        } else {
            report.ok(
                "slo-vision-latency-budget",
                format!("no propose round-trip exceeded the {budget_ms}ms budget"),
            );
        }
    }
    let slo = &config.observability.slo;
    let Some(metrics) = &info.operational_metrics else {
        if slo.vision_max_failure_rate.is_some() || slo.vision_min_acceptance_rate.is_some() {
            report.warn(
                "slo-vision-rates",
                "runtime reported no operational metrics; SLO rates not evaluated".to_string(),
            );
        }
        return;
    };
    let attempted = metrics.vision.attempted;
    if let Some(max_failure_rate) = slo.vision_max_failure_rate {
        if attempted == 0 {
            report.ok(
                "slo-vision-failure-rate",
                "no vision proposals observed".to_string(),
            );
        } else {
            let failures = metrics.vision.failed + metrics.vision.timed_out;
            let rate = failures as f64 / attempted as f64;
            if rate > max_failure_rate {
                report.fail(
                    "slo-vision-failure-rate",
                    format!("{failures}/{attempted} failed or timed out ({rate:.2} > {max_failure_rate:.2})"),
                );
            } else {
                report.ok(
                    "slo-vision-failure-rate",
                    format!("{failures}/{attempted} failed or timed out ({rate:.2} <= {max_failure_rate:.2})"),
                );
            }
        }
    }
    if let Some(min_acceptance_rate) = slo.vision_min_acceptance_rate {
        if attempted == 0 {
            report.ok(
                "slo-vision-acceptance-rate",
                "no vision proposals observed".to_string(),
            );
        } else {
            let rate = metrics.vision.accepted as f64 / attempted as f64;
            if rate < min_acceptance_rate {
                report.fail(
                    "slo-vision-acceptance-rate",
                    format!(
                        "{}/{} accepted ({rate:.2} < {min_acceptance_rate:.2})",
                        metrics.vision.accepted, attempted
                    ),
                );
            } else {
                report.ok(
                    "slo-vision-acceptance-rate",
                    format!(
                        "{}/{} accepted ({rate:.2} >= {min_acceptance_rate:.2})",
                        metrics.vision.accepted, attempted
                    ),
                );
            }
        }
    }
}

pub(in crate::doctor) struct JsonlHealth {
    exists: bool,
    records: usize,
    bytes: u64,
    torn_tail: bool,
    incompatible_records: usize,
    corrupt_line: Option<usize>,
}

#[cfg(test)]
mod slo_tests {
    use super::*;

    fn health_snapshot(
        status: types::ProviderHealthStatus,
        budget_violations: u64,
    ) -> types::ProviderHealthSnapshot {
        types::ProviderHealthSnapshot {
            provider_mode: "http".to_string(),
            status,
            successes: 8,
            failures: 2,
            consecutive_failures: 0,
            budget_violations,
            last_latency_ms: Some(120),
            latency_budget_ms: Some(1_500),
            failure_threshold: 3,
        }
    }

    fn runtime_info(
        provider_health: Option<Vec<types::ProviderHealthSnapshot>>,
        metrics: &observability::OperationalMetrics,
    ) -> types::RuntimeInfo {
        types::RuntimeInfo {
            storage_integrity: Vec::new(),
            version: "0.14.0".to_string(),
            capabilities: Vec::new(),
            active_sessions: 0,
            queued_jobs: 0,
            uptime_ms: 1,
            vision_propose_budget_ms: None,
            operational_metrics: Some(metrics.snapshot()),
            provider_health,
        }
    }

    fn config_with_slo(
        max_failure: Option<f64>,
        min_acceptance: Option<f64>,
        budget_ms: Option<u64>,
    ) -> AppConfig {
        let mut config = AppConfig::default();
        config.observability.slo.vision_max_failure_rate = max_failure;
        config.observability.slo.vision_min_acceptance_rate = min_acceptance;
        config.vision.propose_budget_ms = budget_ms;
        config
    }

    fn record_proposals(
        metrics: &observability::OperationalMetrics,
        outcome: observability::VisionProposalOutcome,
        count: u64,
    ) {
        for _ in 0..count {
            metrics.record_vision_proposal(observability::VisionProposalMetric {
                provider_mode: observability::ProviderMode::Http,
                latency_ms: 100,
                confidence: None,
                outcome,
            });
        }
    }

    #[test]
    fn unhealthy_provider_fails_doctor() {
        let mut report = DoctorReport::default();
        let info = runtime_info(
            Some(vec![health_snapshot(
                types::ProviderHealthStatus::Unhealthy,
                0,
            )]),
            &observability::OperationalMetrics::default(),
        );
        record_operational_slos(&mut report, &config_with_slo(None, None, None), Some(&info));
        let check = report.check("provider-health").expect("recorded");
        assert_eq!(check.status, DoctorStatus::Fail, "{}", check.detail);
        assert!(check.detail.contains("http"), "{}", check.detail);
    }

    #[test]
    fn degraded_provider_warns_and_healthy_is_ok() {
        let mut report = DoctorReport::default();
        let info = runtime_info(
            Some(vec![health_snapshot(
                types::ProviderHealthStatus::Degraded,
                2,
            )]),
            &observability::OperationalMetrics::default(),
        );
        record_operational_slos(&mut report, &config_with_slo(None, None, None), Some(&info));
        assert_eq!(
            report.check("provider-health").unwrap().status,
            DoctorStatus::Warn
        );

        let mut report = DoctorReport::default();
        let info = runtime_info(
            Some(vec![health_snapshot(
                types::ProviderHealthStatus::Healthy,
                0,
            )]),
            &observability::OperationalMetrics::default(),
        );
        record_operational_slos(&mut report, &config_with_slo(None, None, None), Some(&info));
        assert_eq!(
            report.check("provider-health").unwrap().status,
            DoctorStatus::Ok
        );
    }

    #[test]
    fn missing_provider_health_reads_as_no_provider_configured() {
        let mut report = DoctorReport::default();
        let info = runtime_info(None, &observability::OperationalMetrics::default());
        record_operational_slos(&mut report, &config_with_slo(None, None, None), Some(&info));
        let check = report.check("provider-health").unwrap();
        assert_eq!(check.status, DoctorStatus::Ok);
        assert!(
            check.detail.contains("no vision provider"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn unreadable_runtime_warns_instead_of_failing() {
        let mut report = DoctorReport::default();
        record_operational_slos(&mut report, &config_with_slo(Some(0.1), None, None), None);
        assert_eq!(
            report.check("provider-health").unwrap().status,
            DoctorStatus::Warn
        );
        assert_eq!(report.failures(), 0);
    }

    #[test]
    fn vision_failure_rate_slo_fails_only_when_breached() {
        let metrics = observability::OperationalMetrics::default();
        record_proposals(&metrics, observability::VisionProposalOutcome::Accepted, 3);
        record_proposals(&metrics, observability::VisionProposalOutcome::Failed, 1);
        let info = runtime_info(None, &metrics);

        let mut report = DoctorReport::default();
        record_operational_slos(
            &mut report,
            &config_with_slo(Some(0.5), None, None),
            Some(&info),
        );
        assert_eq!(
            report.check("slo-vision-failure-rate").unwrap().status,
            DoctorStatus::Ok
        );

        let mut report = DoctorReport::default();
        record_operational_slos(
            &mut report,
            &config_with_slo(Some(0.1), None, None),
            Some(&info),
        );
        assert_eq!(
            report.check("slo-vision-failure-rate").unwrap().status,
            DoctorStatus::Fail
        );
    }

    #[test]
    fn vision_acceptance_rate_slo_fails_below_the_floor() {
        let metrics = observability::OperationalMetrics::default();
        record_proposals(&metrics, observability::VisionProposalOutcome::Accepted, 1);
        record_proposals(&metrics, observability::VisionProposalOutcome::Rejected, 3);
        let info = runtime_info(None, &metrics);

        let mut report = DoctorReport::default();
        record_operational_slos(
            &mut report,
            &config_with_slo(None, Some(0.5), None),
            Some(&info),
        );
        assert_eq!(
            report.check("slo-vision-acceptance-rate").unwrap().status,
            DoctorStatus::Fail
        );

        let mut report = DoctorReport::default();
        record_operational_slos(
            &mut report,
            &config_with_slo(None, Some(0.2), None),
            Some(&info),
        );
        assert_eq!(
            report.check("slo-vision-acceptance-rate").unwrap().status,
            DoctorStatus::Ok
        );
    }

    #[test]
    fn latency_budget_violations_warn_without_failing() {
        let mut report = DoctorReport::default();
        let info = runtime_info(
            Some(vec![health_snapshot(
                types::ProviderHealthStatus::Healthy,
                5,
            )]),
            &observability::OperationalMetrics::default(),
        );
        record_operational_slos(
            &mut report,
            &config_with_slo(None, None, Some(1_500)),
            Some(&info),
        );
        let check = report.check("slo-vision-latency-budget").unwrap();
        assert_eq!(check.status, DoctorStatus::Warn, "{}", check.detail);
        assert!(check.detail.contains('5'), "{}", check.detail);

        let mut report = DoctorReport::default();
        let info = runtime_info(
            Some(vec![health_snapshot(
                types::ProviderHealthStatus::Healthy,
                0,
            )]),
            &observability::OperationalMetrics::default(),
        );
        record_operational_slos(
            &mut report,
            &config_with_slo(None, None, Some(1_500)),
            Some(&info),
        );
        assert_eq!(
            report.check("slo-vision-latency-budget").unwrap().status,
            DoctorStatus::Ok
        );
    }

    #[test]
    fn unset_slos_are_not_evaluated() {
        let mut report = DoctorReport::default();
        let metrics = observability::OperationalMetrics::default();
        record_proposals(&metrics, observability::VisionProposalOutcome::Failed, 10);
        let info = runtime_info(None, &metrics);
        record_operational_slos(&mut report, &config_with_slo(None, None, None), Some(&info));
        assert!(report.check("slo-vision-failure-rate").is_none());
        assert!(report.check("slo-vision-acceptance-rate").is_none());
        assert!(report.check("slo-vision-latency-budget").is_none());
    }
}
