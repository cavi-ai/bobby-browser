use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use release_gates::{
    GateStatus, ProcessFailure, ProcessOutcome, ProcessRunner, ProcessSpec, ReleaseManifest,
    SecurityGate, SecurityManifest, TokioProcessRunner, MANIFEST_SCHEMA_VERSION,
};

#[test]
fn security_catalog_names_every_authoritative_production_suite() {
    let checks = SecurityGate::default().checks();
    assert_eq!(
        checks.iter().map(|check| check.name).collect::<Vec<_>>(),
        vec![
            "interface-boundaries",
            "adaptive-http-policy",
            "connection-and-workflow-capacity",
            "cdp-target-context-policy",
        ]
    );
    assert!(checks.iter().all(|check| check.required));
    assert_eq!(
        checks[0].args,
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
        ]
    );
    assert_eq!(
        checks[1].args,
        &[
            "test",
            "-p",
            "runtime-tests",
            "--test",
            "adaptive_http_security",
            "--",
            "--nocapture",
        ]
    );
    assert_eq!(
        checks[2].args,
        &[
            "test",
            "-p",
            "runtime-tests",
            "--test",
            "interface_capacity",
            "--",
            "--include-ignored",
            "--nocapture",
        ]
    );
    assert_eq!(
        checks[3].args,
        &[
            "test",
            "-p",
            "cdp-gateway",
            "--test",
            "playwright_domains",
            "--",
            "--nocapture",
        ]
    );
    assert_eq!(checks[0].proof.passed, 1);
    assert_eq!(checks[0].proof.filtered_out, 3);
    assert_eq!(checks[1].proof.passed, 4);
    assert_eq!(checks[2].proof.passed, 5);
    assert_eq!(checks[3].proof.passed, 12);
    assert!(checks.iter().all(|check| check.proof.failed == 0
        && check.proof.ignored == 0
        && check.proof.measured == 0));
}

enum StubOutcome {
    Exit(Option<i32>, Vec<u8>, Vec<u8>),
    Timeout,
    OutputLimit,
    SpawnFailure,
    ProofSuccess,
    DelayedSuccess(Duration),
}

#[derive(Default)]
struct StubRunner {
    outcomes: Mutex<VecDeque<StubOutcome>>,
    trace: Arc<RunnerTrace>,
}

#[derive(Default)]
struct RunnerTrace {
    active: AtomicUsize,
    peak: AtomicUsize,
    specs: Mutex<Vec<ProcessSpec>>,
}

impl StubRunner {
    fn new(outcomes: impl IntoIterator<Item = StubOutcome>) -> Self {
        Self {
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            trace: Arc::new(RunnerTrace::default()),
        }
    }

    fn recording(outcomes: impl IntoIterator<Item = StubOutcome>) -> (Self, Arc<RunnerTrace>) {
        let runner = Self::new(outcomes);
        let trace = Arc::clone(&runner.trace);
        (runner, trace)
    }
}

impl ProcessRunner for StubRunner {
    async fn run(&self, spec: &ProcessSpec) -> Result<ProcessOutcome, ProcessFailure> {
        self.trace
            .specs
            .lock()
            .expect("spec lock")
            .push(spec.clone());
        let active = self.trace.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.trace.peak.fetch_max(active, Ordering::SeqCst);
        let outcome = self
            .outcomes
            .lock()
            .expect("outcome lock")
            .pop_front()
            .expect("stub outcome");
        let result = match outcome {
            StubOutcome::Exit(exit_code, stdout, stderr) => Ok(ProcessOutcome {
                exit_code,
                stdout,
                stderr,
            }),
            StubOutcome::Timeout => Err(ProcessFailure::Timeout),
            StubOutcome::OutputLimit => Err(ProcessFailure::OutputLimit { limit: 8 }),
            StubOutcome::SpawnFailure => Err(ProcessFailure::Spawn {
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "canary binary missing"),
            }),
            StubOutcome::ProofSuccess => Ok(ProcessOutcome {
                exit_code: Some(0),
                stdout: valid_proof_receipt(spec),
                stderr: Vec::new(),
            }),
            StubOutcome::DelayedSuccess(delay) => {
                tokio::time::sleep(delay).await;
                Ok(ProcessOutcome {
                    exit_code: Some(0),
                    stdout: valid_proof_receipt(spec),
                    stderr: Vec::new(),
                })
            }
        };
        self.trace.active.fetch_sub(1, Ordering::SeqCst);
        result
    }
}

fn manifest(canaries: &[&str]) -> ReleaseManifest {
    manifest_with_required(canaries, true)
}

fn manifest_with_required(canaries: &[&str], required: bool) -> ReleaseManifest {
    ReleaseManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        security: SecurityManifest {
            required,
            timeout_secs: 17,
            max_output_bytes: 8192,
        },
        secret_canaries: canaries.iter().map(|canary| (*canary).into()).collect(),
    }
}

fn success() -> StubOutcome {
    StubOutcome::ProofSuccess
}

fn valid_proof_receipt(spec: &ProcessSpec) -> Vec<u8> {
    let gate = SecurityGate::default();
    let check = gate
        .checks()
        .iter()
        .find(|check| {
            check.args.len() == spec.args.len()
                && check
                    .args
                    .iter()
                    .zip(&spec.args)
                    .all(|(expected, actual)| actual == expected)
        })
        .expect("stub spec must match immutable catalog");
    format!(
        "{}\ntest result: ok. {} passed; {} failed; {} ignored; {} measured; {} filtered out; finished in 0.00s\n",
        check.proof.marker,
        check.proof.passed,
        check.proof.failed,
        check.proof.ignored,
        check.proof.measured,
        check.proof.filtered_out,
    )
    .into_bytes()
}

fn first_outcome_then_successes(first: StubOutcome) -> Vec<StubOutcome> {
    let mut outcomes = vec![first];
    outcomes.extend((1..SecurityGate::default().checks().len()).map(|_| success()));
    outcomes
}

#[tokio::test]
async fn exit_zero_with_zero_executed_tests_blocks_required_proof() {
    let gate = SecurityGate::new(StubRunner::new(first_outcome_then_successes(
        StubOutcome::Exit(
            Some(0),
            b"test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.00s\n"
                .to_vec(),
            Vec::new(),
        ),
    )));

    let results = gate
        .run(std::path::Path::new("/repository"), &manifest(&["canary"]))
        .await;

    assert_eq!(results[0].status, GateStatus::Blocked);
    assert!(results[0].diagnostics.contains("proof"));
}

#[tokio::test]
async fn exit_zero_with_ignored_required_test_blocks_proof() {
    let gate = SecurityGate::new(StubRunner::new(first_outcome_then_successes(
        StubOutcome::Exit(
            Some(0),
            b"test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n"
                .to_vec(),
            Vec::new(),
        ),
    )));

    let results = gate
        .run(std::path::Path::new("/repository"), &manifest(&["canary"]))
        .await;

    assert_eq!(results[0].status, GateStatus::Blocked);
    assert!(results[0].diagnostics.contains("proof"));
}

#[tokio::test]
async fn exit_zero_with_mismatched_counts_blocks_proof() {
    let gate = SecurityGate::new(StubRunner::new(first_outcome_then_successes(
        StubOutcome::Exit(
            Some(0),
            b"test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n"
                .to_vec(),
            Vec::new(),
        ),
    )));

    let results = gate
        .run(std::path::Path::new("/repository"), &manifest(&["canary"]))
        .await;

    assert_eq!(results[0].status, GateStatus::Blocked);
    assert!(results[0].diagnostics.contains("proof"));
}

#[tokio::test]
async fn cargo_summary_without_exact_unique_marker_blocks_proof() {
    let gate = SecurityGate::new(StubRunner::new(first_outcome_then_successes(
        StubOutcome::Exit(
            Some(0),
            b"prefix AUTOMATION_RUNTIME_SECURITY_PROOF:v1:interface-boundaries suffix\n\
              test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n"
                .to_vec(),
            Vec::new(),
        ),
    )));

    let results = gate
        .run(std::path::Path::new("/repository"), &manifest(&["canary"]))
        .await;

    assert_eq!(results[0].status, GateStatus::Blocked);
    assert!(results[0].diagnostics.contains("proof"));
}

#[tokio::test]
async fn malformed_or_duplicated_cargo_receipt_blocks_proof() {
    for stdout in [
        b"AUTOMATION_RUNTIME_SECURITY_PROOF:v1:interface-boundaries\n\
          test result: ok. one passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s\n"
            .to_vec(),
        b"AUTOMATION_RUNTIME_SECURITY_PROOF:v1:interface-boundaries\n\
          AUTOMATION_RUNTIME_SECURITY_PROOF:v1:interface-boundaries\n\
          test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s\n"
            .to_vec(),
    ] {
        let gate = SecurityGate::new(StubRunner::new(first_outcome_then_successes(
            StubOutcome::Exit(Some(0), stdout, Vec::new()),
        )));

        let results = gate
            .run(std::path::Path::new("/repository"), &manifest(&["canary"]))
            .await;

        assert_eq!(results[0].status, GateStatus::Blocked);
        assert!(results[0].diagnostics.contains("proof"));
    }
}

#[tokio::test]
async fn failed_exit_blocks_and_remaining_checks_still_execute() {
    let (runner, trace) = StubRunner::recording([
        StubOutcome::Exit(Some(7), b"failed".to_vec(), b"details".to_vec()),
        success(),
        success(),
        success(),
    ]);
    let gate = SecurityGate::new(runner);

    let results = gate
        .run(std::path::Path::new("/repository"), &manifest(&["canary"]))
        .await;

    assert_eq!(results.len(), 4);
    assert_eq!(results[0].status, GateStatus::Blocked);
    assert_eq!(results[1].status, GateStatus::Passed);
    assert_eq!(results[2].status, GateStatus::Passed);
    assert_eq!(results[0].diagnostics, "cargo exited with status code 7");
    let specs = trace.specs.lock().expect("spec lock");
    assert_eq!(specs.len(), 4);
    assert!(specs.iter().all(|spec| spec.program == "cargo"));
    assert!(specs
        .iter()
        .all(|spec| spec.current_dir.as_deref() == Some(std::path::Path::new("/repository"))));
    assert!(specs
        .iter()
        .all(|spec| spec.timeout == Duration::from_secs(17)));
    assert!(specs.iter().all(|spec| spec.max_output_bytes == 8192));
}

#[tokio::test]
async fn optional_manifest_marks_catalog_results_not_required() {
    let gate = SecurityGate::new(StubRunner::new([
        success(),
        success(),
        success(),
        success(),
    ]));
    let manifest = manifest_with_required(&["canary"], false);

    let results = gate
        .run(std::path::Path::new("/repository"), &manifest)
        .await;

    assert!(results.iter().zip(gate.checks()).all(|(result, check)| {
        result.required == (manifest.security.required && check.required)
    }));
    assert!(results.iter().all(|result| !result.required));
}

#[tokio::test]
async fn runner_calls_are_sequential_and_follow_catalog_order() {
    let (runner, trace) = StubRunner::recording([
        StubOutcome::DelayedSuccess(Duration::from_millis(5)),
        StubOutcome::DelayedSuccess(Duration::from_millis(5)),
        StubOutcome::DelayedSuccess(Duration::from_millis(5)),
        StubOutcome::DelayedSuccess(Duration::from_millis(5)),
    ]);
    let gate = SecurityGate::new(runner);

    gate.run(std::path::Path::new("/repository"), &manifest(&["canary"]))
        .await;

    assert_eq!(trace.peak.load(Ordering::SeqCst), 1);
    assert_eq!(trace.active.load(Ordering::SeqCst), 0);
    let specs = trace.specs.lock().expect("spec lock");
    assert_eq!(
        specs
            .iter()
            .map(|spec| spec.args[4].to_string_lossy())
            .collect::<Vec<_>>(),
        vec![
            "interface_security",
            "adaptive_http_security",
            "interface_capacity",
            "playwright_domains",
        ]
    );
}

#[tokio::test]
async fn timeout_blocks() {
    let gate = SecurityGate::new(StubRunner::new([
        StubOutcome::Timeout,
        success(),
        success(),
        success(),
    ]));

    let results = gate
        .run(std::path::Path::new("/repository"), &manifest(&["canary"]))
        .await;

    assert_eq!(results[0].status, GateStatus::Blocked);
}

#[tokio::test]
async fn stdout_and_stderr_canaries_are_redacted_before_results_are_returned() {
    let gate = SecurityGate::new(StubRunner::new([
        StubOutcome::Exit(
            Some(0),
            b"stdout alpha-secret".to_vec(),
            b"stderr alpha-secret".to_vec(),
        ),
        success(),
        success(),
        success(),
    ]));

    let results = gate
        .run(
            std::path::Path::new("/repository"),
            &manifest(&["alpha-secret"]),
        )
        .await;

    assert_eq!(results[0].observations[0].name, "stdout");
    assert_eq!(results[0].observations[0].value, "stdout [REDACTED]");
    assert_eq!(results[0].observations[1].name, "stderr");
    assert_eq!(results[0].observations[1].value, "stderr [REDACTED]");
}

#[tokio::test]
async fn every_non_success_outcome_blocks() {
    let gate = SecurityGate::new(StubRunner::new([
        StubOutcome::Exit(None, Vec::new(), Vec::new()),
        StubOutcome::Exit(Some(0), vec![0xff], Vec::new()),
        StubOutcome::Exit(Some(0), Vec::new(), vec![0xff]),
        StubOutcome::Exit(None, Vec::new(), Vec::new()),
    ]));
    let results = gate
        .run(std::path::Path::new("/repository"), &manifest(&["canary"]))
        .await;
    assert!(results
        .iter()
        .all(|result| result.status == GateStatus::Blocked));
    assert_eq!(results[0].diagnostics, "cargo exited without a status code");
    assert_eq!(results[1].diagnostics, "process stdout was invalid UTF-8");
    assert_eq!(results[2].diagnostics, "process stderr was invalid UTF-8");

    let gate = SecurityGate::new(StubRunner::new([
        StubOutcome::SpawnFailure,
        StubOutcome::OutputLimit,
        StubOutcome::Timeout,
        StubOutcome::Timeout,
    ]));
    let results = gate
        .run(std::path::Path::new("/repository"), &manifest(&["canary"]))
        .await;
    assert!(results
        .iter()
        .all(|result| result.status == GateStatus::Blocked));
    assert_eq!(
        results[0].diagnostics,
        "failed to spawn process: [REDACTED] binary missing"
    );
    assert_eq!(
        results[1].diagnostics,
        "process exceeded the combined output limit of 8 bytes"
    );
    assert_eq!(results[2].diagnostics, "process timed out");
    assert!(results
        .iter()
        .all(|result| !result.diagnostics.contains("canary")));
}

#[tokio::test]
async fn records_each_check_duration() {
    let gate = SecurityGate::new(StubRunner::new([
        StubOutcome::DelayedSuccess(Duration::from_millis(5)),
        StubOutcome::DelayedSuccess(Duration::from_millis(5)),
        StubOutcome::DelayedSuccess(Duration::from_millis(5)),
        StubOutcome::DelayedSuccess(Duration::from_millis(5)),
    ]));

    let results = gate
        .run(std::path::Path::new("/repository"), &manifest(&["canary"]))
        .await;

    assert!(results.iter().all(|result| result.duration_ms >= 5));
}

#[test]
fn default_gate_uses_the_trusted_process_runner() {
    let gate: SecurityGate<TokioProcessRunner> = SecurityGate::default();
    assert_eq!(gate.checks().len(), 4);
}

#[cfg(unix)]
#[tokio::test]
async fn tokio_process_runner_delegates_to_the_bounded_process_runner() {
    let runner = TokioProcessRunner;
    let spec = ProcessSpec::new(
        "/bin/sh",
        ["-c", "printf delegated"],
        Duration::from_secs(1),
        64,
    );

    let outcome = runner.run(&spec).await.expect("delegated process");

    assert_eq!(outcome.exit_code, Some(0));
    assert_eq!(outcome.stdout, b"delegated");
    assert!(outcome.stderr.is_empty());
}
