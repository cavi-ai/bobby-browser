use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use release_gates::{
    cli::{
        exit_code, parse_args, run_security, summary_lines, CertificationBundle, CliError, Command,
    },
    CertificationVerdict, GateResult, GateStatus, ProcessFailure, ProcessOutcome, ProcessRunner,
    ProcessSpec, SecurityGate,
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
    async fn run(&self, _: &ProcessSpec) -> Result<ProcessOutcome, ProcessFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ProcessOutcome {
            exit_code: Some(0),
            stdout: self.canary.clone().unwrap_or_default().into_bytes(),
            stderr: Vec::new(),
        })
    }
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

    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(bundle.verdict, CertificationVerdict::Passed);
    assert_eq!(bundle.results.len(), 3);
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
    assert_eq!(decoded.results.len(), 3);
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
    assert_eq!(decoded.results.len(), 3);
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
        async fn run(&self, _: &ProcessSpec) -> Result<ProcessOutcome, ProcessFailure> {
            if !self.mutated.swap(true, Ordering::SeqCst) {
                std::fs::rename(&self.safe_parent, &self.pinned_parent).unwrap();
                std::fs::create_dir(&self.safe_parent).unwrap();
                std::fs::remove_file(&self.parent_alias).unwrap();
                symlink(&self.manifest_parent, &self.parent_alias).unwrap();
            }
            Ok(ProcessOutcome {
                exit_code: Some(0),
                stdout: Vec::new(),
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
    let bundle = CertificationBundle::new(
        "0".repeat(64),
        vec![
            GateResult::new(
                "security",
                "interface-boundaries",
                true,
                GateStatus::Passed,
                7,
                vec![],
            ),
            GateResult::new(
                "security",
                "adaptive-http-policy",
                true,
                GateStatus::Blocked,
                11,
                vec![],
            ),
        ],
        CertificationVerdict::Blocked,
    );

    assert_eq!(
        summary_lines(&bundle),
        vec![
            "security/interface-boundaries: passed",
            "security/adaptive-http-policy: blocked",
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
fn certification_bundle_rejects_tampering_and_preserves_foreign_temporary_files() {
    let bundle = CertificationBundle::new(
        "0".repeat(64),
        vec![GateResult::new(
            "security",
            "check",
            true,
            GateStatus::Passed,
            1,
            vec![],
        )],
        CertificationVerdict::Passed,
    );
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
