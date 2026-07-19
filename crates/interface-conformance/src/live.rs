use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{Duration, Utc};
use interface_core::{AuthorityStore, CapabilityHandle};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use tempfile::TempDir;
use types::{Capability, PrincipalId, RequestContext};

/// Reusable real-Chrome fixture shared by every interface conformance adapter.
/// It owns all profile, upload, download, artifact, journal, and checkpoint state.
pub struct ChromeRuntimeHarness {
    _root: TempDir,
    site: test_site::FixtureServer,
    pub authority: Arc<AuthorityStore>,
    pub runtime: Arc<AuthenticatedRuntime>,
    pub service: RuntimeService,
    pub handle: CapabilityHandle,
    pub token: String,
    pub denied_token: String,
    pub config: config::AppConfig,
}

impl ChromeRuntimeHarness {
    pub async fn start() -> Self {
        let root = tempfile::tempdir().expect("create conformance root");
        let site = test_site::spawn().await;
        let mut config = config::AppConfig::default();
        config.browser.executable = Some(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ));
        config.browser.profiles_dir = root.path().join("profiles");
        config.browser.upload_roots = vec![root.path().join("uploads")];
        config.browser.downloads_dir = root.path().join("downloads");
        config.browser.artifacts_dir = root.path().join("artifacts");
        config.storage.journal_path = root.path().join("commands.jsonl");
        config.storage.checkpoints_dir = root.path().join("checkpoints");
        config.http.allow_loopback = true;
        for path in [
            &config.browser.upload_roots[0],
            &config.browser.downloads_dir,
            &config.browser.artifacts_dir,
            &config.storage.checkpoints_dir,
        ] {
            std::fs::create_dir_all(path).expect("create confined conformance directory");
        }
        let service = RuntimeService::build(&config)
            .await
            .expect("build real Chrome runtime");
        let authority = Arc::new(AuthorityStore::in_memory());
        let expires = Utc::now() + Duration::minutes(5);
        let token = authority
            .issue(
                PrincipalId::from_uuid(uuid::Uuid::new_v4()),
                all_capabilities(),
                expires,
            )
            .await
            .expect("issue conformance token")
            .expose_once();
        let denied_token = authority
            .issue(PrincipalId::from_uuid(uuid::Uuid::new_v4()), [], expires)
            .await
            .expect("issue denied token")
            .expose_once();
        let handle = authority
            .verify(&token)
            .await
            .expect("verify conformance token");
        let runtime = Arc::new(AuthenticatedRuntime::new(service.clone(), handle.clone()));
        Self {
            _root: root,
            site,
            authority,
            runtime,
            service,
            handle,
            token,
            denied_token,
            config,
        }
    }

    pub fn context(&self) -> RequestContext {
        self.handle
            .context(Utc::now() + Duration::seconds(30), None)
    }
    pub fn site_url(&self) -> String {
        self.site.base_url()
    }
    pub fn upload_root(&self) -> &Path {
        &self.config.browser.upload_roots[0]
    }
}

pub fn all_capabilities() -> [Capability; 12] {
    [
        Capability::SessionRead,
        Capability::SessionWrite,
        Capability::PageRead,
        Capability::PageWrite,
        Capability::BrowserMutate,
        Capability::FileUpload,
        Capability::FileDownload,
        Capability::JavascriptEvaluate,
        Capability::ArtifactRead,
        Capability::ArtifactCapture,
        Capability::RecoveryRead,
        Capability::RecoveryWrite,
    ]
}
