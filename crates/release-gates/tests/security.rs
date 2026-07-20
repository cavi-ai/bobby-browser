use std::collections::VecDeque;
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
        ]
    );
    assert!(checks.iter().all(|check| check.required));
    assert!(checks[0]
        .args
        .iter()
        .any(|arg| arg == &"real_security_release_matrix_executes_every_production_boundary"));
    assert!(checks[1]
        .args
        .iter()
        .any(|arg| arg == &"adaptive_http_security"));
    assert!(checks[2]
        .args
        .iter()
        .any(|arg| arg == &"interface_capacity"));
}

enum StubOutcome {
    Exit(Option<i32>, Vec<u8>, Vec<u8>),
    Timeout,
    OutputLimit,
    SpawnFailure,
    DelayedSuccess(Duration),
}

#[derive(Default)]
struct StubRunner {
    outcomes: Mutex<VecDeque<StubOutcome>>,
    specs: Arc<Mutex<Vec<ProcessSpec>>>,
}

impl StubRunner {
    fn new(outcomes: impl IntoIterator<Item = StubOutcome>) -> Self {
        Self {
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            specs: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn recording(
        outcomes: impl IntoIterator<Item = StubOutcome>,
    ) -> (Self, Arc<Mutex<Vec<ProcessSpec>>>) {
        let runner = Self::new(outcomes);
        let specs = Arc::clone(&runner.specs);
        (runner, specs)
    }
}

impl ProcessRunner for StubRunner {
    async fn run(&self, spec: &ProcessSpec) -> Result<ProcessOutcome, ProcessFailure> {
        self.specs.lock().expect("spec lock").push(spec.clone());
        let outcome = self
            .outcomes
            .lock()
            .expect("outcome lock")
            .pop_front()
            .expect("stub outcome");
        match outcome {
            StubOutcome::Exit(exit_code, stdout, stderr) => Ok(ProcessOutcome {
                exit_code,
                stdout,
                stderr,
            }),
            StubOutcome::Timeout => Err(ProcessFailure::Timeout),
            StubOutcome::OutputLimit => Err(ProcessFailure::OutputLimit { limit: 8 }),
            StubOutcome::SpawnFailure => Err(ProcessFailure::Spawn {
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "stub spawn failure"),
            }),
            StubOutcome::DelayedSuccess(delay) => {
                tokio::time::sleep(delay).await;
                Ok(ProcessOutcome {
                    exit_code: Some(0),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }
    }
}

fn manifest(canaries: &[&str]) -> ReleaseManifest {
    ReleaseManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        security: SecurityManifest {
            required: true,
            timeout_secs: 17,
            max_output_bytes: 8192,
        },
        secret_canaries: canaries.iter().map(|canary| (*canary).into()).collect(),
    }
}

fn success() -> StubOutcome {
    StubOutcome::Exit(Some(0), Vec::new(), Vec::new())
}

#[tokio::test]
async fn failed_exit_blocks_and_remaining_checks_still_execute() {
    let (runner, specs) = StubRunner::recording([
        StubOutcome::Exit(Some(7), b"failed".to_vec(), b"details".to_vec()),
        success(),
        success(),
    ]);
    let gate = SecurityGate::new(runner);

    let results = gate
        .run(std::path::Path::new("/repository"), &manifest(&["canary"]))
        .await;

    assert_eq!(results.len(), 3);
    assert_eq!(results[0].status, GateStatus::Blocked);
    assert_eq!(results[1].status, GateStatus::Passed);
    assert_eq!(results[2].status, GateStatus::Passed);
    let specs = specs.lock().expect("spec lock");
    assert_eq!(specs.len(), 3);
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
async fn timeout_blocks() {
    let gate = SecurityGate::new(StubRunner::new([
        StubOutcome::Timeout,
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
    ]));
    let results = gate
        .run(std::path::Path::new("/repository"), &manifest(&["canary"]))
        .await;
    assert!(results
        .iter()
        .all(|result| result.status == GateStatus::Blocked));

    let gate = SecurityGate::new(StubRunner::new([
        StubOutcome::SpawnFailure,
        StubOutcome::OutputLimit,
        StubOutcome::Timeout,
    ]));
    let results = gate
        .run(std::path::Path::new("/repository"), &manifest(&["canary"]))
        .await;
    assert!(results
        .iter()
        .all(|result| result.status == GateStatus::Blocked));
}

#[tokio::test]
async fn records_each_check_duration() {
    let gate = SecurityGate::new(StubRunner::new([
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
    assert_eq!(gate.checks().len(), 3);
}
