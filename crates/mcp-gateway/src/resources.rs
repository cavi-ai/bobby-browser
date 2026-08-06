use std::{collections::BTreeMap, fmt, path::PathBuf, sync::Arc};

use artifact_store::{ArtifactRecord, ArtifactStore};
use interface_core::{
    ArtifactContent, ArtifactReader, ArtifactReference, CapabilityHandle, InterfaceResult,
};
use sha2::Digest as _;
use tokio::sync::RwLock;
use types::{
    CommandEnvelope, CommandOutcome, ErrorLayer, Evidence, InterfaceError, InterfaceErrorCode,
    RequestContext, SessionId,
};

#[derive(Clone)]
pub struct ArtifactResources {
    reader: Option<ArtifactReader>,
    entries: Arc<RwLock<BTreeMap<String, (SessionId, ArtifactReference)>>>,
    max_entries: usize,
    artifact_store: Option<ArtifactStore>,
    downloads_root: Option<PathBuf>,
    max_download_bytes: usize,
}

impl Default for ArtifactResources {
    fn default() -> Self {
        Self {
            reader: None,
            entries: Arc::new(RwLock::new(BTreeMap::new())),
            max_entries: 1,
            artifact_store: None,
            downloads_root: None,
            max_download_bytes: 1,
        }
    }
}

impl ArtifactResources {
    pub fn new(reader: ArtifactReader, max_entries: usize) -> Self {
        assert!(max_entries > 0, "artifact resource bound must be positive");
        Self {
            reader: Some(reader),
            entries: Arc::new(RwLock::new(BTreeMap::new())),
            max_entries,
            artifact_store: None,
            downloads_root: None,
            max_download_bytes: 1,
        }
    }

    pub fn production(
        reader: ArtifactReader,
        artifact_store: ArtifactStore,
        downloads_root: impl Into<PathBuf>,
        max_download_bytes: usize,
        max_entries: usize,
    ) -> Self {
        assert!(
            max_download_bytes > 0,
            "download import bound must be positive"
        );
        let mut resources = Self::new(reader, max_entries);
        resources.artifact_store = Some(artifact_store);
        resources.downloads_root = Some(downloads_root.into());
        resources.max_download_bytes = max_download_bytes;
        resources
    }

    async fn register_trusted(
        &self,
        session_id: SessionId,
        reference: ArtifactReference,
    ) -> Result<(), ArtifactCatalogFull> {
        let artifact_id = reference.artifact_id().to_owned();
        let mut entries = self.entries.write().await;
        if !entries.contains_key(&artifact_id) && entries.len() >= self.max_entries {
            return Err(ArtifactCatalogFull);
        }
        entries.insert(artifact_id, (session_id, reference));
        Ok(())
    }

    /// Admits runtime artifact evidence only after `ArtifactReader` independently verifies the
    /// committed artifact and issues an opaque reference. Caller-provided MCP arguments never
    /// provide a path, ownership assertion, or `ArtifactRecord`.
    pub(crate) async fn register_outcome(
        &self,
        handle: &CapabilityHandle,
        context: &RequestContext,
        envelope: &CommandEnvelope,
        outcome: &CommandOutcome,
    ) -> ArtifactAdmission {
        let mut admission = ArtifactAdmission::default();
        let evidence = match outcome {
            CommandOutcome::Completed { evidence, .. }
            | CommandOutcome::NeedsReconciliation { evidence, .. }
            | CommandOutcome::Failed { evidence, .. } => evidence,
            _ => return admission,
        };
        for item in evidence {
            let (kind, result) = match item {
                Evidence::Screenshot {
                    artifact_id,
                    media_type,
                    width,
                    height,
                    bytes,
                    sha256,
                } => {
                    admission.attempted += 1;
                    let result = envelope
                        .page_id
                        .clone()
                        .ok_or_else(|| resource_error(context, InterfaceErrorCode::InvalidRequest));
                    let result = match result {
                        Ok(page_id) => {
                            let record = ArtifactRecord {
                                artifact_id: artifact_id.clone(),
                                page_id,
                                media_type: media_type.clone(),
                                width: *width,
                                height: *height,
                                bytes: *bytes,
                                sha256: sha256.clone(),
                            };
                            self.admit_record(handle, context, envelope, &record).await
                        }
                        Err(error) => Err(error),
                    };
                    ("screenshot", result)
                }
                Evidence::Download {
                    filename,
                    path,
                    bytes,
                    sha256,
                } => {
                    admission.attempted += 1;
                    admission.download_uris.insert(sha256.clone(), None);
                    let result = self
                        .download_record(context, envelope, filename, path, *bytes, sha256)
                        .await;
                    let result = match result {
                        Ok(record) => self.admit_record(handle, context, envelope, &record).await,
                        Err(error) => Err(error),
                    };
                    ("download", result)
                }
                _ => continue,
            };
            match result {
                Ok(artifact_id) => {
                    admission.admitted += 1;
                    if let Evidence::Download { sha256, .. } = item {
                        admission
                            .download_uris
                            .insert(sha256.clone(), Some(format!("artifact://{artifact_id}")));
                    }
                }
                Err(error) => admission.failures.push(ArtifactAdmissionFailure {
                    kind,
                    code: error.code,
                }),
            }
        }
        admission
    }

    async fn admit_record(
        &self,
        handle: &CapabilityHandle,
        context: &RequestContext,
        envelope: &CommandEnvelope,
        record: &ArtifactRecord,
    ) -> InterfaceResult<String> {
        let reader = self
            .reader
            .as_ref()
            .ok_or_else(|| resource_error(context, InterfaceErrorCode::ArtifactDenied))?;
        let reference = reader
            .register(handle, context, &envelope.session_id, record)
            .await?;
        let artifact_id = reference.artifact_id().to_owned();
        self.register_trusted(envelope.session_id.clone(), reference)
            .await
            .map_err(|_| resource_error(context, InterfaceErrorCode::ResourceExhausted))?;
        Ok(artifact_id)
    }

    async fn download_record(
        &self,
        context: &RequestContext,
        envelope: &CommandEnvelope,
        filename: &str,
        path: &str,
        expected_bytes: u64,
        expected_sha256: &str,
    ) -> InterfaceResult<ArtifactRecord> {
        let page_id = envelope
            .page_id
            .clone()
            .ok_or_else(|| resource_error(context, InterfaceErrorCode::InvalidRequest))?;
        if path == expected_sha256 && valid_sha256(path) {
            return Ok(ArtifactRecord {
                artifact_id: path.to_owned(),
                page_id,
                media_type: "application/octet-stream".to_owned(),
                width: 0,
                height: 0,
                bytes: expected_bytes,
                sha256: expected_sha256.to_owned(),
            });
        }
        let store = self
            .artifact_store
            .as_ref()
            .ok_or_else(|| resource_error(context, InterfaceErrorCode::ArtifactDenied))?;
        let downloads_root = self
            .downloads_root
            .as_ref()
            .ok_or_else(|| resource_error(context, InterfaceErrorCode::ArtifactDenied))?;
        let session_root = downloads_root.join(envelope.session_id.0.to_string());
        let canonical_root = tokio::fs::canonicalize(session_root)
            .await
            .map_err(|_| resource_error(context, InterfaceErrorCode::ArtifactDenied))?;
        let canonical_path = tokio::fs::canonicalize(path)
            .await
            .map_err(|_| resource_error(context, InterfaceErrorCode::ArtifactDenied))?;
        if !canonical_path.starts_with(&canonical_root) {
            return Err(resource_error(context, InterfaceErrorCode::ArtifactDenied));
        }
        let metadata = tokio::fs::metadata(&canonical_path)
            .await
            .map_err(|_| resource_error(context, InterfaceErrorCode::ArtifactDenied))?;
        if !metadata.is_file()
            || metadata.len() != expected_bytes
            || metadata.len() > self.max_download_bytes as u64
        {
            return Err(resource_error(context, InterfaceErrorCode::ArtifactDenied));
        }
        let bytes = tokio::fs::read(&canonical_path)
            .await
            .map_err(|_| resource_error(context, InterfaceErrorCode::ArtifactDenied))?;
        if bytes.len() as u64 != expected_bytes
            || hex::encode(sha2::Sha256::digest(&bytes)) != expected_sha256
        {
            return Err(resource_error(context, InterfaceErrorCode::ArtifactDenied));
        }
        store
            .put(
                &envelope.session_id,
                &page_id,
                "application/octet-stream",
                safe_extension(filename),
                &bytes,
                self.max_download_bytes,
            )
            .await
            .map_err(|_| resource_error(context, InterfaceErrorCode::ArtifactDenied))
    }

    pub(crate) async fn list(&self) -> Vec<String> {
        self.entries.read().await.keys().cloned().collect()
    }

    pub(crate) async fn read(
        &self,
        handle: &CapabilityHandle,
        context: &RequestContext,
        artifact_id: &str,
    ) -> InterfaceResult<Option<ArtifactContent>> {
        let Some(reader) = &self.reader else {
            return Ok(None);
        };
        let Some((session_id, reference)) = self.entries.read().await.get(artifact_id).cloned()
        else {
            return Ok(None);
        };
        reader
            .read(handle, context, &session_id, &reference)
            .await
            .map(Some)
    }
}

#[derive(Default)]
pub(crate) struct ArtifactAdmission {
    attempted: usize,
    admitted: usize,
    download_uris: BTreeMap<String, Option<String>>,
    failures: Vec<ArtifactAdmissionFailure>,
}

struct ArtifactAdmissionFailure {
    kind: &'static str,
    code: InterfaceErrorCode,
}

impl ArtifactAdmission {
    pub(crate) fn apply_to_mcp_value(
        &self,
        value: &mut serde_json::Value,
        command_id: &types::CommandId,
    ) {
        redact_download_paths(value, &self.download_uris);
        if self.failures.is_empty() {
            return;
        }
        let status = if self.admitted == 0 {
            "failed"
        } else {
            "partial"
        };
        let failures = self
            .failures
            .iter()
            .map(|failure| serde_json::json!({"kind":failure.kind,"code":failure.code}))
            .collect::<Vec<_>>();
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "artifactRegistration".to_owned(),
                serde_json::json!({
                    "status":status,
                    "commandId":command_id,
                    "attempted":self.attempted,
                    "admitted":self.admitted,
                    "failures":failures,
                    "retryable":false,
                    "reconciliationRequired":true
                }),
            );
        }
    }
}

fn redact_download_paths(
    value: &mut serde_json::Value,
    download_uris: &BTreeMap<String, Option<String>>,
) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                redact_download_paths(value, download_uris);
            }
        }
        serde_json::Value::Object(object) => {
            if object.get("kind") == Some(&serde_json::json!("download")) {
                let existing_uri = object
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .filter(|path| path.starts_with("artifact://"));
                let replacement = existing_uri.unwrap_or_else(|| {
                    object
                        .get("sha256")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|sha256| download_uris.get(sha256))
                        .and_then(Option::as_deref)
                        .unwrap_or("[redacted-unavailable-artifact]")
                });
                object.insert(
                    "path".to_owned(),
                    serde_json::Value::String(replacement.to_owned()),
                );
            }
            for value in object.values_mut() {
                redact_download_paths(value, download_uris);
            }
        }
        _ => {}
    }
}

pub(crate) fn redact_mcp_download_paths(value: &mut serde_json::Value) {
    redact_download_paths(value, &BTreeMap::new());
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn safe_extension(filename: &str) -> &str {
    filename
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 10
                && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        .unwrap_or("bin")
}

fn resource_error(context: &RequestContext, code: InterfaceErrorCode) -> InterfaceError {
    InterfaceError {
        code,
        layer: ErrorLayer::Interface,
        message: "runtime interface request failed".to_owned(),
        correlation_id: context.correlation_id.clone(),
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: false,
        required_capability: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactCatalogFull;

impl fmt::Display for ArtifactCatalogFull {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("artifact resource catalog capacity exhausted")
    }
}

impl std::error::Error for ArtifactCatalogFull {}

const CAPABILITIES_URI: &str = "bobby://capabilities";
const FAILURE_TAXONOMY_URI: &str = "bobby://failure-taxonomy";
const INTENTS_URI: &str = "bobby://intents";
const PRIMITIVES_URI: &str = "bobby://primitives";

/// Static reference resources: pullable on demand instead of billed to every
/// `tools/list`. Every claim here must trace back to source -- see the
/// `Capability` enum, `required_capabilities`, `ErrorCode`,
/// `CommandOutcome::NeedsReconciliation`, and `IntentCommand::class` for what
/// backs each document.
const CAPABILITIES_BODY: &str = r#"# Capabilities

Each bearer token grants a set of capability strings (`Capability`, `crates/types/src/auth.rs`).
A tool is advertised in `tools/list` and callable only when the bearer holds
every capability `required_capabilities` names for it (`crates/mcp-gateway/src/server.rs`).

- `session:read` -- read-only visibility into runtime and session state.
  Gates `runtime_info`, `session_list`, `events_read`.
- `session:write` -- create and close sessions. Gates `session_create`,
  `session_close`.
- `page:read` -- read a page's form-control inventory. Gates `form_snapshot`.
- `page:write` -- open a page in an owned session. Gates `page_open`.
  `page_open` also requires `browser:mutate`, checked only at call time, when
  the same call navigates the new page to a URL.
- `browser:mutate` -- the base capability for acting on or reading a page.
  Gates on its own: `navigate`, `click`, `type_text`, `inspect`, `screenshot`,
  `wait_for`, `page_list`, `page_close`, `page_activate`, `a11y_snapshot`,
  `pdf`, `dialog`, `emulate`, `network_log`, `cookie_get`, `cookie_set`,
  `cookie_delete`, and `control_action` for every action except `setFiles`.
  Required alongside one more capability for `extract_structured`
  (+ `vision:assist`), `download_url` (+ `file:download`), `upload_files`
  (+ `file:upload`), `control_action` with a `setFiles` action
  (+ `file:upload`), `evaluate_javascript` (+ `javascript:evaluate`), and all
  eight `intent_*` tools (+ `intent:execute`).
- Note: `command_execute` is a tool name, not a capability string -- it is
  advertised in `tools/list` on `browser:mutate` alone, but that is not a way
  around the gates above. Whatever extra capability the *wrapped* command
  needs is enforced at call time, before the command is submitted
  (`command_extra_capabilities`, `crates/sdk-core/src/interface.rs`), and the
  check is exhaustive by command variant:
  - `uploadFiles`, and `controlAction` with a `setFiles` action:
    `file:upload`.
  - `downloadUrl` and `clickAndWaitForDownload`: `file:download`.
  - `evaluateJavaScript`: `javascript:evaluate`.
  - `extractStructured`: `vision:assist`.
  - every intent: `intent:execute`; a `fill` whose value is `files`, or a
    `completeForm` with any `files` field, additionally needs `file:upload`.
  - every other primitive: nothing beyond `browser:mutate`.
- `file:upload` -- gates `upload_files` (with `browser:mutate`), among
  others: `control_action` with a `setFiles` action and file-carrying intent
  fields need it too -- see above.
- `file:download` -- gates `download_url` (with `browser:mutate`), among
  others: `clickAndWaitForDownload` (reachable via `command_execute`, see
  `bobby://primitives`) needs it too.
- `javascript:evaluate` -- gates `evaluate_javascript` (with `browser:mutate`).
- `intent:execute` -- gates `intent_locate`, `intent_fill`,
  `intent_complete_form`, `intent_submit_and_verify`, `intent_wait_for_state`,
  `intent_follow`, `intent_dismiss_obstruction`, `intent_extract` (each also
  with `browser:mutate`). See `bobby://intents` for what each one does.
- `vision:assist` -- gates `extract_structured` (with `browser:mutate`) up
  front. It is also half of a deny-by-default double gate on the
  vision-fallback resolution path inside every `intent_*` tool: holding the
  capability is necessary but not sufficient on its own -- the session must
  also opt in and a vision provider must be configured. The configured
  endpoint must be reachable at runtime (for local OpenAI, run
  `bobby vision-proxy` and point `[vision].endpoint_url` at it). That
  escalation-time check runs per stuck resolution, not per tool call, so it is
  not part of `required_capabilities`.
- `artifact:read` -- required to list or read live `artifact://` resources.
  The static `bobby://` documents (this taxonomy, capabilities, intents,
  primitives) are deliberately NOT gated: an agent that just hit
  `missingCapability` is exactly the one that needs the repair docs.
- `artifact:capture` -- required to register a command's captured evidence
  (screenshots, downloads) as a readable `artifact://` resource. Checked when
  evidence is admitted after a command completes, not by
  `required_capabilities`.
- `recovery:read` -- gates `recovery_status`.
- `recovery:write` -- gates `checkpoint_save`, `workflow_recover`.
- `authority:admin` -- not required by any tool in this gateway. It guards
  principal issuance and revocation, which are not exposed as MCP tools.
- `browser:fingerprint` -- required at `session_create` time to set
  `executionPolicy.fingerprint`. Checked when the session is created, not by
  `required_capabilities`; a principal without it cannot materialize a
  fingerprint-spoofing session at all.
- `browser:humanize` -- required at `session_create` time to set
  `executionPolicy.humanize`. Same creation-time gate as
  `browser:fingerprint`.
"#;

const FAILURE_TAXONOMY_BODY: &str = r#"# Failure taxonomy

Every runtime *command* failure carries one `ErrorCode`
(`crates/types/src/outcomes.rs`); those are the "Error codes" section below.
They are not the only way a call fails, so do not read that list as the whole
vocabulary:

- A request the MCP layer rejects before any command exists carries a
  protocol-layer reason string instead of an `ErrorCode` -- see "Protocol-layer
  rejections".
- `InterfaceErrorCode` (`crates/types/src/interface.rs`) is a separate,
  RPC-layer enum whose names overlap this one. Where the same name means two
  different things, the entry below says so -- see `notFound` and
  `resourceExhausted`. Every RPC-layer code and its repair is in
  "RPC-layer rejections".

Repair actions below are the general pattern for each code; where a specific
tool's own description gives a more precise repair, that tool description
wins for that tool.

## RPC-layer rejections

A call that fails before a command runs (auth, capability, routing) returns a
JSON-RPC error whose `data.interfaceError` carries one of these codes
(`crates/types/src/interface.rs`). The `message` is deliberately generic;
the code, `retryable`, `retryAfterMs`, and `requiredCapability` fields are
the signal:

- `authenticationFailed` -- the bearer did not verify. Re-source the
  credential (`bootstrap.env` or `AUTOMATION_RUNTIME_BOOTSTRAP_*`); do not
  retry with the same token.
- `tokenExpired` -- the credential's `expiresAt` passed. Run `bobby init
  --force` and update the host's environment, then reconnect.
- `missingCapability` -- the principal lacks the named capability; the error
  carries `requiredCapability`. Re-issue the credential with that capability
  (see bobby://capabilities) or pick a tool the current grant covers.
- `idempotencyConflict` -- the idempotency key was already used with a
  different request body. Mint a fresh key; never reuse a key across
  different calls.
- `invalidIdempotencyKey` -- the key is malformed (wrong length/characters).
  Fix the key and retry; the call had no effect.
- `malformedScope` -- the request's scope (session/page ids) is
  structurally invalid. Re-read the ids from `session_list` / `page_list`.
- `artifactDenied` -- the artifact is not readable by this principal (wrong
  owner or past retention). Re-capture it with a command this principal owns.
- `unsupportedInterfaceVersion` -- the client's interface version header is
  not one this runtime serves. Downgrade/upgrade the client to match
  `runtime_info`'s advertised version.
- `unsupportedOperation` -- the operation does not exist on this surface.
  Check the tool name against `tools/list`.
- `invalidRequest` -- the request shape itself is invalid. Fix and retry;
  nothing ran.
- `deadlineExceeded` -- the request's deadline passed before dispatch.
  Re-issue with a longer deadline; retryable if the command is Replayable.
- `notFound` / `resourceExhausted` / `internal` -- see their entries below;
  the RPC-layer and command-layer meanings are called out there.


Repair actions below are the general pattern for each code; where a specific
tool's own description gives a more precise repair, that tool description
wins for that tool.

## `NeedsReconciliation` -- an outcome, not an error code

`CommandOutcome::NeedsReconciliation` means the runtime cannot say whether a
mutating command's side effect already happened. It carries an error but is
not itself an `ErrorCode` value -- do not treat it like a plain failure.

It surfaces when the durable journal write for a command failed after the
command may have already reached the browser, for a Boundary-class command or
a `download_url` (which always needs reconciliation regardless of class), or
when a restart finds a non-Replayable command left mid-flight (`Executing` or
`Verifying`) in the journal with no proof the browser action never ran.

**Never blindly retry this outcome.** Retrying can resubmit an action that
already landed -- a duplicate form submission, a second download. Call
`recovery_status` for the workflow first, and follow `workflow_recover` if a
checkpoint exists. Over HTTP this outcome answers `409 Conflict`. Over MCP,
`intent_submit_and_verify` and a `boundary: true` `intent_follow` are the
tools most likely to produce it.

## Error codes

- `invalidRequest` -- the call's own arguments are malformed or out of range
  (a disallowed URL scheme, an out-of-range scale or viewport, too many
  cookies in one call). Repair: fix the argument and resubmit; nothing ran.
- `notFound` -- a resource named inside an already-accepted command turns out
  to be stale once the command actually runs -- most commonly a page id that
  is no longer open (nearly every per-page primitive shares one "page not
  found" check). Repair: re-list with `page_list` and use a current id. A
  *session* this principal doesn't own, or that doesn't exist, is caught
  earlier than that -- before any command outcome is produced -- and answers
  as a top-level JSON-RPC error rather than inside a tool's structured
  result; the wire code reads the same, but it isn't this per-command code.
  Repair there is the same shape: re-list with `session_list` and use a
  current id.
- `deadlineExceeded` -- the command's own deadline elapsed before it
  finished. Repair: confirm the condition is actually reachable, then retry
  with a longer timeout or deadline.
- `browserLaunchFailed` -- launching a session's browser engine failed, at
  any step: creating its profile or download directories, building the
  launch configuration, or starting the engine process itself. Repair: this
  is an environment problem, not a bad call; retry `session_create`, and
  escalate if it persists.
- `browserCommandFailed` -- the browser engine reported a driver-level
  failure executing an otherwise well-formed command. Repair: retry the same
  call; recreate the session or page if it keeps failing.
- `verificationFailed` -- the action ran, but its result didn't verify: a
  fill's committed value violates the browser's own constraint-validity
  state, or a `type_text`/`click` call carrying an expected-URL guard finds
  the page already navigated away from it. Repair: read the returned
  validation detail, correct the specific thing that failed, then retry
  only that step.
- `journalFailed` -- the durable outcome journal failed to record a
  command's result. Repair: for the plain retryable form, resubmit with the
  same idempotency key; see `NeedsReconciliation` above for when it may have
  already executed.
- `resourceExhausted` -- the worker pool has no capacity for a command that
  needs a browser lease (it is shutting down, or a launch/cleanup task
  itself failed), or an engine-side per-page tracking cap was hit. Repair:
  free capacity -- close an idle session, or an idle page if the engine
  enforces a page cap -- before retrying. As with `notFound`, an
  `InterfaceErrorCode` of the same name exists and is a different condition:
  `session_create`'s "this principal already holds its session limit" is
  `InterfaceErrorCode::ResourceExhausted`, raised while reserving session
  ownership before any command is built
  (`crates/sdk-core/src/interface.rs`), and it answers as a top-level
  JSON-RPC error rather than inside a tool's structured result. The wire
  code reads the same; it is not this per-command code. Repair there is to
  close an idle session first -- `session_list`, then `session_close`.
- `policyDenied` -- the runtime's own execution or upload policy forbids the
  requested action for this session. Repair: not retryable as-is; use an
  allowed path or policy, or a different tool.
- `internal` -- an opaque runtime fault not attributable to caller input,
  policy, or a specific target or browser condition -- for example, HAR
  serialization or artifact-store write failures inside `network_log`, or
  the restart-recovery scan finding a non-Replayable command left mid-flight
  with no proof the browser action never ran (that case carries this code
  inside `NeedsReconciliation`, not a plain failure -- see above). Repair:
  nothing caller-side to fix; treat as non-retryable and escalate if it
  recurs.
- `targetNotFound` -- the described element no longer resolves to anything on
  the page. Repair: take a fresh `a11y_snapshot` (or `form_snapshot` for
  typed controls) and pass the new target.
- `targetAmbiguous` -- the description or selector matched more than one
  candidate. Repair: narrow the purpose or hints until exactly one candidate
  matches, or explicitly allow best-match resolution.
- `frameNotFound` -- a step in the target's frame path didn't resolve to a
  child frame, including the page having no main frame at all. Repair:
  re-resolve the target from a fresh snapshot.
- `shadowRootUnavailable` -- a step in the target's shadow path named a host
  with no attached shadow root the engine can reach. Repair: re-resolve the
  target; the element may not have attached a shadow root yet.
- `targetDetached` -- the element existed at resolution time but was no
  longer connected to the DOM by the time the engine acted on it. Repair:
  re-resolve the target -- the page changed underneath the call.
- `targetObscured` -- **Firefox-engine only.** The Firefox companion's
  pointer preflight hit-tests the resolved element's center and finds a
  different element on top of it there. There is no equivalent preflight in
  the Chromium driver -- the identical situation on a Chromium session does
  not produce this code; it surfaces as generic `browserCommandFailed`
  instead (the click call itself fails at the driver level). Do not treat
  the absence of this code as proof nothing is obscuring the target on a
  Chromium session. Repair: clear whatever is on top (for example
  `intent_dismiss_obstruction`) or scroll the element into the clear, then
  retry.
- `targetOutOfBounds` -- **Firefox-engine only**, same preflight: the
  resolved element has no clickable point inside the current viewport. As
  with `targetObscured`, Chromium has no equivalent preflight and the same
  condition there surfaces as generic `browserCommandFailed`, not this code.
  Repair: bring the element into view (scroll, resize, or `emulate` a larger
  viewport) before retrying.
- `waitConditionTimedOut` -- the awaited page condition didn't hold before
  the call's timeout. Repair: confirm the condition via `inspect`, then
  retry with a longer timeout.
- `screenshotCaptureFailed` -- either the underlying browser capture call
  (screenshot or PDF print) failed at the driver level, or the captured
  bytes failed to write to the local artifact store afterward. Repair:
  retry; if it persists, the page, engine, or artifact store may be in a
  bad state.
- `networkPolicyDenied` -- the requested URL failed the runtime's own network
  policy before any request was made (not http(s), missing host, or embedded
  credentials). Repair: use a plain http(s) URL with no userinfo.
- `httpResponseTooLarge` -- the HTTP response exceeded the configured byte
  limit for the operation. Repair: raise the byte limit within the
  configured range, or expect a smaller resource.
- `httpTransferFailed` -- the direct HTTP transfer itself failed partway
  through. Repair: retryable -- resubmit the same call.
- `httpStateConflict` -- the page's cookie or cache state changed between
  the snapshot the direct-HTTP path took and the moment it tried to commit
  the response back into the page's jar. Repair: not retryable as the same
  attempt; issue a fresh call so it re-snapshots current state.
- `httpEquivalenceUnproven` -- the runtime could not prove that a request
  built outside the browser would produce the same cookie or header
  behavior the browser itself would use, so it fails closed rather than
  risk a mismatched request. Repair: not retryable as-is; needs the page in
  a state where equivalence can be proven.
- `intentCompileFailed` -- the intent's own shape is invalid before anything
  touches the page (an empty or oversized purpose, an empty or oversized
  field list, or a blank or duplicate field name). Repair: fix the request
  shape and resubmit; nothing was attempted.
- `intentActionMismatch` -- the resolved control can't perform the requested
  action: the value's kind doesn't match the control's actual role or type,
  the control doesn't support the requested action, or it's disabled or
  read-only. Repair: re-check the control's real role or kind and match the
  action to it.
- `obstructionSuspected` -- an `intent_dismiss_obstruction` attempt acted,
  but the target it expected to disappear was still present afterward.
  Repair: take a fresh `a11y_snapshot` -- there may be another dismissal
  control, or the wrong thing was dismissed.
- `visionAssistDenied` -- the vision double gate is closed, but which
  conditions count as "the gate" depends on the path, and the two paths
  disagree about provider configuration. `extract_structured` folds
  capability, session policy, *and* provider configuration into one combined
  check and denies if any of the three is false. Every `intent_*` tool's
  vision-fallback escalation instead computes its gate from only capability
  and session policy; provider configuration is checked separately, so a
  missing provider reached through an `intent_*` tool does **not** produce
  this code -- see `visionAssistFailed`. Repair: this is a configuration or
  authorization gap, not a retry -- fall back to a deterministic tool, or
  grant the missing capability / session policy.
- `visionAssistFailed` -- the gate the reached path checks was open, but the
  vision path still didn't produce a usable result. Critically, on every
  `intent_*` tool this includes a **provider that isn't configured at all**:
  the `intent_*` gate only checks capability and session policy, so with
  both of those satisfied and no provider configured, the escalation returns
  this code, not `visionAssistDenied` -- the identical "no provider" cause
  that `extract_structured` reports as `visionAssistDenied`. This code also
  covers genuinely transient causes on any path: a screenshot capture error,
  a vision response/transport error, or a proposal that didn't clear the
  engine's confidence floor. **Repair is conditional, not "retry once":** if
  a provider simply isn't configured, retrying will fail forever -- treat it
  like `visionAssistDenied` and fall back to a deterministic tool or fix the
  configuration. Only for the transient causes (capture error, response
  error, low-confidence proposal) is a single retry reasonable.

## Protocol-layer rejections

Not every failed call produces an `ErrorCode`. A call the MCP layer refuses
before it builds a command answers with JSON-RPC `-32602` ("Invalid params")
and a reason string under `error.data.reason`
(`crates/mcp-gateway/src/server.rs`). These are protocol-layer rejections, not
per-command codes: they never appear inside a tool result, they carry no
`commandId`, and nothing ran, so the repair is always "fix the request and
resubmit" -- never reconciliation.

- `schemaViolation` -- the arguments failed the tool's declared input schema.
  Unlike the other three this one is specific: `error.data.pointer` is a JSON
  pointer to the offending value and `error.data.constraint` names the keyword
  it violated (`maxLength`, `required`, `oneOf`, ...). Repair: fix that one
  value.
- `malformedArguments` -- the arguments passed the schema but the server could
  not turn them into the tool's own argument type, or a bound checked outside
  the schema failed (`events_read`'s `limit`, which must be 1..=256). Repair:
  re-read the tool's `inputSchema` and description, and check any documented
  numeric bound.
- `deadlineOutOfRange` -- `command_execute` only: the envelope's `deadline` is
  already in the past, or more than 300,000 ms in the future. Repair: set the
  deadline inside that window and resubmit.
- `invalidIdempotencyKey` -- the supplied `idempotencyKey` is not a well-formed
  key. Repair: send a valid key, or omit the field entirely.

## Pushed events, and what a gap frame means

`events_read` is not the only way to see runtime events. The server also
pushes two JSON-RPC *notifications* (no `id`, no response expected) over the
same MCP connection (`crates/mcp-gateway/src/notify.rs`):

- `notifications/bobby/event` -- one runtime event. `params` is `{cursor,
  kind, payload}`, exactly the shape `events_read` returns in its `events`
  array. Frames are scoped to the connected principal: the only read path the
  notification stream can reach is `EventStore::read_after_for`, filtered to
  that principal's audience, and the subscription is built from the server's
  own capability handle. Delivery starts at the store's tail when the
  connection subscribes -- history before that point is not replayed, so use
  `events_read` with a cursor to read backwards.
- `notifications/tools/list_changed` -- this principal's capability set
  changed, so the tool list already downloaded is stale. Re-run `tools/list`.

A frame whose `kind` is `event.gap` is not an event: it means retention
evicted events this connection had not yet been sent, and they are gone from
the push channel. Its `cursor` is the last cursor actually delivered and its
`payload` is `{reason, earliestAvailable}`. **A gap is a catch-up signal, not
a stream failure** -- the subscription re-arms from the store's tail
immediately afterwards and keeps delivering. To recover what was missed, call
`events_read` with `cursor` set to `earliestAvailable - 1`, which is the
oldest position that still reads clean; anything before that is unrecoverable.
`events_read` reports the same shape for itself, under `error.data.eventGap`
rather than as a frame, with `reason` one of `historyLost` (the cursor fell
behind retention) or `invalidCursor` (the cursor is ahead of anything the
store holds).

Over the streamable-HTTP transport, subscribing to the push channel needs the
same `session:read` that `events_read` does (`InterfaceOperation::SubscribeEvents`,
`crates/types/src/interface.rs`); a connection whose principal lacks it still
receives `notifications/tools/list_changed`, but no event frames at all
(`crates/broker/src/mcp_http.rs`). The stdio transport (`Server::serve`,
`crates/mcp-gateway/src/server.rs`) applies no such gate on subscription.
"#;

const INTENTS_BODY: &str = r#"# Intent commands

Eight `IntentCommand` variants (`crates/types/src/commands.rs`) sit above the
primitive browser actions. Each MCP `intent_*` tool wraps exactly one
variant; `IntentCommand::class` fixes its recovery behavior.

| Intent | Tool | Class |
|---|---|---|
| `Locate` | `intent_locate` | Replayable |
| `WaitForState` | `intent_wait_for_state` | Replayable |
| `Extract` | `intent_extract` | Replayable |
| `Fill` | `intent_fill` | Reconciliable |
| `CompleteForm` | `intent_complete_form` | Reconciliable |
| `DismissObstruction` | `intent_dismiss_obstruction` | Reconciliable |
| `SubmitAndVerify` | `intent_submit_and_verify` | Boundary |
| `Follow` | `intent_follow` | Boundary if `boundary: true`, else Reconciliable |

## Classes and recovery

- **Replayable** -- never mutates the page. Safe to retry on its own; a
  crash mid-call never risks a duplicate side effect.
- **Reconciliable** -- may mutate the page, but the runtime treats it as
  inspectable and redoable rather than a point of no return. If the process
  loses track of one mid-flight, it comes back as
  `CommandOutcome::NeedsReconciliation` rather than a plain failure -- see
  `bobby://failure-taxonomy`.
- **Boundary** -- a mutating action flagged as needing a pre-established
  workflow checkpoint before it runs, because replaying it blind could
  resubmit something real. `Follow` is `Boundary` only when the caller passes
  `boundary: true` for the activated control; ordinary link navigation stays
  `Reconciliable`. `DismissObstruction` has no such flag: dismissing a popup
  is never treated as needing a checkpoint, so it is unconditionally
  `Reconciliable`.

## Targeting an intent

Pass an `a11y_snapshot` node's `target` object straight into `hints`. It
carries `{role, accessibleName, ordinal}` and every one of those is an
`IntentHints` field, so no remapping is needed and `ordinal` keeps duplicate
role/name pairs apart.

`accessibleName` matches the control's accessible name exactly and is the
same hint as `nearText: {"kind":"exact","value":...}`. Use `nearText` when
you need `contains` or `regex` matching instead. Setting `accessibleName`
and an exact `nearText` to two different values is refused
(`intentCompileFailed`) rather than resolved to one of them -- send one name.

With no name hint at all, the intent falls back to matching on `purpose`,
which is looser and more likely to go ambiguous.

## When to reach for an intent instead of a primitive

Intents resolve a target from a described purpose and verify their own
result; primitives act on a selector or a target already resolved and only
report that the action executed, not that it took hold. Reach for an intent
when:

- There is no stable target yet -- resolution by purpose and hints replaces
  a separate snapshot-then-primitive round trip.
- The action needs its own postcondition check -- a raw primitive reports
  that it ran, not that the browser accepted the result.
- Several fields need to go in as one unit with per-field verification and a
  stop-on-first-failure guarantee, rather than independent calls that leave
  no ordering contract.
- The action is mutating enough to need the checkpoint-and-reconciliation
  path -- a raw `click` on a submit control has neither.
- A structured read should degrade per-field instead of failing the whole
  call when one field can't be resolved.

Reach for the matching primitive instead when a target is already resolved
and verified, and the caller only wants the plain action without intent-style
resolution or verification overhead.
"#;

const PRIMITIVES_BODY: &str = r#"# Primitives with no named tool

Every `IntentCommand` variant, and every `PrimitiveCommand` variant but four,
has a named MCP tool that builds the command envelope for you. These four do
not (`PrimitiveCommand`, `crates/types/src/commands.rs`). They are fully
executable -- validation, capability gating, and evidence are identical to any
named tool -- but they are reachable only through `command_execute`, and
`command_execute` advertises `envelope.command` as an opaque object, so
nothing in `tools/list` describes their shape. This document is that
description.

## Calling one

`command_execute` takes `{envelope, idempotencyKey?}`. Inside the envelope,
`command` is `{"kind":"primitive","input":{"kind":<name>,"input":{...}}}`,
where `<name>` is one of the four below. The envelope's own required fields
(`schemaVersion` 2, `commandId`, `workflowId`, `attemptId`, `sessionId`,
`deadline`) are unchanged. All four are per-page commands: `pageId` is
optional in the envelope schema but required by the runtime, and omitting it
fails with `invalidRequest` ("pageId is required for page commands",
`crates/page-runtime/src/executor.rs`) before anything runs. The envelope
schema is validated in full before dispatch even though it advertises narrow
-- a malformed `input` fails with `-32602` (`schemaViolation`) and never
reaches the runtime.

`deadline` must be in the future and no more than 300,000 ms out, or the call
is rejected with `deadlineOutOfRange` -- see `bobby://failure-taxonomy`.

## `clickAndWaitForPopup`

Clicks the resolved element and waits for the popup it opens, then registers
that popup as a new page in the session so later calls can address it.

    "input": {
      "selector": string,          // required, may be ""
      "target": TargetSpec | null, // required key; null to use the selector
      "timeoutMs": integer         // required, 1..=300000
    }

Capabilities: `browser:mutate` only. **Boundary class** -- like
`intent_submit_and_verify`, it needs a verified pre-action checkpoint whose
`attemptId`, `sessionId`, `pageId` all match the envelope, whose
`recoveryClass` is `boundary`, and whose `boundaryCommandId` is this
envelope's `commandId`; otherwise it fails with `invalidRequest` before
clicking. Produces `Evidence::Popup` (`openerPageId`, the new `pageId`, `url`,
`title`); a run that produces no popup evidence fails with
`verificationFailed`. A timeout waiting for the popup is `deadlineExceeded`.

## `clickAndWaitForDownload`

Clicks the resolved element and waits for the download it starts, then admits
the downloaded file as an artifact.

    "input": {
      "selector": string,          // required, may be ""
      "target": TargetSpec | null, // required key; null to use the selector
      "timeoutMs": integer         // required, 1..=300000
    }

Capabilities: `browser:mutate` **and `file:download`**
(`command_extra_capabilities`, `crates/sdk-core/src/interface.rs`) -- the same
pair `download_url` requires. **Boundary class**, with the same pre-action
checkpoint requirement as `clickAndWaitForPopup`. Produces
`Evidence::Download`; no download evidence means `verificationFailed`, and a
timeout is `deadlineExceeded`. The download's local filesystem path is never
returned: it is replaced by an `artifact://` URI readable through
`resources/read` when the artifact was admitted, and by a redaction marker
when it was not. Admitting it needs `artifact:capture`; reading it back needs
`artifact:read`.

## `setFocusEmulation`

Turns page focus emulation on or off, so the page keeps behaving as focused
while it is not actually foregrounded.

    "input": { "enabled": boolean }   // required

Capabilities: `browser:mutate` only. Reconciliable. Produces
`Evidence::Configuration` with `name` `"focusEmulation"` and `value` the
boolean rendered as a string; anything else fails `verificationFailed`.

## `setEmulatedMedia`

Overrides the page's CSS media type and media features -- printing a page's
print stylesheet, or forcing `prefers-color-scheme`, without changing the
real environment.

    "input": {
      "media": string,             // required, 0..=256 bytes, e.g. "print", "screen"
      "features": {                // required, object of string -> string
        "<name>": "<value>"        // <=64 entries, name <=128 bytes, value <=4096 bytes
      }                            // e.g. {"prefers-color-scheme":"dark"}
    }

Capabilities: `browser:mutate` only. Reconciliable. Produces
`Evidence::Configuration` with `name` `"emulatedMedia"` and `value` the
serialized command; anything else fails `verificationFailed`.

## Engine support

`setFocusEmulation` and `setEmulatedMedia` are implemented by the Chromium
worker only (`crates/worker-pool/src/chromium.rs`). Every other worker keeps
the `BrowserWorker` default, which refuses with `browserCommandFailed`
("browser primitive is not supported by this worker",
`crates/worker-pool/src/lib.rs`), so on a Firefox session neither is a
retryable failure -- use a named tool instead. `clickAndWaitForPopup` and
`clickAndWaitForDownload` are implemented on both Chromium
(`crates/worker-pool/src/chromium.rs`) and Firefox
(`crates/firefox-companion/src/worker.rs`).
"#;

pub(crate) fn static_resources() -> &'static [(&'static str, &'static str, &'static str)] {
    &[
        (
            CAPABILITIES_URI,
            "Capabilities",
            "Every capability string, what it unlocks, and which tools require it.",
        ),
        (
            FAILURE_TAXONOMY_URI,
            "Failure taxonomy",
            "Every runtime error code, the protocol-layer rejections, the NeedsReconciliation outcome, and the pushed-event channel.",
        ),
        (
            INTENTS_URI,
            "Intents",
            "The eight intent commands, their execution class, and when to reach for one over a primitive.",
        ),
        (
            PRIMITIVES_URI,
            "Primitives without a tool",
            "The four executable primitives that have no named tool, their argument shape, and how to reach them via command_execute.",
        ),
    ]
}

pub(crate) fn static_resource_body(uri: &str) -> Option<&'static str> {
    match uri {
        CAPABILITIES_URI => Some(CAPABILITIES_BODY),
        FAILURE_TAXONOMY_URI => Some(FAILURE_TAXONOMY_BODY),
        INTENTS_URI => Some(INTENTS_BODY),
        PRIMITIVES_URI => Some(PRIMITIVES_BODY),
        _ => None,
    }
}
