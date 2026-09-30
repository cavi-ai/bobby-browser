use std::collections::HashSet;

use interface_conformance::{
    execution_policy_requirements, operation_support, render_preset_matrix_markdown,
    render_support_matrix_markdown, AdapterSupport, EngineSupport, ExecutionPolicyRequirement,
    PRESET_MATRIX_BEGIN, PRESET_MATRIX_END, SUPPORT_MATRIX_BEGIN, SUPPORT_MATRIX_END,
};
use types::{Capability, InterfaceOperation};

#[test]
fn support_matrix_covers_every_interface_operation_once() {
    let rows = operation_support();
    assert_eq!(rows.len(), InterfaceOperation::ALL.len());

    let mut seen = HashSet::new();
    for row in rows {
        assert!(
            seen.insert(row.operation),
            "duplicate operation: {:?}",
            row.operation
        );
        assert!(row
            .adapters()
            .iter()
            .any(|support| *support != AdapterSupport::Unsupported));
    }

    assert!(InterfaceOperation::ALL
        .iter()
        .all(|operation| seen.contains(operation)));
}

#[test]
fn support_matrix_records_current_adapter_and_engine_boundaries() {
    let recover = operation_support()
        .iter()
        .find(|row| row.operation == InterfaceOperation::RecoverWorkflow)
        .unwrap();
    assert_eq!(recover.http, AdapterSupport::Direct);
    assert_eq!(recover.mcp, AdapterSupport::Direct);
    assert_eq!(recover.cdp, AdapterSupport::Unsupported);
    assert_eq!(recover.acp, AdapterSupport::Direct);
    assert_eq!(recover.engines, EngineSupport::ChromiumAndFirefox);

    let artifact = operation_support()
        .iter()
        .find(|row| row.operation == InterfaceOperation::CaptureArtifact)
        .unwrap();
    assert_eq!(artifact.http, AdapterSupport::Translated);
    assert_eq!(artifact.mcp, AdapterSupport::Translated);
    assert_eq!(artifact.cdp, AdapterSupport::Direct);
    assert_eq!(artifact.acp, AdapterSupport::Unsupported);

    let submit = operation_support()
        .iter()
        .find(|row| row.operation == InterfaceOperation::SubmitCommand)
        .unwrap();
    assert!(submit
        .adapters()
        .iter()
        .all(|support| *support == AdapterSupport::Direct));
}

#[test]
fn acp_exposes_context_checkpoint_and_recovery_operations() {
    for operation in [
        InterfaceOperation::ReadContext,
        InterfaceOperation::CreateCheckpoint,
        InterfaceOperation::ReadCheckpoint,
        InterfaceOperation::RecoverWorkflow,
    ] {
        let support = operation_support()
            .iter()
            .find(|row| row.operation == operation)
            .unwrap();
        assert_eq!(support.acp, AdapterSupport::Direct, "{operation:?}");
    }
}

#[test]
fn execution_policy_requirements_are_complete() {
    assert_eq!(
        execution_policy_requirements(),
        &[
            ExecutionPolicyRequirement::new(
                "javascriptEvaluation",
                Capability::JavascriptEvaluate,
                InterfaceOperation::SubmitCommand,
                [true, true, true, false],
            ),
            ExecutionPolicyRequirement::new(
                "visionAssist",
                Capability::VisionAssist,
                InterfaceOperation::SubmitCommand,
                [true, true, false, true],
            ),
            ExecutionPolicyRequirement::new(
                "fingerprint",
                Capability::BrowserFingerprint,
                InterfaceOperation::CreateSession,
                [true, true, false, false],
            ),
            ExecutionPolicyRequirement::new(
                "humanize",
                Capability::BrowserHumanize,
                InterfaceOperation::CreateSession,
                [true, true, false, false],
            ),
        ]
    );
}

#[test]
fn generated_documentation_matches_the_support_source() {
    let documentation = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/bobby-browser/source/pages/concepts/capabilities.md"),
    )
    .unwrap();
    let start = documentation.find(SUPPORT_MATRIX_BEGIN).unwrap();
    let end = documentation.find(SUPPORT_MATRIX_END).unwrap() + SUPPORT_MATRIX_END.len();
    assert_eq!(&documentation[start..end], render_support_matrix_markdown());
}

#[test]
fn generated_preset_matrix_matches_the_preset_table() {
    let documentation = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/bobby-browser/source/pages/concepts/capabilities.md"),
    )
    .unwrap();
    let start = documentation.find(PRESET_MATRIX_BEGIN).unwrap();
    let end = documentation.find(PRESET_MATRIX_END).unwrap() + PRESET_MATRIX_END.len();
    assert_eq!(&documentation[start..end], render_preset_matrix_markdown());
}

#[test]
fn preset_matrix_names_every_withheld_admin_operation() {
    let matrix = render_preset_matrix_markdown();
    let agent = matrix
        .split("\n- `")
        .find(|section| section.starts_with("agent`"))
        .expect("agent section");
    assert!(agent.contains("`issuePrincipal`"), "{agent}");
    assert!(agent.contains("`revokePrincipal`"), "{agent}");
    let claude = matrix
        .split("\n- `")
        .find(|section| section.starts_with("claude`"))
        .expect("claude section");
    assert!(
        claude.contains("`executionPolicy.javascriptEvaluation`"),
        "{claude}"
    );
    assert!(claude.contains("`executionPolicy.fingerprint`"), "{claude}");
    assert!(!claude.contains("`browser:mutate`"), "{claude}");
}
