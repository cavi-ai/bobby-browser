use artifact_store::{ArtifactStore, PendingArtifact};
use network_engine::{
    DirectHttpExecutor, EligibilityDecision, EligibilityPolicy, HttpCandidate, NetworkPolicy,
};
use types::{
    CommandEnvelope, CommandError, ErrorCode, ErrorLayer, Evidence, ExecutionPath, ExecutionReason,
    PageState, PrimitiveCommand,
};
use worker_pool::WorkerLease;

#[derive(Debug)]
pub struct AdaptiveExecution {
    pub evidence: Vec<Evidence>,
    pub used_browser: bool,
    pub prepared_download: Option<PreparedDownload>,
}

#[derive(Debug)]
pub struct PreparedDownload {
    pub state_version: u64,
    pub state: network_engine::ResponseStateDelta,
    pub artifact: PendingArtifact,
}

#[derive(Clone, Default)]
pub struct AdaptivePageEngine {
    direct: Option<DirectComponents>,
}

#[derive(Clone)]
struct DirectComponents {
    eligibility: EligibilityPolicy,
    executor: DirectHttpExecutor,
    artifacts: ArtifactStore,
    network: NetworkPolicy,
}

impl AdaptivePageEngine {
    pub fn browser_only() -> Self {
        Self::default()
    }

    pub fn new(
        eligibility: EligibilityPolicy,
        executor: DirectHttpExecutor,
        artifacts: ArtifactStore,
        network: NetworkPolicy,
    ) -> Self {
        Self {
            direct: Some(DirectComponents {
                eligibility,
                executor,
                artifacts,
                network,
            }),
        }
    }

    pub async fn execute(
        &self,
        envelope: &CommandEnvelope,
        lease: &WorkerLease,
        page: PageState,
    ) -> Result<AdaptiveExecution, CommandError> {
        let Some(direct) = &self.direct else {
            return browser_execute(
                envelope,
                lease,
                ExecutionPath::Chromium,
                ExecutionReason::IneligibleCommand,
                0,
            )
            .await;
        };
        let page_url = page.url.as_deref().unwrap_or_default();
        match direct.eligibility.classify(&envelope.command, page_url) {
            EligibilityDecision::Denied(error) => Err(error),
            EligibilityDecision::Chromium(reason) => {
                browser_execute(envelope, lease, ExecutionPath::Chromium, reason, 0).await
            }
            EligibilityDecision::DirectHttp(reason) => {
                let page_id = envelope.page_id.as_ref().expect("validated page id");
                let snapshot = lease.worker().http_state(page_id).await?;
                let version = snapshot.version;
                let candidate = match &envelope.command {
                    PrimitiveCommand::Inspect(command) => {
                        direct.executor.inspect(&snapshot, command).await?
                    }
                    PrimitiveCommand::DownloadUrl(command) => {
                        direct.executor.download(&snapshot, command).await?
                    }
                    _ => unreachable!("eligibility only selects supported HTTP commands"),
                };
                match candidate {
                    HttpCandidate::FallbackRequired(fallback_reason) => {
                        if matches!(envelope.command, PrimitiveCommand::Inspect(_)) {
                            browser_execute(
                                envelope,
                                lease,
                                ExecutionPath::ChromiumFallback,
                                fallback_reason,
                                version,
                            )
                            .await
                        } else {
                            Err(equivalence_unproven(fallback_reason))
                        }
                    }
                    HttpCandidate::Inspection {
                        evidence,
                        state,
                        meta,
                    } => {
                        if let Err(error) = lease
                            .worker()
                            .commit_http_state(page_id, version, state)
                            .await
                        {
                            if error.code == ErrorCode::HttpStateConflict {
                                return browser_execute(
                                    envelope,
                                    lease,
                                    ExecutionPath::ChromiumFallback,
                                    ExecutionReason::StateConflict,
                                    version,
                                )
                                .await;
                            }
                            return Err(error);
                        }
                        Ok(AdaptiveExecution {
                            evidence: vec![
                                evidence,
                                execution_evidence(
                                    ExecutionPath::DirectHttp,
                                    reason,
                                    version,
                                    meta.elapsed_ms,
                                    Some(meta.bytes),
                                    Some(meta.sha256),
                                    Some(meta.final_url),
                                    Some(meta.content_type),
                                    Some(meta.status),
                                    meta.redirect_chain,
                                ),
                            ],
                            used_browser: false,
                            prepared_download: None,
                        })
                    }
                    HttpCandidate::Download {
                        bytes,
                        filename,
                        media_type,
                        state,
                        meta,
                    } => {
                        let extension = safe_extension(&filename);
                        let pending = direct
                            .artifacts
                            .put_pending(
                                &envelope.session_id,
                                page_id,
                                &media_type,
                                extension,
                                &bytes,
                                direct.network.max_download_bytes,
                            )
                            .await
                            .map_err(artifact_error)?;
                        let record = pending.record().clone();
                        Ok(AdaptiveExecution {
                            evidence: vec![
                                Evidence::Download {
                                    filename,
                                    path: record.artifact_id,
                                    bytes: record.bytes,
                                    sha256: record.sha256.clone(),
                                },
                                execution_evidence(
                                    ExecutionPath::DirectHttp,
                                    reason,
                                    version,
                                    meta.elapsed_ms,
                                    Some(meta.bytes),
                                    Some(meta.sha256),
                                    Some(meta.final_url),
                                    Some(meta.content_type),
                                    Some(meta.status),
                                    meta.redirect_chain,
                                ),
                            ],
                            used_browser: false,
                            prepared_download: Some(PreparedDownload {
                                state_version: version,
                                state,
                                artifact: pending,
                            }),
                        })
                    }
                }
            }
        }
    }
}

fn safe_extension(filename: &str) -> &str {
    filename
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 16
                && extension.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .unwrap_or("bin")
}

fn artifact_error(error: artifact_store::ArtifactError) -> CommandError {
    CommandError {
        code: ErrorCode::Internal,
        message: format!("download artifact persistence failed: {error}"),
        layer: ErrorLayer::Page,
        retryable: false,
    }
}

fn equivalence_unproven(reason: ExecutionReason) -> CommandError {
    CommandError {
        code: ErrorCode::HttpEquivalenceUnproven,
        message: format!("direct download equivalence was not proven: {reason:?}"),
        layer: ErrorLayer::Network,
        retryable: false,
    }
}

fn execution_evidence(
    path: ExecutionPath,
    reason: ExecutionReason,
    state_version: u64,
    elapsed_ms: u64,
    bytes: Option<u64>,
    sha256: Option<String>,
    final_url: Option<String>,
    content_type: Option<String>,
    status: Option<u16>,
    redirect_chain: Vec<String>,
) -> Evidence {
    Evidence::ExecutionPath {
        path,
        reason,
        state_version,
        elapsed_ms,
        bytes,
        sha256,
        final_url,
        content_type,
        status,
        redirect_chain,
    }
}

async fn browser_execute(
    envelope: &CommandEnvelope,
    lease: &WorkerLease,
    path: ExecutionPath,
    reason: ExecutionReason,
    state_version: u64,
) -> Result<AdaptiveExecution, CommandError> {
    let page_id = envelope.page_id.as_ref().expect("validated page id");
    let mut evidence = match &envelope.command {
        PrimitiveCommand::Navigate(command) => lease.worker().navigate(page_id, command).await?,
        PrimitiveCommand::Inspect(command) => lease.worker().inspect(page_id, command).await?,
        PrimitiveCommand::Click(command) => lease.worker().click(page_id, command).await?,
        PrimitiveCommand::TypeText(command) => lease.worker().type_text(page_id, command).await?,
        PrimitiveCommand::UploadFiles(command) => {
            lease.worker().upload_files(page_id, command).await?
        }
        PrimitiveCommand::OpenPage(command) => lease.worker().open_page_command(command).await?,
        PrimitiveCommand::ListPages(command) => lease.worker().list_pages(command).await?,
        PrimitiveCommand::ClosePage(command) => lease.worker().close_page_command(command).await?,
        PrimitiveCommand::ClickAndWaitForPopup(command) => {
            lease
                .worker()
                .click_and_wait_for_popup(page_id, command)
                .await?
        }
        PrimitiveCommand::ClickAndWaitForDownload(command) => {
            lease
                .worker()
                .click_and_wait_for_download(page_id, command)
                .await?
        }
        PrimitiveCommand::WaitFor(command) => lease.worker().wait_for(page_id, command).await?,
        PrimitiveCommand::CaptureScreenshot(command) => {
            lease.worker().capture_screenshot(page_id, command).await?
        }
        PrimitiveCommand::DownloadUrl(_) => return Err(equivalence_unproven(reason)),
    };
    let (bytes, sha256) = evidence
        .iter()
        .find_map(|item| match item {
            Evidence::Download { bytes, sha256, .. } => Some((Some(*bytes), Some(sha256.clone()))),
            _ => None,
        })
        .unwrap_or((None, None));
    evidence.push(execution_evidence(
        path,
        reason,
        state_version,
        0,
        bytes,
        sha256,
        None,
        None,
        None,
        Vec::new(),
    ));
    Ok(AdaptiveExecution {
        evidence,
        used_browser: true,
        prepared_download: None,
    })
}
