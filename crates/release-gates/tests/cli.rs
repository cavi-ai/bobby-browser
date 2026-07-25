use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use release_gates::{
    cli::{
        exit_code, failure_exit_code, parse_args, run_security, summary_lines, BundleError,
        CertificationBundle, CliError, CliFailureStage, Command, MAX_MANIFEST_BYTES,
    },
    CertificationVerdict, GateResult, GateStatus, PolicyError, ProcessFailure, ProcessOutcome,
    ProcessRunner, ProcessSpec, SecurityGate,
};
use sha2::{Digest, Sha256};

#[test]
fn security_cli_requires_explicit_paths_and_has_stable_exit_codes() {
    let parsed = parse_args([
        "security",
        "--manifest",
        "config/release-gates.json",
        "--output",
        "target/release-gates/security.json",
    ])
    .unwrap();

    assert!(matches!(parsed.command, Command::Security));
    assert_eq!(exit_code(CertificationVerdict::Passed), 0);
    assert_eq!(exit_code(CertificationVerdict::Degraded), 3);
    assert_eq!(exit_code(CertificationVerdict::Blocked), 1);
    assert!(parse_args(["security", "--manifest", "config/release-gates.json"]).is_err());
}

#[test]
fn cli_failures_have_typed_stages_and_exact_exit_codes() {
    let usage = CliError::Usage;
    assert_eq!(usage.failure_stage(), CliFailureStage::PreExecution);
    assert_eq!(failure_exit_code(&usage), 2);

    let policy = CliError::Policy(PolicyError::MissingRequiredSuite("security".into()));
    assert_eq!(policy.failure_stage(), CliFailureStage::PostExecution);
    assert_eq!(failure_exit_code(&policy), 1);

    let evidence = CliError::Bundle(BundleError::Serialize(
        serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
    ));
    assert_eq!(evidence.failure_stage(), CliFailureStage::PostExecution);
    assert_eq!(failure_exit_code(&evidence), 1);

    let bundle_size = CliError::Bundle(BundleError::TooLarge {
        actual_bytes: 2,
        max_bytes: 1,
    });
    assert_eq!(bundle_size.failure_stage(), CliFailureStage::PostExecution);
    assert_eq!(failure_exit_code(&bundle_size), 1);

    let persistence = CliError::Bundle(BundleError::Io(std::io::Error::other(
        "injected persistence failure",
    )));
    assert_eq!(persistence.failure_stage(), CliFailureStage::PostExecution);
    assert_eq!(failure_exit_code(&persistence), 1);
}

#[test]
fn security_cli_rejects_ambiguous_or_unsafe_arguments() {
    let invalid = [
        vec![
            "security",
            "--manifest",
            "config/release-gates.json",
            "--manifest",
            "config/other.json",
            "--output",
            "target/security.json",
        ],
        vec![
            "security",
            "--manifest",
            "config/release-gates.json",
            "--output",
            "target/security.json",
            "extra",
        ],
        vec![
            "security",
            "--manifest",
            "config/release-gates.json",
            "--unknown",
            "value",
            "--output",
            "target/security.json",
        ],
        vec![
            "security",
            "--manifest",
            "config/../release-gates.json",
            "--output",
            "target/security.json",
        ],
        vec![
            "security",
            "--manifest",
            "config/release-gates.json",
            "--output",
            "target/../security.json",
        ],
        vec!["security", "--manifest", "--output", "target/security.json"],
    ];

    for args in invalid {
        assert!(parse_args(args).is_err());
    }
}

#[derive(Clone, Default)]
struct StubRunner {
    calls: Arc<AtomicUsize>,
    canary: Option<String>,
}

impl ProcessRunner for StubRunner {
    async fn run(&self, spec: &ProcessSpec) -> Result<ProcessOutcome, ProcessFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ProcessOutcome {
            exit_code: Some(0),
            stdout: valid_proof_receipt(spec, self.canary.as_deref()),
            stderr: Vec::new(),
        })
    }
}

fn valid_proof_receipt(spec: &ProcessSpec, extra: Option<&str>) -> Vec<u8> {
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
        "{}\n{}\ntest result: ok. {} passed; {} failed; {} ignored; {} measured; {} filtered out; finished in 0.00s\n",
        extra.unwrap_or_default(),
        check.proof.marker,
        check.proof.passed,
        check.proof.failed,
        check.proof.ignored,
        check.proof.measured,
        check.proof.filtered_out,
    )
    .into_bytes()
}

fn manifest(max_output_bytes: usize) -> String {
    format!(
        r#"{{
          "schemaVersion":1,
          "security":{{"required":true,"timeoutSecs":30,"maxOutputBytes":{max_output_bytes}}},
          "secretCanaries":["cli-secret-canary"]
        }}"#
    )
}

fn catalog_results(status: GateStatus) -> Vec<GateResult> {
    SecurityGate::default()
        .checks()
        .iter()
        .map(|check| GateResult::new("security", check.name, true, status.clone(), 1, vec![]))
        .collect()
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ForgedBundleEvidence<'a> {
    schema_version: u32,
    catalog_sha256: &'a str,
    manifest_sha256: &'a str,
    results: &'a [GateResult],
    verdict: &'a str,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ForgedBundleEnvelope<'a> {
    schema_version: u32,
    catalog_sha256: &'a str,
    manifest_sha256: &'a str,
    results: &'a [GateResult],
    verdict: &'a str,
    bundle_sha256: &'a str,
}

fn forged_bundle_value(
    schema_version: u32,
    catalog_sha256: &str,
    manifest_sha256: &str,
    results: &[GateResult],
    verdict: &str,
) -> serde_json::Value {
    let evidence = ForgedBundleEvidence {
        schema_version,
        catalog_sha256,
        manifest_sha256,
        results,
        verdict,
    };
    let bundle_sha256 = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&evidence).unwrap())
    );
    serde_json::to_value(ForgedBundleEnvelope {
        schema_version,
        catalog_sha256,
        manifest_sha256,
        results,
        verdict,
        bundle_sha256: &bundle_sha256,
    })
    .unwrap()
}

fn assert_forged_bundle_rejected(results: &[GateResult], verdict: &str, catalog_sha256: &str) {
    let value = forged_bundle_value(1, catalog_sha256, &"0".repeat(64), results, verdict);
    assert!(serde_json::from_value::<CertificationBundle>(value).is_err());
}

fn security_cli(
    manifest_path: &std::path::Path,
    output: &std::path::Path,
) -> release_gates::cli::Cli {
    parse_args([
        "security".into(),
        "--manifest".into(),
        manifest_path.display().to_string(),
        "--output".into(),
        output.display().to_string(),
    ])
    .unwrap()
}

#[test]
fn certification_bundle_try_new_rejects_missing_catalog_results() {
    let error =
        CertificationBundle::try_new("0".repeat(64), Vec::new(), CertificationVerdict::Passed)
            .unwrap_err();

    assert!(error.to_string().contains("catalog"));
}

#[test]
fn certification_bundle_rejects_empty_and_missing_catalog_results_even_when_rehashed() {
    let catalog_sha256 = release_gates::security_catalog_sha256();
    assert_forged_bundle_rejected(&[], "passed", &catalog_sha256);
    let mut missing = catalog_results(GateStatus::Passed);
    missing.pop();
    assert_forged_bundle_rejected(&missing, "passed", &catalog_sha256);
}

#[test]
fn certification_bundle_rejects_reordered_extra_and_duplicate_catalog_results() {
    let catalog_sha256 = release_gates::security_catalog_sha256();

    let mut reordered = catalog_results(GateStatus::Passed);
    reordered.swap(0, 1);
    assert_forged_bundle_rejected(&reordered, "passed", &catalog_sha256);

    let mut extra = catalog_results(GateStatus::Passed);
    extra.push(extra[0].clone());
    assert_forged_bundle_rejected(&extra, "passed", &catalog_sha256);

    let mut duplicate = catalog_results(GateStatus::Passed);
    duplicate[1] = duplicate[0].clone();
    assert_forged_bundle_rejected(&duplicate, "passed", &catalog_sha256);
}

#[test]
fn certification_bundle_rejects_blocked_as_passed_and_nonrequired_results_when_rehashed() {
    let catalog_sha256 = release_gates::security_catalog_sha256();
    let mut blocked = catalog_results(GateStatus::Passed);
    blocked[0].status = GateStatus::Blocked;
    assert_forged_bundle_rejected(&blocked, "passed", &catalog_sha256);

    let mut nonrequired = catalog_results(GateStatus::Passed);
    nonrequired[0].required = false;
    assert_forged_bundle_rejected(&nonrequired, "passed", &catalog_sha256);
}

#[test]
fn certification_bundle_rejects_forged_catalog_manifest_and_schema_identity_when_rehashed() {
    let results = catalog_results(GateStatus::Passed);
    assert_forged_bundle_rejected(&results, "passed", &"1".repeat(64));

    let invalid_manifest = forged_bundle_value(
        1,
        &release_gates::security_catalog_sha256(),
        "not-a-digest",
        &results,
        "passed",
    );
    assert!(serde_json::from_value::<CertificationBundle>(invalid_manifest).is_err());

    let invalid_schema = forged_bundle_value(
        2,
        &release_gates::security_catalog_sha256(),
        &"0".repeat(64),
        &results,
        "passed",
    );
    assert!(serde_json::from_value::<CertificationBundle>(invalid_schema).is_err());
}

#[tokio::test]
async fn invalid_manifest_is_rejected_before_output_or_checks() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("invalid.json");
    let output = dir.path().join("never-created/security.json");
    std::fs::write(&manifest_path, manifest(0)).unwrap();
    let cli = parse_args([
        "security".into(),
        "--manifest".into(),
        manifest_path.display().to_string(),
        "--output".into(),
        output.display().to_string(),
    ])
    .unwrap();
    let runner = StubRunner::default();
    let calls = Arc::clone(&runner.calls);

    assert!(run_security(&cli, dir.path(), &SecurityGate::new(runner))
        .await
        .is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!output.exists());
    assert!(!output.parent().unwrap().exists());
}

#[cfg(unix)]
#[tokio::test]
async fn manifest_descriptor_rejects_oversize_and_nonregular_inputs_before_checks() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("security.json");
    let runner = StubRunner::default();
    let calls = Arc::clone(&runner.calls);

    let oversized = dir.path().join("oversized.json");
    std::fs::write(&oversized, vec![b' '; MAX_MANIFEST_BYTES + 1]).unwrap();
    assert!(matches!(
        run_security(
            &security_cli(&oversized, &output),
            dir.path(),
            &SecurityGate::new(runner.clone())
        )
        .await,
        Err(CliError::ManifestTooLarge { .. })
    ));

    let directory = dir.path().join("manifest-directory");
    std::fs::create_dir(&directory).unwrap();
    assert!(matches!(
        run_security(
            &security_cli(&directory, &output),
            dir.path(),
            &SecurityGate::new(runner.clone())
        )
        .await,
        Err(CliError::ManifestNotRegular)
    ));

    assert!(matches!(
        run_security(
            &security_cli(std::path::Path::new("/dev/null"), &output),
            dir.path(),
            &SecurityGate::new(runner)
        )
        .await,
        Err(CliError::ManifestNotRegular)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!output.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn fifo_manifest_is_rejected_without_blocking() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("manifest.fifo");
    let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
    let output = dir.path().join("security.json");
    let runner = StubRunner::default();
    let calls = Arc::clone(&runner.calls);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        run_security(
            &security_cli(&fifo, &output),
            dir.path(),
            &SecurityGate::new(runner),
        ),
    )
    .await
    .expect("nonblocking manifest open must not wait for a FIFO writer");

    assert!(matches!(result, Err(CliError::ManifestNotRegular)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!output.exists());
}

#[tokio::test]
async fn successful_security_run_persists_a_bounded_integrity_checked_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.json");
    let output = dir.path().join("evidence/security.json");
    let manifest_bytes = manifest(64 * 1024);
    std::fs::write(&manifest_path, &manifest_bytes).unwrap();
    let cli = parse_args([
        "security".into(),
        "--manifest".into(),
        manifest_path.display().to_string(),
        "--output".into(),
        output.display().to_string(),
    ])
    .unwrap();
    let runner = StubRunner {
        calls: Arc::new(AtomicUsize::new(0)),
        canary: Some("cli-secret-canary".into()),
    };
    let calls = Arc::clone(&runner.calls);

    let bundle = run_security(&cli, dir.path(), &SecurityGate::new(runner))
        .await
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(bundle.verdict, CertificationVerdict::Passed);
    assert_eq!(bundle.results.len(), 4);
    assert_eq!(
        bundle.catalog_sha256,
        release_gates::security_catalog_sha256()
    );
    assert!(bundle
        .results
        .iter()
        .all(|result| result.required && result.status == GateStatus::Passed));
    assert_eq!(
        bundle.manifest_sha256,
        format!("{:x}", Sha256::digest(manifest_bytes.as_bytes()))
    );
    assert_eq!(bundle.bundle_sha256().unwrap().len(), 64);

    let persisted = std::fs::read(&output).unwrap();
    assert!(persisted.len() <= 64 * 1024);
    assert!(!String::from_utf8_lossy(&persisted).contains("cli-secret-canary"));
    let decoded: CertificationBundle = serde_json::from_slice(&persisted).unwrap();
    assert_eq!(
        decoded.bundle_sha256().unwrap(),
        bundle.bundle_sha256().unwrap()
    );
    assert_eq!(decoded.results.len(), 4);
    assert_eq!(decoded.catalog_sha256, bundle.catalog_sha256);
}

#[tokio::test]
async fn bundle_output_bound_is_enforced_without_a_partial_destination() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.json");
    let output = dir.path().join("security.json");
    std::fs::write(&manifest_path, manifest(1)).unwrap();
    let cli = parse_args([
        "security".into(),
        "--manifest".into(),
        manifest_path.display().to_string(),
        "--output".into(),
        output.display().to_string(),
    ])
    .unwrap();

    assert!(
        run_security(&cli, dir.path(), &SecurityGate::new(StubRunner::default()))
            .await
            .is_err()
    );
    assert!(!output.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn manifest_and_output_aliases_are_rejected_before_checks_or_side_effects() {
    use std::os::unix::fs::symlink;

    async fn assert_rejected(
        repo_root: &std::path::Path,
        manifest_arg: String,
        output_arg: String,
        manifest_target: &std::path::Path,
    ) {
        let before = std::fs::read(manifest_target).unwrap();
        let cli = parse_args([
            "security".into(),
            "--manifest".into(),
            manifest_arg,
            "--output".into(),
            output_arg,
        ])
        .unwrap();
        let runner = StubRunner::default();
        let calls = Arc::clone(&runner.calls);

        assert!(matches!(
            run_security(&cli, repo_root, &SecurityGate::new(runner)).await,
            Err(CliError::PathConflict)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(std::fs::read(manifest_target).unwrap(), before);
    }

    let dir = tempfile::tempdir().unwrap();
    let lexical = dir.path().join("lexical.json");
    std::fs::write(&lexical, manifest(64 * 1024)).unwrap();
    assert_rejected(
        dir.path(),
        lexical.display().to_string(),
        lexical.display().to_string(),
        &lexical,
    )
    .await;

    let relative = dir.path().join("relative.json");
    std::fs::write(&relative, manifest(64 * 1024)).unwrap();
    assert_rejected(
        dir.path(),
        "relative.json".into(),
        relative.display().to_string(),
        &relative,
    )
    .await;

    let hardlink_manifest = dir.path().join("hardlink-manifest.json");
    let hardlink_output = dir.path().join("hardlink-output.json");
    std::fs::write(&hardlink_manifest, manifest(64 * 1024)).unwrap();
    std::fs::hard_link(&hardlink_manifest, &hardlink_output).unwrap();
    assert_rejected(
        dir.path(),
        hardlink_manifest.display().to_string(),
        hardlink_output.display().to_string(),
        &hardlink_manifest,
    )
    .await;

    let symlink_target = dir.path().join("symlink-target.json");
    let manifest_symlink = dir.path().join("manifest-symlink.json");
    std::fs::write(&symlink_target, manifest(64 * 1024)).unwrap();
    symlink(&symlink_target, &manifest_symlink).unwrap();
    assert_rejected(
        dir.path(),
        manifest_symlink.display().to_string(),
        symlink_target.display().to_string(),
        &symlink_target,
    )
    .await;

    let real_parent = dir.path().join("real-parent");
    let parent_alias = dir.path().join("parent-alias");
    std::fs::create_dir(&real_parent).unwrap();
    let parent_manifest = real_parent.join("manifest.json");
    std::fs::write(&parent_manifest, manifest(64 * 1024)).unwrap();
    symlink(&real_parent, &parent_alias).unwrap();
    assert_rejected(
        dir.path(),
        parent_manifest.display().to_string(),
        parent_alias.join("manifest.json").display().to_string(),
        &parent_manifest,
    )
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn successful_security_run_atomically_replaces_an_existing_output() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.json");
    let output = dir.path().join("security.json");
    std::fs::write(&manifest_path, manifest(64 * 1024)).unwrap();
    std::fs::write(&output, b"stale certification").unwrap();
    let cli = parse_args([
        "security".into(),
        "--manifest".into(),
        manifest_path.display().to_string(),
        "--output".into(),
        output.display().to_string(),
    ])
    .unwrap();

    run_security(&cli, dir.path(), &SecurityGate::new(StubRunner::default()))
        .await
        .unwrap();

    let decoded: CertificationBundle =
        serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
    assert_eq!(decoded.verdict, CertificationVerdict::Passed);
    assert_eq!(decoded.results.len(), 4);
}

#[cfg(unix)]
#[tokio::test]
async fn missing_output_under_a_symlinked_parent_is_resolved_and_persisted_safely() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let real_parent = dir.path().join("real-parent");
    let parent_alias = dir.path().join("parent-alias");
    std::fs::create_dir(&real_parent).unwrap();
    symlink(&real_parent, &parent_alias).unwrap();
    let manifest_path = dir.path().join("manifest.json");
    let output = parent_alias.join("security.json");
    let canonical_output = real_parent.join("security.json");
    std::fs::write(&manifest_path, manifest(64 * 1024)).unwrap();
    let cli = parse_args([
        "security".into(),
        "--manifest".into(),
        manifest_path.display().to_string(),
        "--output".into(),
        output.display().to_string(),
    ])
    .unwrap();

    run_security(&cli, dir.path(), &SecurityGate::new(StubRunner::default()))
        .await
        .unwrap();

    let decoded: CertificationBundle =
        serde_json::from_slice(&std::fs::read(canonical_output).unwrap()).unwrap();
    assert_eq!(decoded.verdict, CertificationVerdict::Passed);
}

#[cfg(unix)]
#[tokio::test]
async fn output_directory_is_pinned_before_checks_when_parent_is_retargeted_and_replaced() {
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    struct RetargetingRunner {
        mutated: AtomicBool,
        safe_parent: PathBuf,
        pinned_parent: PathBuf,
        parent_alias: PathBuf,
        manifest_parent: PathBuf,
    }

    impl ProcessRunner for RetargetingRunner {
        async fn run(&self, spec: &ProcessSpec) -> Result<ProcessOutcome, ProcessFailure> {
            if !self.mutated.swap(true, Ordering::SeqCst) {
                std::fs::rename(&self.safe_parent, &self.pinned_parent).unwrap();
                std::fs::create_dir(&self.safe_parent).unwrap();
                std::fs::remove_file(&self.parent_alias).unwrap();
                symlink(&self.manifest_parent, &self.parent_alias).unwrap();
            }
            Ok(ProcessOutcome {
                exit_code: Some(0),
                stdout: valid_proof_receipt(spec, None),
                stderr: Vec::new(),
            })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let manifest_parent = dir.path().join("manifest-parent");
    let safe_parent = dir.path().join("safe-parent");
    let pinned_parent = dir.path().join("pinned-parent");
    let parent_alias = dir.path().join("output-parent");
    std::fs::create_dir(&manifest_parent).unwrap();
    std::fs::create_dir(&safe_parent).unwrap();
    symlink(&safe_parent, &parent_alias).unwrap();

    let manifest_path = manifest_parent.join("manifest.json");
    let output = parent_alias.join("manifest.json");
    let manifest_bytes = manifest(64 * 1024).into_bytes();
    std::fs::write(&manifest_path, &manifest_bytes).unwrap();
    let cli = parse_args([
        "security".into(),
        "--manifest".into(),
        manifest_path.display().to_string(),
        "--output".into(),
        output.display().to_string(),
    ])
    .unwrap();
    let runner = RetargetingRunner {
        mutated: AtomicBool::new(false),
        safe_parent: safe_parent.clone(),
        pinned_parent: pinned_parent.clone(),
        parent_alias,
        manifest_parent,
    };

    run_security(&cli, dir.path(), &SecurityGate::new(runner))
        .await
        .unwrap();

    assert_eq!(std::fs::read(&manifest_path).unwrap(), manifest_bytes);
    assert!(!safe_parent.join("manifest.json").exists());
    let persisted = std::fs::read(pinned_parent.join("manifest.json")).unwrap();
    let decoded: CertificationBundle = serde_json::from_slice(&persisted).unwrap();
    assert_eq!(decoded.verdict, CertificationVerdict::Passed);
}

#[test]
fn concise_summary_has_one_line_per_check_and_a_final_verdict() {
    let mut results = catalog_results(GateStatus::Passed);
    results[0].duration_ms = 7;
    results[1].duration_ms = 11;
    results[1].status = GateStatus::Blocked;
    let bundle =
        CertificationBundle::try_new("0".repeat(64), results, CertificationVerdict::Blocked)
            .unwrap();

    assert_eq!(
        summary_lines(&bundle),
        vec![
            "security/interface-boundaries: passed",
            "security/adaptive-http-policy: blocked",
            "security/connection-and-workflow-capacity: passed",
            "security/cdp-target-context-policy: passed",
            "release verdict: blocked",
        ]
    );
}

#[test]
fn binary_configuration_errors_exit_two_without_certification_output() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("invalid.json");
    let output = dir.path().join("evidence/security.json");
    std::fs::write(&manifest_path, manifest(0)).unwrap();

    let command_output = std::process::Command::new(env!("CARGO_BIN_EXE_release-gates"))
        .current_dir(dir.path())
        .args([
            "security",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(command_output.status.code(), Some(2));
    assert!(command_output.stdout.is_empty());
    assert!(!output.exists());
    assert!(!output.parent().unwrap().exists());
}

#[test]
fn binary_path_conflicts_exit_two_without_replacing_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.json");
    let manifest_bytes = manifest(64 * 1024);
    std::fs::write(&manifest_path, &manifest_bytes).unwrap();

    let command_output = std::process::Command::new(env!("CARGO_BIN_EXE_release-gates"))
        .current_dir(dir.path())
        .args([
            "security",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--output",
            manifest_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(command_output.status.code(), Some(2));
    assert!(command_output.stdout.is_empty());
    assert_eq!(
        std::fs::read_to_string(manifest_path).unwrap(),
        manifest_bytes
    );
}

#[test]
fn binary_bundle_size_failure_exits_one_after_checks() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.json");
    let output = dir.path().join("security.json");
    std::fs::write(&manifest_path, manifest(1)).unwrap();

    let command_output = std::process::Command::new(env!("CARGO_BIN_EXE_release-gates"))
        .current_dir(dir.path())
        .args([
            "security",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(command_output.status.code(), Some(1));
    assert!(command_output.stdout.is_empty());
    assert!(!output.exists());
}

#[cfg(unix)]
#[test]
fn binary_persistence_failure_exits_one_after_checks() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("manifest.json");
    let output = dir.path().join("security.json");
    std::fs::write(&manifest_path, manifest(64 * 1024)).unwrap();
    std::fs::create_dir(&output).unwrap();

    let command_output = std::process::Command::new(env!("CARGO_BIN_EXE_release-gates"))
        .current_dir(dir.path())
        .args([
            "security",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(command_output.status.code(), Some(1));
    assert!(command_output.stdout.is_empty());
    assert!(output.is_dir());
}

#[test]
fn certification_bundle_rejects_tampering_and_preserves_foreign_temporary_files() {
    let bundle = CertificationBundle::try_new(
        "0".repeat(64),
        catalog_results(GateStatus::Passed),
        CertificationVerdict::Passed,
    )
    .unwrap();
    let mut forged = serde_json::to_value(&bundle).unwrap();
    forged["manifestSha256"] = serde_json::Value::String("1".repeat(64));
    assert!(serde_json::from_value::<CertificationBundle>(forged)
        .unwrap_err()
        .to_string()
        .contains("bundleSha256 does not match"));

    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("security.json");
    let temporary = output.with_extension("json.tmp");
    let foreign_temporary = dir.path().join(".security.json.release-gates-foreign.tmp");
    std::fs::write(&temporary, b"existing temporary bundle").unwrap();
    std::fs::write(&foreign_temporary, b"foreign temporary bundle").unwrap();
    bundle.write_json(&output, 4096).unwrap();
    assert_eq!(
        std::fs::read(&temporary).unwrap(),
        b"existing temporary bundle"
    );
    assert_eq!(
        std::fs::read(&foreign_temporary).unwrap(),
        b"foreign temporary bundle"
    );
    let decoded: CertificationBundle =
        serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
    assert_eq!(decoded.verdict, CertificationVerdict::Passed);
}
