use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use artifact_store::{ArtifactStore, PendingArtifact};
use async_trait::async_trait;
use intent_engine::{IntentBrowser, IntentEngine, IntentOutcome, VisionAssist, VisionContext};
use network_engine::{
    DirectHttpExecutor, EligibilityDecision, EligibilityPolicy, HttpCandidate, NetworkPolicy,
};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandEnvelope, CommandError, ControlActionCommand,
    ErrorCode, ErrorLayer, Evidence, ExecutionPath, ExecutionReason, PageId, PageState,
    PrimitiveCommand, RuntimeCommand, TargetSpec, TypeTextCommand, UploadFilesCommand,
    WaitForCommand,
};
use worker_pool::WorkerLease;
use workflow_journal::PreparedDownload;

/// Session + capability flags for vision escalation. Provider lives on
/// [`AdaptivePageEngine`]; IntentEngine enforces the deny-by-default double gate.
#[derive(Debug, Clone, Copy, Default)]
pub struct VisionGate {
    pub session_ok: bool,
    pub capability_ok: bool,
}

/// Everything `ExecutionPolicy` decides that the executor has to apply, resolved
/// once per command by the layer that can see the session
/// (`sdk_core::RuntimeService`).
///
/// `Default` must stay all-off with no provider: a caller that cannot prove the session
/// opted in gets no fingerprinting, no humanization, and no node.
#[derive(Clone, Default)]
pub struct SessionGate {
    pub vision: VisionGate,
    pub fingerprint: bool,
    pub humanize: bool,
    /// The outcome of resolving this session's named vision node.
    pub vision_node: NodeSelection,
}

/// What resolving a session's `visionNode` produced.
///
/// The three states must stay distinct. Collapsing `Unresolved` into `NotRequested`
/// (an `Option<Arc<dyn VisionAssist>>`) lets a mistyped node name silently escalate to
/// whatever provider the process was built with.
#[derive(Clone, Default)]
pub enum NodeSelection {
    /// The session named no node. An embedder-installed provider, if any,
    /// applies: nothing was chosen, so nothing was overridden.
    #[default]
    NotRequested,
    /// The session named a node and it resolved.
    Resolved(Arc<dyn VisionAssist>),
    /// The session named a node that did not resolve. No provider runs, and
    /// no other provider stands in for it.
    Unresolved,
}

impl NodeSelection {
    /// The private `provider`, exposed so the three states can be asserted apart
    /// without a live browser escalation.
    pub fn provider_for_test(
        &self,
        installed: Option<Arc<dyn VisionAssist>>,
    ) -> Option<Arc<dyn VisionAssist>> {
        self.provider(installed)
    }

    /// The provider to escalate to, given the process-wide default.
    fn provider(&self, installed: Option<Arc<dyn VisionAssist>>) -> Option<Arc<dyn VisionAssist>> {
        match self {
            Self::NotRequested => installed,
            Self::Resolved(provider) => Some(Arc::clone(provider)),
            Self::Unresolved => None,
        }
    }
}

impl std::fmt::Debug for NodeSelection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotRequested => "NotRequested",
            Self::Resolved(_) => "Resolved",
            Self::Unresolved => "Unresolved",
        })
    }
}

impl std::fmt::Debug for SessionGate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionGate")
            .field("vision", &self.vision)
            .field("fingerprint", &self.fingerprint)
            .field("humanize", &self.humanize)
            .field("vision_node", &self.vision_node)
            .finish()
    }
}

impl From<VisionGate> for SessionGate {
    fn from(vision: VisionGate) -> Self {
        Self {
            vision,
            ..Self::default()
        }
    }
}

struct WorkerIntentBrowser<'a> {
    lease: &'a WorkerLease,
}

#[async_trait]
impl IntentBrowser for WorkerIntentBrowser<'_> {
    async fn collect_candidates(
        &self,
        page_id: &PageId,
        target: &TargetSpec,
    ) -> Result<Vec<dom_engine::Candidate>, CommandError> {
        self.lease
            .worker()
            .collect_candidates(page_id, target)
            .await
    }

    async fn click(
        &self,
        page_id: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.lease.worker().click(page_id, command).await
    }

    async fn click_xy(
        &self,
        page_id: &PageId,
        x: f64,
        y: f64,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.lease.worker().click_xy(page_id, x, y).await
    }

    async fn type_text(
        &self,
        page_id: &PageId,
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.lease.worker().type_text(page_id, command).await
    }

    async fn element_at_point(
        &self,
        page_id: &PageId,
        x: f64,
        y: f64,
    ) -> Result<Option<(String, String)>, CommandError> {
        self.lease.worker().element_at_point(page_id, x, y).await
    }

    async fn upload_files(
        &self,
        page_id: &PageId,
        command: &UploadFilesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.lease.worker().upload_files(page_id, command).await
    }

    async fn control_action(
        &self,
        page_id: &PageId,
        command: &ControlActionCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.lease.worker().control_action(page_id, command).await
    }

    async fn wait_for(
        &self,
        page_id: &PageId,
        command: &WaitForCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.lease.worker().wait_for(page_id, command).await
    }

    async fn validation_errors_visible(&self, page_id: &PageId) -> Result<bool, CommandError> {
        let probe = TargetSpec {
            css: Some("[aria-invalid='true']".into()),
            ..TargetSpec::default()
        };
        let candidates = self
            .lease
            .worker()
            .collect_candidates(page_id, &probe)
            .await?;
        Ok(!candidates.is_empty())
    }

    async fn capture_screenshot(
        &self,
        page_id: &PageId,
        command: &CaptureScreenshotCommand,
    ) -> Result<(Vec<u8>, Vec<Evidence>), CommandError> {
        // Real PNG bytes when the worker supports them. Workers without byte plumbing
        // stay artifact-only, and vision providers get an empty frame, which their own
        // confidence floor rejects.
        let bytes = self
            .lease
            .worker()
            .screenshot_bytes(page_id)
            .await
            .unwrap_or_default();
        let evidence = self
            .lease
            .worker()
            .capture_screenshot(page_id, command)
            .await?;
        Ok((bytes, evidence))
    }
}

#[derive(Debug)]
pub struct AdaptiveExecution {
    pub evidence: Vec<Evidence>,
    pub used_browser: bool,
    pub prepared_http: Option<PreparedHttpResult>,
}

#[derive(Debug)]
pub struct AdaptiveFailure {
    pub error: CommandError,
    pub evidence: Vec<Evidence>,
}

impl From<CommandError> for AdaptiveFailure {
    fn from(error: CommandError) -> Self {
        Self {
            error,
            evidence: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct PreparedHttpResult {
    pub state_version: u64,
    pub state: network_engine::ResponseStateDelta,
    pub artifact: Option<PendingArtifact>,
    pub download: Option<PendingDownload>,
}

#[derive(Debug)]
pub struct PendingDownload {
    root: SecureDownloadRoot,
    record: PreparedDownload,
    filename: String,
    sha256: String,
    bytes: u64,
}

#[derive(Debug)]
pub struct CommittedDownload {
    root: SecureDownloadRoot,
    record: PreparedDownload,
}

#[derive(Debug)]
struct DownloadDestination {
    root: SecureDownloadRoot,
    filename: String,
    staging_id: String,
}

#[derive(Debug)]
struct SecureDownloadRoot {
    #[cfg(unix)]
    directory: std::fs::File,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StagedDownloadMetadata {
    filename: String,
}

struct StagingCleanup<'a> {
    root: &'a SecureDownloadRoot,
    staging_name: String,
    metadata_name: String,
    armed: bool,
}

impl Drop for StagingCleanup<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = self.root.remove_if_exists(&self.staging_name);
        let _ = self.root.remove_if_exists(&self.metadata_name);
        let _ = self.root.sync();
    }
}

impl DownloadDestination {
    async fn validate(
        downloads_root: &Path,
        save_as: &str,
        command_id: &types::CommandId,
    ) -> Result<Self, CommandError> {
        tokio::fs::create_dir_all(downloads_root)
            .await
            .map_err(download_storage_error)?;
        let canonical_root = tokio::fs::canonicalize(downloads_root)
            .await
            .map_err(download_storage_error)?;
        let requested = PathBuf::from(save_as);
        let destination = if requested.is_absolute() {
            requested
        } else {
            canonical_root.join(requested)
        };
        let parent = destination
            .parent()
            .ok_or_else(|| download_policy_error("download destination has no parent"))?;
        let canonical_parent = tokio::fs::canonicalize(parent)
            .await
            .map_err(|_| download_policy_error("download destination parent is unavailable"))?;
        if canonical_parent != canonical_root {
            return Err(download_policy_error(
                "download destination must be a file directly below the configured downloads root",
            ));
        }
        let filename = single_filename(
            destination
                .file_name()
                .and_then(|filename| filename.to_str())
                .ok_or_else(|| {
                    download_policy_error("download destination has no valid filename")
                })?,
        )?;
        let root = SecureDownloadRoot::open(&canonical_root)?;
        if root.exists(&filename)? {
            return Err(download_policy_error("download destination already exists"));
        }
        let staging_id = command_id.0.to_string();
        Ok(Self {
            root,
            filename,
            staging_id,
        })
    }

    async fn stage(self, bytes: &[u8]) -> Result<PendingDownload, CommandError> {
        let staging_name = staging_name(&self.staging_id)?;
        let metadata_name = metadata_name(&self.staging_id)?;
        let mut file = self.root.create_file(&staging_name)?;
        let mut cleanup = StagingCleanup {
            root: &self.root,
            staging_name: staging_name.clone(),
            metadata_name: metadata_name.clone(),
            armed: true,
        };
        if let Err(error) = async {
            file.write_all(bytes).await?;
            file.flush().await?;
            file.sync_all().await
        }
        .await
        {
            return Err(download_storage_error(error));
        }
        let metadata = serde_json::to_vec(&StagedDownloadMetadata {
            filename: self.filename.clone(),
        })
        .map_err(|error| download_storage_error(std::io::Error::other(error)))?;
        let record = PreparedDownload {
            staging_id: self.staging_id,
            metadata_sha256: hex::encode(Sha256::digest(&metadata)),
        };
        let mut metadata_file = match self.root.create_file(&metadata_name) {
            Ok(file) => file,
            Err(error) => return Err(error),
        };
        if let Err(error) = async {
            metadata_file.write_all(&metadata).await?;
            metadata_file.flush().await?;
            metadata_file.sync_all().await
        }
        .await
        {
            return Err(download_storage_error(error));
        }
        self.root.sync()?;
        cleanup.armed = false;
        drop(cleanup);
        Ok(PendingDownload {
            root: self.root,
            record,
            filename: self.filename,
            sha256: hex::encode(Sha256::digest(bytes)),
            bytes: bytes.len() as u64,
        })
    }
}

impl PendingDownload {
    pub fn record(&self) -> &PreparedDownload {
        &self.record
    }

    pub fn commit(self) -> Result<CommittedDownload, CommandError> {
        self.root
            .publish(&self.record, &self.filename, &self.sha256, self.bytes)?;
        Ok(CommittedDownload {
            root: self.root,
            record: self.record,
        })
    }

    pub fn discard(self) {
        let _ = self.root.cleanup(&self.record);
    }
}

impl CommittedDownload {
    pub fn cleanup(self) -> Result<(), CommandError> {
        self.root.cleanup(&self.record)
    }
}

impl SecureDownloadRoot {
    #[cfg(unix)]
    fn open(root: &Path) -> Result<Self, CommandError> {
        use rustix::fs::{Mode, OFlags};
        let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY;
        let root_fd = rustix::fs::open("/", flags, Mode::empty())
            .map_err(|error| download_storage_error(std::io::Error::from(error)))?;
        let mut directory = std::fs::File::from(root_fd);
        for component in root.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => {
                    let fd = rustix::fs::openat(&directory, name, flags, Mode::empty())
                        .map_err(|error| download_storage_error(std::io::Error::from(error)))?;
                    directory = std::fs::File::from(fd);
                }
                _ => {
                    return Err(download_policy_error(
                        "downloads root contains an unsupported path component",
                    ))
                }
            }
        }
        Ok(Self { directory })
    }

    #[cfg(not(unix))]
    fn open(_root: &Path) -> Result<Self, CommandError> {
        Err(download_policy_error(
            "saveAs requires secure directory-relative file operations on this platform",
        ))
    }

    #[cfg(unix)]
    fn exists(&self, filename: &str) -> Result<bool, CommandError> {
        use rustix::fs::AtFlags;
        match rustix::fs::statat(&self.directory, filename, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(error) if error == rustix::io::Errno::NOENT => Ok(false),
            Err(error) => Err(download_storage_error(std::io::Error::from(error))),
        }
    }

    #[cfg(not(unix))]
    fn exists(&self, _filename: &str) -> Result<bool, CommandError> {
        unreachable!("non-Unix saveAs is rejected while opening the root")
    }

    #[cfg(unix)]
    fn create_file(&self, name: &str) -> Result<tokio::fs::File, CommandError> {
        use rustix::fs::{Mode, OFlags};
        let fd = rustix::fs::openat(
            &self.directory,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|error| download_storage_error(std::io::Error::from(error)))?;
        Ok(tokio::fs::File::from_std(std::fs::File::from(fd)))
    }

    #[cfg(not(unix))]
    fn create_file(&self, _name: &str) -> Result<tokio::fs::File, CommandError> {
        unreachable!("non-Unix saveAs is rejected while opening the root")
    }

    #[cfg(unix)]
    fn file_matches(
        &self,
        name: &str,
        expected_sha256: &str,
        expected_bytes: u64,
    ) -> Result<Option<(u64, u64)>, CommandError> {
        use rustix::fs::{Mode, OFlags};
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;
        let fd = rustix::fs::openat(
            &self.directory,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|error| download_storage_error(std::io::Error::from(error)))?;
        let mut file = std::fs::File::from(fd);
        let metadata = file.metadata().map_err(download_storage_error)?;
        if !metadata.file_type().is_file() || metadata.len() != expected_bytes {
            return Ok(None);
        }
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let read = file.read(&mut buffer).map_err(download_storage_error)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        if hex::encode(digest.finalize()) != expected_sha256 {
            return Ok(None);
        }
        Ok(Some((metadata.dev(), metadata.ino())))
    }

    #[cfg(not(unix))]
    fn file_matches(
        &self,
        _name: &str,
        _expected_sha256: &str,
        _expected_bytes: u64,
    ) -> Result<Option<(u64, u64)>, CommandError> {
        unreachable!("non-Unix saveAs is rejected while opening the root")
    }

    #[cfg(unix)]
    fn identity(&self, name: &str) -> Result<(u64, u64), CommandError> {
        use rustix::fs::{Mode, OFlags};
        use std::os::unix::fs::MetadataExt;
        let fd = rustix::fs::openat(
            &self.directory,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|error| download_storage_error(std::io::Error::from(error)))?;
        let metadata = std::fs::File::from(fd)
            .metadata()
            .map_err(download_storage_error)?;
        if !metadata.file_type().is_file() {
            return Err(download_policy_error(
                "download staging entry is not a regular file",
            ));
        }
        Ok((metadata.dev(), metadata.ino()))
    }

    #[cfg(unix)]
    fn read_metadata(&self, record: &PreparedDownload) -> Result<String, CommandError> {
        use rustix::fs::{Mode, OFlags};
        use std::io::Read;
        let name = metadata_name(&record.staging_id)?;
        let fd = rustix::fs::openat(
            &self.directory,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|error| download_storage_error(std::io::Error::from(error)))?;
        let mut file = std::fs::File::from(fd);
        let metadata = file.metadata().map_err(download_storage_error)?;
        if !metadata.file_type().is_file() || metadata.len() > 4096 {
            return Err(download_policy_error("invalid prepared download metadata"));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.read_to_end(&mut bytes)
            .map_err(download_storage_error)?;
        if hex::encode(Sha256::digest(&bytes)) != record.metadata_sha256 {
            return Err(download_policy_error(
                "prepared download metadata digest mismatch",
            ));
        }
        let metadata: StagedDownloadMetadata = serde_json::from_slice(&bytes)
            .map_err(|_| download_policy_error("invalid prepared download metadata"))?;
        single_filename(&metadata.filename)
    }

    #[cfg(not(unix))]
    fn read_metadata(&self, _record: &PreparedDownload) -> Result<String, CommandError> {
        unreachable!("non-Unix saveAs is rejected while opening the root")
    }

    #[cfg(unix)]
    fn publish(
        &self,
        record: &PreparedDownload,
        filename: &str,
        sha256: &str,
        bytes: u64,
    ) -> Result<(), CommandError> {
        let staging = staging_name(&record.staging_id)?;
        let expected_identity = self
            .file_matches(&staging, sha256, bytes)?
            .ok_or_else(|| download_policy_error("prepared download staging digest mismatch"))?;
        rustix::fs::linkat(
            &self.directory,
            staging.as_str(),
            &self.directory,
            filename,
            rustix::fs::AtFlags::empty(),
        )
        .map_err(|error| {
            if error == rustix::io::Errno::EXIST {
                download_policy_error("download destination already exists")
            } else {
                download_storage_error(std::io::Error::from(error))
            }
        })?;
        if self.identity(filename)? != expected_identity {
            let _ = self.remove(filename);
            let _ = self.sync();
            return Err(download_policy_error(
                "download staging entry changed during publication",
            ));
        }
        // Persist the destination before removing recovery state. A durable
        // Completed record must never outlive the directory entry it reports.
        self.sync()?;
        self.remove(&staging)?;
        self.sync()
    }

    #[cfg(not(unix))]
    fn publish(
        &self,
        _record: &PreparedDownload,
        _filename: &str,
        _sha256: &str,
        _bytes: u64,
    ) -> Result<(), CommandError> {
        unreachable!("non-Unix saveAs is rejected while opening the root")
    }

    #[cfg(unix)]
    fn remove(&self, name: &str) -> Result<(), CommandError> {
        rustix::fs::unlinkat(&self.directory, name, rustix::fs::AtFlags::empty())
            .map_err(|error| download_storage_error(std::io::Error::from(error)))
    }

    #[cfg(not(unix))]
    fn remove(&self, _name: &str) -> Result<(), CommandError> {
        unreachable!("non-Unix saveAs is rejected while opening the root")
    }

    fn remove_if_exists(&self, name: &str) -> Result<(), CommandError> {
        if self.exists(name)? {
            self.remove(name)?;
        }
        Ok(())
    }

    fn cleanup(&self, record: &PreparedDownload) -> Result<(), CommandError> {
        self.remove_if_exists(&staging_name(&record.staging_id)?)?;
        self.remove_if_exists(&metadata_name(&record.staging_id)?)?;
        self.sync()
    }

    #[cfg(unix)]
    fn sync(&self) -> Result<(), CommandError> {
        rustix::fs::fsync(&self.directory)
            .map_err(|error| download_storage_error(std::io::Error::from(error)))
    }

    #[cfg(not(unix))]
    fn sync(&self) -> Result<(), CommandError> {
        unreachable!("non-Unix saveAs is rejected while opening the root")
    }
}

fn staging_name(staging_id: &str) -> Result<String, CommandError> {
    opaque_staging_name(staging_id, ".tmp")
}

fn metadata_name(staging_id: &str) -> Result<String, CommandError> {
    opaque_staging_name(staging_id, ".meta")
}

fn opaque_staging_name(staging_id: &str, suffix: &str) -> Result<String, CommandError> {
    if staging_id.is_empty()
        || !staging_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(download_policy_error(
            "invalid prepared download staging id",
        ));
    }
    Ok(format!(".bobby-download-{staging_id}{suffix}"))
}

fn single_filename(value: &str) -> Result<String, CommandError> {
    let mut components = Path::new(value).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(value.to_owned()),
        _ => Err(download_policy_error(
            "download destination must be a single filename",
        )),
    }
}

#[derive(Clone, Default)]
pub struct AdaptivePageEngine {
    direct: Option<DirectComponents>,
    /// Pages that have seen a non-read-only command since their last
    /// navigation. A whole-page read of a tainted page must come from the
    /// live DOM: a direct-HTTP refetch of the URL answers the app shell,
    /// not the post-interaction state.
    taints: Arc<tokio::sync::Mutex<std::collections::HashSet<types::PageId>>>,
    vision_assist: Option<Arc<dyn VisionAssist>>,
    structured_extractor: Option<Arc<dyn intent_engine::StructuredExtractor>>,
    /// Prefill proposal cache handle. `None` unless `[vision].prefill` is
    /// on; the executor attaches the session's context graph.
    proposals: Option<Arc<dyn intent_engine::ProposalLookup>>,
    /// The runtime's context graph, for the vision prompt's recent-commands
    /// block. Attached always; independent of the prefill flag.
    context_graph: Option<Arc<crate::ContextGraph>>,
    /// Escalation corpus sink (`[vision].corpus_dir`). `None` writes nothing.
    corpus: Option<intent_engine::VisionCorpus>,
    operational_metrics: Option<observability::OperationalMetrics>,
}

#[derive(Clone)]
struct DirectComponents {
    eligibility: EligibilityPolicy,
    executor: DirectHttpExecutor,
    artifacts: ArtifactStore,
    downloads_root: Option<PathBuf>,
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
                downloads_root: None,
                network,
            }),
            taints: Default::default(),
            vision_assist: None,
            structured_extractor: None,
            proposals: None,
            context_graph: None,
            corpus: None,
            operational_metrics: None,
        }
    }

    pub fn with_operational_metrics(
        mut self,
        metrics: observability::OperationalMetrics,
    ) -> Self {
        self.operational_metrics = Some(metrics);
        self
    }

    pub(crate) fn record_retry(&self, class: observability::RetryClass) {
        if let Some(metrics) = &self.operational_metrics {
            metrics.record_retry(class);
        }
    }

    pub fn with_downloads_root(mut self, downloads_root: impl Into<PathBuf>) -> Self {
        if let Some(direct) = self.direct.as_mut() {
            direct.downloads_root = Some(downloads_root.into());
        }
        self
    }

    /// Enables lazy batch prefill against this proposal cache (the
    /// runtime's context graph). Off by default.
    pub fn with_vision_prefill(
        mut self,
        proposals: Arc<dyn intent_engine::ProposalLookup>,
    ) -> Self {
        self.proposals = Some(proposals);
        self
    }

    /// Attaches the runtime's context graph so escalation prompts carry the
    /// recent-commands block.
    pub fn with_context_graph(mut self, graph: Arc<crate::ContextGraph>) -> Self {
        self.context_graph = Some(graph);
        self
    }

    pub fn with_vision_assist(mut self, assist: Arc<dyn VisionAssist>) -> Self {
        self.vision_assist = Some(assist);
        self
    }

    /// Enables escalation corpus collection into the configured directory.
    pub fn with_vision_corpus(mut self, corpus: intent_engine::VisionCorpus) -> Self {
        self.corpus = Some(corpus);
        self
    }

    pub fn with_structured_extractor(
        mut self,
        extractor: Arc<dyn intent_engine::StructuredExtractor>,
    ) -> Self {
        self.structured_extractor = Some(extractor);
        self
    }

    pub fn finalize_prepared_artifact(
        &self,
        session_id: &types::SessionId,
        artifact_id: &str,
        staging_id: &str,
        sha256: &str,
        bytes: u64,
    ) -> Result<(), CommandError> {
        let direct = self
            .direct
            .as_ref()
            .ok_or_else(|| internal_artifact_error("adaptive artifact store is not configured"))?;
        direct
            .artifacts
            .finalize_staged(session_id, artifact_id, staging_id, sha256, bytes)
            .map_err(artifact_error)
    }

    pub fn finalize_prepared_download(
        &self,
        prepared: &PreparedDownload,
        sha256: &str,
        bytes: u64,
    ) -> Result<(), CommandError> {
        staging_name(&prepared.staging_id)?;
        metadata_name(&prepared.staging_id)?;
        let root = self
            .direct
            .as_ref()
            .and_then(|direct| direct.downloads_root.as_deref())
            .ok_or_else(|| {
                download_policy_error("download destination storage is not configured")
            })?;
        let canonical_root = std::fs::canonicalize(root).map_err(download_storage_error)?;
        let root = SecureDownloadRoot::open(&canonical_root)?;
        let filename = root.read_metadata(prepared)?;
        if root.exists(&filename)? {
            if root.file_matches(&filename, sha256, bytes)?.is_some() {
                root.cleanup(prepared)?;
                return Ok(());
            }
            return Err(download_policy_error(
                "prepared download destination exists with different contents",
            ));
        }
        root.publish(prepared, &filename, sha256, bytes)?;
        root.cleanup(prepared)
    }

    pub fn cleanup_prepared_download(
        &self,
        prepared: &PreparedDownload,
    ) -> Result<(), CommandError> {
        let root = self
            .direct
            .as_ref()
            .and_then(|direct| direct.downloads_root.as_deref())
            .ok_or_else(|| {
                download_policy_error("download destination storage is not configured")
            })?;
        let canonical_root = std::fs::canonicalize(root).map_err(download_storage_error)?;
        SecureDownloadRoot::open(&canonical_root)?.cleanup(prepared)
    }

    pub async fn execute(
        &self,
        envelope: &CommandEnvelope,
        lease: &WorkerLease,
        page: Option<PageState>,
        gate: &SessionGate,
    ) -> Result<AdaptiveExecution, AdaptiveFailure> {
        let result = self.execute_inner(envelope, lease, page, gate).await;
        self.record_page_taint(envelope, result.is_ok()).await;
        result
    }

    /// Taint bookkeeping: navigation replaces the DOM with a fresh document
    /// (refetch == live again), so it clears the taint; any other
    /// non-Replayable command that touches the page may have mutated it, so
    /// it sets it.
    async fn record_page_taint(&self, envelope: &CommandEnvelope, succeeded: bool) {
        let Some(page_id) = envelope.page_id.as_ref() else {
            return;
        };
        let mut taints = self.taints.lock().await;
        if matches!(
            envelope.command,
            RuntimeCommand::Primitive(PrimitiveCommand::Navigate(_))
        ) {
            if succeeded {
                taints.remove(page_id);
            } else {
                // A failed navigation may still have replaced or partially
                // mutated the document. Preserve correctness by requiring a
                // live-DOM read until a later navigation succeeds.
                taints.insert(page_id.clone());
            }
        } else if envelope.command.class() != types::CommandClass::Replayable
            && !side_band_of_the_dom(&envelope.command)
        {
            taints.insert(page_id.clone());
        }
    }

    async fn execute_inner(
        &self,
        envelope: &CommandEnvelope,
        lease: &WorkerLease,
        page: Option<PageState>,
        gate: &SessionGate,
    ) -> Result<AdaptiveExecution, AdaptiveFailure> {
        let vision_gate = gate.vision;
        if let RuntimeCommand::Intent(intent) = &envelope.command {
            let assist = gate
                .vision_node
                .provider(self.vision_assist.clone())
                .map(|assist| match &self.operational_metrics {
                    Some(metrics) => intent_engine::instrument_vision_assist(assist, metrics.clone()),
                    None => assist,
                });
            return execute_intent(
                envelope,
                lease,
                intent,
                vision_gate,
                assist,
                self.proposals.clone(),
                page.as_ref().and_then(|page| page.url.clone()),
                self.context_graph.clone(),
                self.corpus.clone(),
                self.operational_metrics.clone(),
            )
            .await;
        }
        if let RuntimeCommand::Primitive(PrimitiveCommand::ExtractStructured(command)) =
            &envelope.command
        {
            return extract_structured(
                envelope,
                lease,
                command,
                vision_gate,
                self.structured_extractor.clone(),
            )
            .await;
        }
        let RuntimeCommand::Primitive(command) = &envelope.command else {
            unreachable!("Intent handled above");
        };
        // The direct-HTTP path reads and commits the worker's own HTTP state mirror.
        // A worker without that mirror (the Firefox companion) can still serve every
        // eligible command through the browser, so treat it exactly like an unconfigured
        // direct path rather than letting `http_state` fail the command outright.
        let Some(direct) = self
            .direct
            .as_ref()
            .filter(|_| lease.worker().supports_http_state())
        else {
            return browser_execute(
                envelope,
                lease,
                ExecutionPath::Chromium,
                ExecutionReason::IneligibleCommand,
                0,
            )
            .await;
        };
        let page_url = page
            .as_ref()
            .and_then(|page| page.url.as_deref())
            .unwrap_or_default();
        // A whole-page read of a page that has mutated since load must come
        // from the live DOM: a direct-HTTP refetch answers the app shell.
        if matches!(command, PrimitiveCommand::Inspect(_)) {
            let tainted = match envelope.page_id.as_ref() {
                Some(page_id) => self.taints.lock().await.contains(page_id),
                None => false,
            };
            if tainted {
                return browser_execute(
                    envelope,
                    lease,
                    ExecutionPath::Chromium,
                    ExecutionReason::PageMutated,
                    0,
                )
                .await;
            }
        }
        match direct.eligibility.classify(command, page_url) {
            EligibilityDecision::Denied(error) => {
                if matches!(command, PrimitiveCommand::Inspect(_)) {
                    // The direct-HTTP path is a read optimization over a page
                    // the browser already has open. A network-policy denial
                    // there (e.g. loopback) must degrade to the browser, not
                    // fail a DOM read with a network error code. Downloads
                    // keep the hard denial: that denial is the boundary.
                    browser_execute(
                        envelope,
                        lease,
                        ExecutionPath::ChromiumFallback,
                        ExecutionReason::PolicyRequired,
                        0,
                    )
                    .await
                } else {
                    Err(error.into())
                }
            }
            EligibilityDecision::Chromium(reason) => {
                browser_execute(envelope, lease, ExecutionPath::Chromium, reason, 0).await
            }
            EligibilityDecision::DirectHttp(reason) => {
                let download_destination = match command {
                    PrimitiveCommand::DownloadUrl(command) => {
                        match (&direct.downloads_root, command.save_as.as_deref()) {
                            (_, None) => None,
                            (Some(root), Some(save_as)) => Some(
                                DownloadDestination::validate(root, save_as, &envelope.command_id)
                                    .await?,
                            ),
                            (None, Some(_)) => {
                                return Err(download_policy_error(
                                    "download destination storage is not configured",
                                )
                                .into())
                            }
                        }
                    }
                    _ => None,
                };
                let page_id = envelope.page_id.as_ref().expect("validated page id");
                let snapshot = lease.worker().http_state(page_id).await?;
                let version = snapshot.version;
                let candidate = match command {
                    PrimitiveCommand::Inspect(command) => {
                        match direct.executor.inspect(&snapshot, command).await {
                            Ok(candidate) => candidate,
                            // A network-policy denial (e.g. loopback) fires at
                            // fetch time, after eligibility already routed the
                            // read here. Degrade to the browser — which has
                            // the page open — instead of failing a DOM read
                            // with a network error code.
                            Err(error) if error.code == ErrorCode::NetworkPolicyDenied => {
                                return browser_execute(
                                    envelope,
                                    lease,
                                    ExecutionPath::ChromiumFallback,
                                    ExecutionReason::PolicyRequired,
                                    version,
                                )
                                .await;
                            }
                            Err(error) => return Err(error.into()),
                        }
                    }
                    PrimitiveCommand::DownloadUrl(command) => {
                        direct.executor.download(&snapshot, command).await?
                    }
                    _ => unreachable!("eligibility only selects supported HTTP commands"),
                };
                match candidate {
                    HttpCandidate::FallbackRequired(fallback_reason) => {
                        if matches!(command, PrimitiveCommand::Inspect(_)) {
                            browser_execute(
                                envelope,
                                lease,
                                ExecutionPath::ChromiumFallback,
                                fallback_reason,
                                version,
                            )
                            .await
                        } else {
                            Err(equivalence_unproven(fallback_reason).into())
                        }
                    }
                    HttpCandidate::Inspection {
                        evidence,
                        state,
                        meta,
                    } => Ok(AdaptiveExecution {
                        evidence: vec![
                            evidence,
                            execution_evidence(
                                ExecutionPath::DirectHttp,
                                reason,
                                version,
                                ExecutionMetrics::http(meta),
                            ),
                        ],
                        used_browser: false,
                        prepared_http: Some(PreparedHttpResult {
                            state_version: version,
                            state,
                            artifact: None,
                            download: None,
                        }),
                    }),
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
                        let mut saved_to: Option<String> = None;
                        let download = match download_destination {
                            Some(destination) => {
                                saved_to = Some(destination.filename.clone());
                                Some(destination.stage(&bytes).await?)
                            }
                            None => None,
                        };
                        let record = pending.record().clone();
                        Ok(AdaptiveExecution {
                            evidence: vec![
                                Evidence::Download {
                                    filename,
                                    path: record.artifact_id,
                                    bytes: record.bytes,
                                    sha256: record.sha256.clone(),
                                    saved_to,
                                },
                                execution_evidence(
                                    ExecutionPath::DirectHttp,
                                    reason,
                                    version,
                                    ExecutionMetrics::http(meta),
                                ),
                            ],
                            used_browser: false,
                            prepared_http: Some(PreparedHttpResult {
                                state_version: version,
                                state,
                                artifact: Some(pending),
                                download,
                            }),
                        })
                    }
                }
            }
        }
    }
}

/// Byte-index slicing a `String` panics mid-codepoint; back off to a char
/// boundary so non-ASCII content cannot kill the request task.
fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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

fn download_policy_error(message: impl Into<String>) -> CommandError {
    CommandError {
        code: ErrorCode::PolicyDenied,
        message: message.into(),
        layer: ErrorLayer::Page,
        retryable: false,
    }
}

fn download_storage_error(error: impl std::fmt::Display) -> CommandError {
    CommandError {
        code: ErrorCode::Internal,
        message: format!("download destination storage failed: {error}"),
        layer: ErrorLayer::Page,
        retryable: false,
    }
}

fn internal_artifact_error(message: impl Into<String>) -> CommandError {
    CommandError {
        code: ErrorCode::Internal,
        message: message.into(),
        layer: ErrorLayer::Page,
        retryable: false,
    }
}

/// Non-Replayable commands that never touch the document. `DownloadUrl` is a
/// side-band HTTP fetch against the session's own state mirror: it leaves the
/// open page byte-for-byte as it was, so it must not taint the page and push
/// the next whole-page read onto the browser. Tainting it also loses the state
/// the download itself established (a `Set-Cookie` the DOM predates), which the
/// deterministic refetch reflects and a stale DOM does not.
fn side_band_of_the_dom(command: &RuntimeCommand) -> bool {
    matches!(
        command,
        RuntimeCommand::Primitive(PrimitiveCommand::DownloadUrl(_))
    )
}

fn equivalence_unproven(reason: ExecutionReason) -> CommandError {
    CommandError {
        code: ErrorCode::HttpEquivalenceUnproven,
        message: format!("direct download equivalence was not proven: {reason:?}"),
        layer: ErrorLayer::Network,
        retryable: false,
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_intent(
    envelope: &CommandEnvelope,
    lease: &WorkerLease,
    intent: &types::IntentCommand,
    vision_gate: VisionGate,
    assist: Option<Arc<dyn VisionAssist>>,
    proposals: Option<Arc<dyn intent_engine::ProposalLookup>>,
    page_url: Option<String>,
    context_graph: Option<Arc<crate::ContextGraph>>,
    corpus: Option<intent_engine::VisionCorpus>,
    operational_metrics: Option<observability::OperationalMetrics>,
) -> Result<AdaptiveExecution, AdaptiveFailure> {
    let page_id = envelope.page_id.as_ref().expect("validated page id");
    let browser = WorkerIntentBrowser { lease };
    let gates_open = vision_gate.session_ok && vision_gate.capability_ok;
    let recent_command_kinds = context_graph
        .as_ref()
        .map(|graph| graph.recent_command_kinds(page_id))
        .unwrap_or_default();
    let prompt_context = if page_url.is_none() && recent_command_kinds.is_empty() {
        None
    } else {
        Some(intent_engine::VisionPromptContext {
            url: page_url,
            candidates: Vec::new(),
            recent_command_kinds,
        })
    };
    let vision = VisionContext {
        session_ok: vision_gate.session_ok,
        capability_ok: vision_gate.capability_ok,
        assist,
        // The cache is only ever consulted behind both gates; a closed gate
        // gets `None` and the byte-identical pre-prefill path.
        proposals: proposals.filter(|_| gates_open),
        // Escalation deferral is an engine-internal complete_form decision.
        defer_escalation: false,
        prompt_context,
        corpus,
    };
    let outcome = IntentEngine::execute(intent, page_id, &browser, &vision).await;
    record_intent_metrics(operational_metrics.as_ref(), intent, &outcome);
    match outcome {
        IntentOutcome::Completed { evidence } => Ok(AdaptiveExecution {
            evidence,
            used_browser: true,
            prepared_http: None,
        }),
        IntentOutcome::Failed { error, evidence } => Err(AdaptiveFailure { error, evidence }),
    }
}

fn record_intent_metrics(
    metrics: Option<&observability::OperationalMetrics>,
    intent: &types::IntentCommand,
    outcome: &IntentOutcome,
) {
    let Some(metrics) = metrics else { return };
    let kind = match intent {
        types::IntentCommand::Locate(_) => observability::IntentMetricKind::Locate,
        types::IntentCommand::Fill(_) => observability::IntentMetricKind::Fill,
        types::IntentCommand::CompleteForm(_) => observability::IntentMetricKind::CompleteForm,
        types::IntentCommand::SubmitAndVerify(_) => observability::IntentMetricKind::Submit,
        types::IntentCommand::WaitForState(_) => observability::IntentMetricKind::WaitForState,
        types::IntentCommand::Follow(_) => observability::IntentMetricKind::Follow,
        types::IntentCommand::DismissObstruction(_) => observability::IntentMetricKind::Dismiss,
        types::IntentCommand::Extract(_) => observability::IntentMetricKind::Extract,
    };
    let evidence = match outcome {
        IntentOutcome::Completed { evidence } | IntentOutcome::Failed { evidence, .. } => evidence,
    };
    let mut source = observability::ResolutionSource::Deterministic;
    for item in evidence {
        let Evidence::IntentExecution { record } = item else { continue };
        source = match record.resolution_path {
            types::IntentResolutionPath::VisionFallback => {
                observability::ResolutionSource::VisionFallback
            }
            types::IntentResolutionPath::VisionPrefill
                if source != observability::ResolutionSource::VisionFallback =>
            {
                observability::ResolutionSource::VisionPrefill
            }
            types::IntentResolutionPath::Deterministic => source,
            types::IntentResolutionPath::VisionPrefill => source,
        };
    }
    metrics.record_intent_resolution(kind, source);
}

fn execution_evidence(
    path: ExecutionPath,
    reason: ExecutionReason,
    state_version: u64,
    metrics: ExecutionMetrics,
) -> Evidence {
    Evidence::ExecutionPath {
        path,
        reason,
        state_version,
        elapsed_ms: metrics.elapsed_ms,
        bytes: metrics.bytes,
        sha256: metrics.sha256,
        final_url: metrics.final_url,
        content_type: metrics.content_type,
        status: metrics.status,
        redirect_chain: metrics.redirect_chain,
    }
}

struct ExecutionMetrics {
    elapsed_ms: u64,
    bytes: Option<u64>,
    sha256: Option<String>,
    final_url: Option<String>,
    content_type: Option<String>,
    status: Option<u16>,
    redirect_chain: Vec<String>,
}

impl ExecutionMetrics {
    fn http(meta: network_engine::HttpMeta) -> Self {
        Self {
            elapsed_ms: meta.elapsed_ms,
            bytes: Some(meta.bytes),
            sha256: Some(meta.sha256),
            final_url: Some(meta.final_url),
            content_type: Some(meta.content_type),
            status: Some(meta.status),
            redirect_chain: meta.redirect_chain,
        }
    }

    fn browser(bytes: Option<u64>, sha256: Option<String>) -> Self {
        Self {
            elapsed_ms: 0,
            bytes,
            sha256,
            final_url: None,
            content_type: None,
            status: None,
            redirect_chain: Vec::new(),
        }
    }
}

async fn browser_execute(
    envelope: &CommandEnvelope,
    lease: &WorkerLease,
    path: ExecutionPath,
    reason: ExecutionReason,
    state_version: u64,
) -> Result<AdaptiveExecution, AdaptiveFailure> {
    let page_id = envelope.page_id.as_ref();
    let RuntimeCommand::Primitive(command) = &envelope.command else {
        unreachable!("intent commands use execute_intent");
    };
    let mut evidence = match command {
        PrimitiveCommand::Navigate(command) => {
            lease
                .worker()
                .navigate(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::Inspect(command) => {
            lease
                .worker()
                .inspect(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::Click(command) => {
            lease
                .worker()
                .click(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::TypeText(command) => {
            lease
                .worker()
                .type_text(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::ControlAction(command) => {
            lease
                .worker()
                .control_action(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::UploadFiles(command) => {
            lease
                .worker()
                .upload_files(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::OpenPage(command) => lease.worker().open_page_command(command).await?,
        PrimitiveCommand::ListPages(command) => lease.worker().list_pages(command).await?,
        PrimitiveCommand::ClosePage(command) => lease.worker().close_page_command(command).await?,
        PrimitiveCommand::ActivatePage(command) => lease.worker().activate_page(command).await?,
        PrimitiveCommand::AccessibilitySnapshot(command) => {
            lease
                .worker()
                .a11y_snapshot(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::ClickAndWaitForPopup(command) => {
            lease
                .worker()
                .click_and_wait_for_popup(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::ClickAndWaitForDownload(command) => {
            lease
                .worker()
                .click_and_wait_for_download(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::WaitFor(command) => {
            lease
                .worker()
                .wait_for(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::CaptureScreenshot(command) => {
            lease
                .worker()
                .capture_screenshot(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::SetFocusEmulation(command) => {
            lease
                .worker()
                .set_focus_emulation(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::SetEmulatedMedia(command) => {
            lease
                .worker()
                .set_emulated_media(page_id.expect("validated page id"), command)
                .await?
        }
        // Only ChromiumWorker::evaluate_javascript executes the JS; other workers
        // return the default unsupported CommandError.
        PrimitiveCommand::EvaluateJavaScript(command) => {
            lease
                .worker()
                .evaluate_javascript(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::NetworkLog(command) => {
            lease
                .worker()
                .network_log(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::Emulate(command) => {
            lease
                .worker()
                .emulate(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::HandleDialog(command) => {
            lease
                .worker()
                .handle_dialog(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::PrintToPdf(command) => {
            lease
                .worker()
                .print_to_pdf(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::GetCookies(command) => {
            lease
                .worker()
                .get_cookies(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::SetCookies(command) => {
            lease
                .worker()
                .set_cookies(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::DeleteCookies(command) => {
            lease
                .worker()
                .delete_cookies(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::ExtractStructured(_) => {
            unreachable!("structured extraction is intercepted in execute")
        }
        PrimitiveCommand::DownloadUrl(_) => {
            return Err(equivalence_unproven(reason).into());
        }
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
        ExecutionMetrics::browser(bytes, sha256),
    ));
    Ok(AdaptiveExecution {
        evidence,
        used_browser: true,
        prepared_http: None,
    })
}

const MAX_EXTRACT_CONTENT_BYTES: usize = 16 * 1024;
const MAX_EXTRACT_RESULT_BYTES: usize = 64 * 1024;
const MAX_EXTRACT_SCHEMA_BYTES: usize = 16 * 1024;

/// Structured extraction over the configured provider. Shares the vision
/// double gate (session policy + token capability + configured provider)
/// because page content leaves the runtime toward an external model.
async fn extract_structured(
    envelope: &CommandEnvelope,
    lease: &WorkerLease,
    command: &types::ExtractStructuredCommand,
    vision_gate: VisionGate,
    assist: Option<Arc<dyn intent_engine::StructuredExtractor>>,
) -> Result<AdaptiveExecution, AdaptiveFailure> {
    let denied = || {
        AdaptiveFailure {
        error: CommandError {
            code: ErrorCode::VisionAssistDenied,
            message: "structured extraction requires vision:assist capability, session vision policy, and a configured provider".into(),
            layer: ErrorLayer::Workflow,
            retryable: false,
        },
        evidence: Vec::new(),
    }
    };
    if !vision_gate.session_ok || !vision_gate.capability_ok {
        return Err(denied());
    }
    let Some(assist) = assist else {
        return Err(denied());
    };
    let schema_bytes = serde_json::to_vec(&command.schema).map_err(|_| AdaptiveFailure {
        error: CommandError {
            code: ErrorCode::InvalidRequest,
            message: "extraction schema is not serializable".into(),
            layer: ErrorLayer::Workflow,
            retryable: false,
        },
        evidence: Vec::new(),
    })?;
    if schema_bytes.len() > MAX_EXTRACT_SCHEMA_BYTES {
        return Err(AdaptiveFailure {
            error: CommandError {
                code: ErrorCode::InvalidRequest,
                message: format!("extraction schema exceeds {MAX_EXTRACT_SCHEMA_BYTES} bytes"),
                layer: ErrorLayer::Workflow,
                retryable: false,
            },
            evidence: Vec::new(),
        });
    }
    jsonschema::validator_for(&command.schema).map_err(|_| AdaptiveFailure {
        error: CommandError {
            code: ErrorCode::InvalidRequest,
            message: "extraction schema is not a valid JSON schema".into(),
            layer: ErrorLayer::Workflow,
            retryable: false,
        },
        evidence: Vec::new(),
    })?;

    let page_id = envelope.page_id.as_ref().expect("validated page id");
    let mut evidence = lease
        .worker()
        .inspect(page_id, &types::InspectCommand::default())
        .await?;
    let content = evidence
        .iter()
        .find_map(|item| match item {
            Evidence::Inspection { text, .. } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let content = truncate_utf8(&content, MAX_EXTRACT_CONTENT_BYTES).to_owned();

    let value = assist
        .extract_structured(intent_engine::StructuredExtractRequest {
            schema: command.schema.clone(),
            content,
            purpose: command.purpose.clone(),
        })
        .await?;

    let validator = jsonschema::validator_for(&command.schema).map_err(|_| AdaptiveFailure {
        error: CommandError {
            code: ErrorCode::InvalidRequest,
            message: "extraction schema is not a valid JSON schema".into(),
            layer: ErrorLayer::Workflow,
            retryable: false,
        },
        evidence: Vec::new(),
    })?;
    if validator.validate(&value).is_err() {
        return Err(AdaptiveFailure {
            error: CommandError {
                code: ErrorCode::VerificationFailed,
                message: "provider result does not match the extraction schema".into(),
                layer: ErrorLayer::Workflow,
                retryable: true,
            },
            evidence: Vec::new(),
        });
    }
    let result_bytes = serde_json::to_vec(&value).unwrap_or_default();
    if result_bytes.len() > MAX_EXTRACT_RESULT_BYTES {
        return Err(AdaptiveFailure {
            error: CommandError {
                code: ErrorCode::VerificationFailed,
                message: format!("provider result exceeds {MAX_EXTRACT_RESULT_BYTES} bytes"),
                layer: ErrorLayer::Workflow,
                retryable: false,
            },
            evidence: Vec::new(),
        });
    }
    evidence.push(Evidence::StructuredExtraction {
        page_id: page_id.clone(),
        value,
        truncated: false,
    });
    evidence.push(execution_evidence(
        ExecutionPath::Chromium,
        ExecutionReason::IneligibleCommand,
        0,
        ExecutionMetrics::browser(None, None),
    ));
    Ok(AdaptiveExecution {
        evidence,
        used_browser: true,
        prepared_http: None,
    })
}

#[cfg(test)]
mod tests {
    use super::{metadata_name, staging_name, truncate_utf8, SecureDownloadRoot, StagingCleanup};

    #[cfg(unix)]
    #[test]
    fn dropping_unjournaled_staging_cleans_every_private_entry() {
        let root = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let secure = SecureDownloadRoot::open(&canonical_root).unwrap();
        let staging = staging_name("cancelled-command").unwrap();
        let metadata = metadata_name("cancelled-command").unwrap();
        drop(secure.create_file(&staging).unwrap());
        drop(secure.create_file(&metadata).unwrap());

        drop(StagingCleanup {
            root: &secure,
            staging_name: staging.clone(),
            metadata_name: metadata.clone(),
            armed: true,
        });

        assert!(!secure.exists(&staging).unwrap());
        assert!(!secure.exists(&metadata).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn replaced_fifo_entries_fail_closed_without_blocking() {
        use std::ffi::CString;

        let root = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let secure = SecureDownloadRoot::open(&canonical_root).unwrap();
        let fifo = canonical_root.join("staging-fifo");
        let fifo_c = CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        // SAFETY: the path is NUL-terminated and points into this test's private directory.
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);

        assert!(secure
            .file_matches("staging-fifo", "00", 1)
            .unwrap()
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn secure_download_root_rejects_a_symlink_component() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        let linked = root.path().join("linked");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, &linked).unwrap();

        assert!(SecureDownloadRoot::open(&linked).is_err());
    }

    #[test]
    fn truncation_never_splits_a_codepoint() {
        // 'é' is two bytes; a cut at 1 would panic on a byte-index slice.
        let text = "é".repeat(100);
        let cut = truncate_utf8(&text, 51);
        assert_eq!(cut.len(), 50);
        assert!(cut.is_char_boundary(cut.len()));

        let ascii = "a".repeat(100);
        assert_eq!(truncate_utf8(&ascii, 51).len(), 51);

        let short = "héllo";
        assert_eq!(truncate_utf8(short, 100), short);
    }
}
