use std::future::Future;
use std::path::Path;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::{
    run_process, GateObservation, GateResult, GateStatus, ProcessFailure, ProcessOutcome,
    ProcessSpec, ReleaseManifest,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecurityCheck {
    pub name: &'static str,
    pub required: bool,
    pub args: &'static [&'static str],
    pub proof: CargoTestProof,
}

impl SecurityCheck {
    const fn required(
        name: &'static str,
        args: &'static [&'static str],
        proof: CargoTestProof,
    ) -> Self {
        Self {
            name,
            required: true,
            args,
            proof,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CargoTestProof {
    pub passed: u64,
    pub failed: u64,
    pub ignored: u64,
    pub measured: u64,
    pub filtered_out: u64,
    pub marker: &'static str,
}

impl CargoTestProof {
    const fn new(
        passed: u64,
        failed: u64,
        ignored: u64,
        measured: u64,
        filtered_out: u64,
        marker: &'static str,
    ) -> Self {
        Self {
            passed,
            failed,
            ignored,
            measured,
            filtered_out,
            marker,
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
        CargoTestProof::new(
            1,
            0,
            0,
            0,
            3,
            "AUTOMATION_RUNTIME_SECURITY_PROOF:v1:interface-boundaries",
        ),
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
        CargoTestProof::new(
            4,
            0,
            0,
            0,
            0,
            "AUTOMATION_RUNTIME_SECURITY_PROOF:v1:adaptive-http-policy",
        ),
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
            "--include-ignored",
            "--nocapture",
        ],
        CargoTestProof::new(
            5,
            0,
            0,
            0,
            0,
            "AUTOMATION_RUNTIME_SECURITY_PROOF:v1:connection-and-workflow-capacity",
        ),
    ),
    SecurityCheck::required(
        "cdp-target-context-policy",
        &[
            "test",
            "-p",
            "cdp-gateway",
            "--test",
            "playwright_domains",
            "--",
            "--nocapture",
        ],
        CargoTestProof::new(
            12,
            0,
            0,
            0,
            0,
            "AUTOMATION_RUNTIME_SECURITY_PROOF:v1:cdp-target-context-policy",
        ),
    ),
];

pub(crate) fn security_catalog() -> &'static [SecurityCheck] {
    CHECKS
}

pub fn security_catalog_sha256() -> String {
    let mut digest = Sha256::new();
    digest.update(b"automation-runtime-security-catalog-v1\0");
    for check in CHECKS {
        digest_catalog_field(&mut digest, check.name.as_bytes());
        digest.update([u8::from(check.required)]);
        digest.update((check.args.len() as u64).to_be_bytes());
        for arg in check.args {
            digest_catalog_field(&mut digest, arg.as_bytes());
        }
        for count in [
            check.proof.passed,
            check.proof.failed,
            check.proof.ignored,
            check.proof.measured,
            check.proof.filtered_out,
        ] {
            digest.update(count.to_be_bytes());
        }
        digest_catalog_field(&mut digest, check.proof.marker.as_bytes());
    }
    hex::encode(digest.finalize())
}

fn digest_catalog_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

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

            if outcome.exit_code == Some(0) && stdout_valid && stderr_valid {
                if let Err(error) = validate_cargo_test_proof(
                    check,
                    std::str::from_utf8(&outcome.stdout).expect("validated stdout"),
                    std::str::from_utf8(&outcome.stderr).expect("validated stderr"),
                ) {
                    diagnostics.push(format!("invalid cargo test proof: {error}"));
                }
            }

            let status = if diagnostics.is_empty() {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CargoTestCounts {
    passed: u64,
    failed: u64,
    ignored: u64,
    measured: u64,
    filtered_out: u64,
}

fn validate_cargo_test_proof(
    check: &SecurityCheck,
    stdout: &str,
    stderr: &str,
) -> Result<(), String> {
    let lines = stdout.lines().chain(stderr.lines()).map(str::trim);
    let collected = lines.collect::<Vec<_>>();
    let marker_count = collected
        .iter()
        .filter(|line| **line == check.proof.marker)
        .count();
    if marker_count != 1 {
        return Err(format!(
            "expected marker {:?} exactly once, observed {marker_count}",
            check.proof.marker
        ));
    }

    let summaries = collected
        .iter()
        .filter(|line| line.starts_with("test result:"))
        .copied()
        .collect::<Vec<_>>();
    if summaries.len() != 1 {
        return Err(format!(
            "expected exactly one cargo test summary, observed {}",
            summaries.len()
        ));
    }
    let actual = parse_cargo_test_summary(summaries[0])?;
    let expected = CargoTestCounts {
        passed: check.proof.passed,
        failed: check.proof.failed,
        ignored: check.proof.ignored,
        measured: check.proof.measured,
        filtered_out: check.proof.filtered_out,
    };
    if actual.passed == 0 {
        return Err("cargo reported zero passed tests".into());
    }
    if actual.ignored != 0 {
        return Err(format!(
            "cargo reported {} ignored required tests",
            actual.ignored
        ));
    }
    if actual != expected {
        return Err(format!(
            "cargo counts did not match catalog: expected {expected:?}, observed {actual:?}"
        ));
    }
    Ok(())
}

fn parse_cargo_test_summary(line: &str) -> Result<CargoTestCounts, String> {
    let counts = line
        .strip_prefix("test result: ok. ")
        .ok_or_else(|| "cargo test summary did not report ok".to_owned())?;
    let (counts, elapsed) = counts
        .split_once("; finished in ")
        .ok_or_else(|| "cargo test summary was malformed".to_owned())?;
    if elapsed.is_empty() {
        return Err("cargo test summary omitted elapsed time".into());
    }
    let mut fields = counts.split("; ");
    let parsed = CargoTestCounts {
        passed: parse_count_field(fields.next(), "passed")?,
        failed: parse_count_field(fields.next(), "failed")?,
        ignored: parse_count_field(fields.next(), "ignored")?,
        measured: parse_count_field(fields.next(), "measured")?,
        filtered_out: parse_count_field(fields.next(), "filtered out")?,
    };
    if fields.next().is_some() {
        return Err("cargo test summary contained extra count fields".into());
    }
    Ok(parsed)
}

fn parse_count_field(field: Option<&str>, label: &str) -> Result<u64, String> {
    let field = field.ok_or_else(|| format!("cargo test summary omitted {label} count"))?;
    let number = field
        .strip_suffix(&format!(" {label}"))
        .ok_or_else(|| format!("cargo test summary malformed {label} count"))?;
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("cargo test summary invalid {label} count"));
    }
    number
        .parse()
        .map_err(|_| format!("cargo test summary overflowed {label} count"))
}
