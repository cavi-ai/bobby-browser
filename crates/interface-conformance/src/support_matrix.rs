use types::{Capability, InterfaceOperation};

pub const SUPPORT_MATRIX_BEGIN: &str = "<!-- BEGIN GENERATED INTERFACE SUPPORT -->";
pub const SUPPORT_MATRIX_END: &str = "<!-- END GENERATED INTERFACE SUPPORT -->";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterSupport {
    Unsupported,
    Direct,
    Translated,
}

impl AdapterSupport {
    const fn label(self) -> &'static str {
        match self {
            Self::Unsupported => "—",
            Self::Direct => "direct",
            Self::Translated => "via command",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineSupport {
    EngineAgnostic,
    ChromiumAndFirefox,
}

impl EngineSupport {
    const fn label(self) -> &'static str {
        match self {
            Self::EngineAgnostic => "engine-agnostic",
            Self::ChromiumAndFirefox => "Chromium, Firefox",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperationSupport {
    pub operation: InterfaceOperation,
    pub http: AdapterSupport,
    pub mcp: AdapterSupport,
    pub cdp: AdapterSupport,
    pub acp: AdapterSupport,
    pub engines: EngineSupport,
}

impl OperationSupport {
    pub const fn new(
        operation: InterfaceOperation,
        http: AdapterSupport,
        mcp: AdapterSupport,
        cdp: AdapterSupport,
        acp: AdapterSupport,
        engines: EngineSupport,
    ) -> Self {
        Self {
            operation,
            http,
            mcp,
            cdp,
            acp,
            engines,
        }
    }

    pub const fn adapters(self) -> [AdapterSupport; 4] {
        [self.http, self.mcp, self.cdp, self.acp]
    }
}

const U: AdapterSupport = AdapterSupport::Unsupported;
const D: AdapterSupport = AdapterSupport::Direct;
const T: AdapterSupport = AdapterSupport::Translated;
const RUNTIME: EngineSupport = EngineSupport::EngineAgnostic;
const BROWSERS: EngineSupport = EngineSupport::ChromiumAndFirefox;

const OPERATION_SUPPORT: [OperationSupport; 20] = [
    OperationSupport::new(InterfaceOperation::RuntimeInfo, D, D, D, U, RUNTIME),
    OperationSupport::new(InterfaceOperation::CreateSession, D, D, U, D, BROWSERS),
    OperationSupport::new(InterfaceOperation::ReadSession, D, D, D, U, BROWSERS),
    OperationSupport::new(InterfaceOperation::DeleteSession, D, D, U, D, BROWSERS),
    OperationSupport::new(InterfaceOperation::OpenPage, D, D, D, D, BROWSERS),
    OperationSupport::new(InterfaceOperation::ReadPage, D, D, T, T, BROWSERS),
    OperationSupport::new(InterfaceOperation::ClosePage, T, T, T, T, BROWSERS),
    OperationSupport::new(InterfaceOperation::SubmitCommand, D, D, D, D, BROWSERS),
    OperationSupport::new(InterfaceOperation::CreateCheckpoint, D, D, D, U, BROWSERS),
    OperationSupport::new(InterfaceOperation::ReadCheckpoint, D, D, D, U, BROWSERS),
    OperationSupport::new(InterfaceOperation::RecoverWorkflow, D, D, U, U, BROWSERS),
    OperationSupport::new(InterfaceOperation::ReadArtifact, D, U, U, U, RUNTIME),
    OperationSupport::new(InterfaceOperation::ReadContext, D, D, U, U, BROWSERS),
    OperationSupport::new(InterfaceOperation::CaptureArtifact, T, T, D, U, BROWSERS),
    OperationSupport::new(InterfaceOperation::SubscribeEvents, D, D, D, U, BROWSERS),
    OperationSupport::new(InterfaceOperation::SubmitJob, D, D, U, U, RUNTIME),
    OperationSupport::new(InterfaceOperation::ReadJob, D, D, U, U, RUNTIME),
    OperationSupport::new(InterfaceOperation::CancelJob, D, D, U, U, RUNTIME),
    OperationSupport::new(InterfaceOperation::IssuePrincipal, D, U, U, U, RUNTIME),
    OperationSupport::new(InterfaceOperation::RevokePrincipal, D, U, U, U, RUNTIME),
];

pub const fn operation_support() -> &'static [OperationSupport] {
    &OPERATION_SUPPORT
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionPolicyRequirement {
    pub field: &'static str,
    pub capability: Capability,
    pub enforced_at: InterfaceOperation,
    pub adapters: [bool; 4],
}

impl ExecutionPolicyRequirement {
    pub const fn new(
        field: &'static str,
        capability: Capability,
        enforced_at: InterfaceOperation,
        adapters: [bool; 4],
    ) -> Self {
        Self {
            field,
            capability,
            enforced_at,
            adapters,
        }
    }
}

const EXECUTION_POLICY_REQUIREMENTS: [ExecutionPolicyRequirement; 4] = [
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
];

pub const fn execution_policy_requirements() -> &'static [ExecutionPolicyRequirement] {
    &EXECUTION_POLICY_REQUIREMENTS
}

pub fn render_support_matrix_markdown() -> String {
    let mut output = String::from(SUPPORT_MATRIX_BEGIN);
    output.push_str(
        "\n## Generated operation support\n\n\
         `direct` means the adapter exposes the operation. `via command` means it is reached through `submitCommand`. The capability column is the direct operation gate; translated paths use `browser:mutate` plus nested command requirements.\n\n\
         | Operation | Required capability | HTTP | MCP | CDP | ACP | Engine scope |\n\
         |---|---|---|---|---|---|---|\n",
    );
    for row in operation_support() {
        let capabilities = row
            .operation
            .required()
            .iter()
            .map(|capability| format!("`{}`", capability.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} | {} |\n",
            row.operation.as_str(),
            capabilities,
            row.http.label(),
            row.mcp.label(),
            row.cdp.label(),
            row.acp.label(),
            row.engines.label(),
        ));
    }
    output.push_str(
        "\n## Execution-policy gates\n\n\
         These fields are opt-ins. The capability is checked at the listed operation before protected behavior runs.\n\n\
         | `executionPolicy` field | Capability | Enforced at | HTTP | MCP | CDP | ACP | Engines |\n\
         |---|---|---|---|---|---|---|---|\n",
    );
    for requirement in execution_policy_requirements() {
        let support = requirement
            .adapters
            .map(|supported| if supported { "yes" } else { "—" });
        output.push_str(&format!(
            "| `{}` | `{}` | `{}` | {} | {} | {} | {} | Chromium, Firefox |\n",
            requirement.field,
            requirement.capability.as_str(),
            requirement.enforced_at.as_str(),
            support[0],
            support[1],
            support[2],
            support[3],
        ));
    }
    output.push_str(&format!("\n{SUPPORT_MATRIX_END}"));
    output
}
