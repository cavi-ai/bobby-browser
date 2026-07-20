use std::future::Future;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::{
    run_process, GateObservation, GateResult, GateStatus, ProcessFailure, ProcessOutcome,
    ProcessSpec, ReleaseManifest,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecurityCheck {
    pub name: &'static str,
    pub required: bool,
    pub args: &'static [&'static str],
}

impl SecurityCheck {
    const fn required(name: &'static str, args: &'static [&'static str]) -> Self {
        Self {
            name,
            required: true,
            args,
        }
    }
}

const CHECKS: &[SecurityCheck] = &[
    SecurityCheck::required(
        "interface-boundaries",
        &[
            "test",
            "-p",
            "runtime-tests",
            "--test",
            "interface_security",
            "real_security_release_matrix_executes_every_production_boundary",
            "--",
            "--ignored",
            "--exact",
            "--nocapture",
        ],
    ),
    SecurityCheck::required(
        "adaptive-http-policy",
        &[
            "test",
            "-p",
            "runtime-tests",
            "--test",
            "adaptive_http_security",
            "--",
            "--nocapture",
        ],
    ),
    SecurityCheck::required(
        "connection-and-workflow-capacity",
        &[
            "test",
            "-p",
            "runtime-tests",
            "--test",
            "interface_capacity",
            "--",
            "--nocapture",
        ],
    ),
];

pub trait ProcessRunner {
    fn run<'a>(
        &'a self,
        spec: &'a ProcessSpec,
    ) -> impl Future<Output = Result<ProcessOutcome, ProcessFailure>> + Send + 'a;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TokioProcessRunner;

impl ProcessRunner for TokioProcessRunner {
    fn run<'a>(
        &'a self,
        spec: &'a ProcessSpec,
    ) -> impl Future<Output = Result<ProcessOutcome, ProcessFailure>> + Send + 'a {
        run_process(spec)
    }
}

#[derive(Clone, Debug)]
pub struct SecurityGate<R = TokioProcessRunner> {
    runner: R,
}

impl Default for SecurityGate<TokioProcessRunner> {
    fn default() -> Self {
        Self::new(TokioProcessRunner)
    }
}

impl<R> SecurityGate<R> {
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    pub fn checks(&self) -> &'static [SecurityCheck] {
        CHECKS
    }
}

impl<R> SecurityGate<R>
where
    R: ProcessRunner,
{
    pub async fn run(&self, repo_root: &Path, manifest: &ReleaseManifest) -> Vec<GateResult> {
        let mut results = Vec::with_capacity(CHECKS.len());
        for check in CHECKS {
            let spec = ProcessSpec::new(
                "cargo",
                check.args,
                Duration::from_secs(manifest.security.timeout_secs),
                manifest.security.max_output_bytes,
            )
            .with_current_dir(repo_root);
            let started = Instant::now();
            let outcome = self.runner.run(&spec).await;
            let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let mut result = result_from_outcome(check, manifest, duration_ms, outcome);
            result.redact(&manifest.secret_canaries);
            results.push(result);
        }
        results
    }
}

fn result_from_outcome(
    check: &SecurityCheck,
    manifest: &ReleaseManifest,
    duration_ms: u64,
    outcome: Result<ProcessOutcome, ProcessFailure>,
) -> GateResult {
    let required = check.required && manifest.security.required;
    match outcome {
        Ok(outcome) => {
            let stdout_valid = std::str::from_utf8(&outcome.stdout).is_ok();
            let stderr_valid = std::str::from_utf8(&outcome.stderr).is_ok();
            let mut diagnostics = Vec::new();
            match outcome.exit_code {
                Some(0) => {}
                Some(exit_code) => {
                    diagnostics.push(format!("cargo exited with status code {exit_code}"));
                }
                None => diagnostics.push("cargo exited without a status code".into()),
            }
            if !stdout_valid {
                diagnostics.push("process stdout was invalid UTF-8".into());
            }
            if !stderr_valid {
                diagnostics.push("process stderr was invalid UTF-8".into());
            }

            let status = if outcome.exit_code == Some(0) && stdout_valid && stderr_valid {
                GateStatus::Passed
            } else {
                GateStatus::Blocked
            };
            let mut result = GateResult::new(
                "security",
                check.name,
                required,
                status,
                duration_ms,
                vec![
                    GateObservation::new(
                        "stdout",
                        String::from_utf8_lossy(&outcome.stdout).into_owned(),
                    ),
                    GateObservation::new(
                        "stderr",
                        String::from_utf8_lossy(&outcome.stderr).into_owned(),
                    ),
                ],
            );
            result.diagnostics = diagnostics.join("; ");
            result
        }
        Err(error) => {
            let mut result = GateResult::new(
                "security",
                check.name,
                required,
                GateStatus::Blocked,
                duration_ms,
                vec![
                    GateObservation::new("stdout", ""),
                    GateObservation::new("stderr", ""),
                ],
            );
            result.diagnostics = match error {
                ProcessFailure::Spawn { source } => {
                    format!("failed to spawn process: {source}")
                }
                error => error.to_string(),
            };
            result
        }
    }
}
