//! The enrolled Firefox is brought to the companion build installed in its
//! profile without a manual step: a stale build is reloaded or Firefox is
//! restarted, and a Firefox that is not running is started. Each test uses
//! its own profile with the companion sideloaded the way `bobby install`
//! does, and the scoped test native host.

mod support;

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use cli::EnrolledFirefoxProfile;
use companion_core::CompanionServerHandle;
use config::{
    AppConfig, BrowserEngineConfig, BrowserSelectionConfig, EnginePreferenceConfig,
    FirefoxCompanionConfig,
};
use support::rig::{Live, Rig};
use test_site::{FixtureSite, Route};
use tokio::process::{Child, Command};
use types::ProfileId;
use url::Url;

const EXTENSION_ID: &str = "firefox-companion@bobby-browser.local";
const PLACEHOLDER: &str = "@@BOBBY_EXTENSION_BUILD_ID@@";
const OLD_BUILD: &str = "00000000000000000000000000000a11";
const TIMEOUT: Duration = Duration::from_secs(60);

struct Env {
    firefox_bin: PathBuf,
    extension: PathBuf,
    proof_dir: PathBuf,
}

impl Env {
    fn load() -> Self {
        let required = |name: &str| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| panic!("{name} must be set to run the Firefox suite"))
        };
        // Firefox started by the runtime itself inherits this.
        if std::env::var_os("BOBBY_FIREFOX_HEADLESS").is_some_and(|value| value == "1") {
            std::env::set_var("MOZ_HEADLESS", "1");
        }
        Self {
            firefox_bin: required("BOBBY_FIREFOX_BIN"),
            extension: required("BOBBY_COMPANION_EXTENSION"),
            proof_dir: required("BOBBY_FIREFOX_PROOF_DIR"),
        }
    }

    fn descriptor(&self) -> PathBuf {
        self.proof_dir.join("native-host-descriptor.json")
    }

    /// The id the extension build under test was stamped with.
    fn installed_build(&self) -> String {
        build_id_file(&self.extension).expect("the test extension carries build-id.json")
    }
}

fn build_id_file(extension: &Path) -> Option<String> {
    let bytes = std::fs::read(extension.join("build-id.json")).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value["buildId"].as_str().map(str::to_owned)
}

/// A profile that accepts the unsigned companion as a profile sideload.
fn new_profile(env: &Env) -> tempfile::TempDir {
    let profile = tempfile::Builder::new()
        .prefix("lifecycle-profile-")
        .tempdir_in(&env.proof_dir)
        .expect("create Firefox profile");
    std::fs::write(
        profile.path().join("user.js"),
        [
            r#"user_pref("xpinstall.signatures.required", false);"#,
            r#"user_pref("extensions.autoDisableScopes", 14);"#,
            r#"user_pref("privacy.resistFingerprinting", false);"#,
            r#"user_pref("browser.shell.checkDefaultBrowser", false);"#,
            r#"user_pref("browser.aboutwelcome.enabled", false);"#,
            r#"user_pref("browser.startup.homepage_override.mstone", "ignore");"#,
            r#"user_pref("datareporting.policy.dataSubmissionEnabled", false);"#,
            "",
        ]
        .join("\n"),
    )
    .expect("write profile prefs");
    profile
}

/// Install the test extension into the profile the way `bobby install`
/// sideloads it. `build` restamps it as another build (`None`: a build from
/// before the stamp, which reports no id). Returns the id it reports.
fn install_extension(env: &Env, profile: &Path, build: Option<Option<&str>>) -> Option<String> {
    let target = profile.join("extensions").join(EXTENSION_ID);
    if target.exists() {
        std::fs::remove_dir_all(&target).expect("remove the installed companion");
    }
    copy_dir(&env.extension, &target);
    let stamped = env.installed_build();
    let Some(build) = build else {
        return Some(stamped);
    };
    let background = target.join("background.js");
    let source = std::fs::read_to_string(&background).expect("read background.js");
    assert_eq!(source.matches(&stamped).count(), 1, "one stamped build id");
    std::fs::write(
        &background,
        source.replace(&stamped, build.unwrap_or(PLACEHOLDER)),
    )
    .expect("restamp background.js");
    match build {
        Some(build) => std::fs::write(
            target.join("build-id.json"),
            format!(r#"{{"buildId":"{build}"}}"#),
        )
        .expect("restamp build-id.json"),
        None => std::fs::remove_file(target.join("build-id.json")).expect("unstamp"),
    }
    build.map(str::to_owned)
}

fn copy_dir(source: &Path, dest: &Path) {
    std::fs::create_dir_all(dest).expect("create extension dir");
    for entry in std::fs::read_dir(source).expect("read extension dir") {
        let entry = entry.expect("extension entry");
        let target = dest.join(entry.file_name());
        if entry.file_type().expect("entry type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy extension file");
        }
    }
}

/// The profile's Firefox, started the way the enrolled profile is started.
struct Browser {
    profile: PathBuf,
    child: Option<Child>,
}

impl Browser {
    async fn launch(env: &Env, profile: &Path) -> (Self, Url) {
        let mut command = Command::new(&env.firefox_bin);
        command
            .arg("--no-remote")
            .arg("--foreground")
            .arg("--profile")
            .arg(profile)
            .arg("--remote-debugging-port=0");
        if std::env::var_os("MOZ_HEADLESS").is_some() {
            command.arg("--headless");
        }
        let child = command
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("launch Firefox");
        let browser = Self {
            profile: profile.to_path_buf(),
            child: Some(child),
        };
        let url = wait_for_endpoint(profile).await;
        (browser, url)
    }

    /// Stop the browser the test started.
    async fn stop_started(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    /// Stop whichever Firefox now owns the test profile: the runtime may have
    /// replaced the one the test started.
    async fn stop(mut self) {
        self.stop_started().await;
        for pid in profile_owner_pids(&self.profile) {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while !profile_owner_pids(&self.profile).is_empty() {
            if tokio::time::Instant::now() >= deadline {
                for pid in profile_owner_pids(&self.profile) {
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

/// A failing test still stops every Firefox on its profile.
impl Drop for Browser {
    fn drop(&mut self) {
        for pid in profile_owner_pids(&self.profile) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// Firefox processes started on exactly this test profile.
fn profile_owner_pids(profile: &Path) -> Vec<i32> {
    let output = std::process::Command::new("ps")
        .args(["-A", "-ww", "-o", "pid=,command="])
        .output()
        .expect("list processes");
    let needle = format!("--profile {} ", profile.display());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.to_ascii_lowercase().contains("firefox") && line.contains(&needle))
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}

async fn wait_for_endpoint(profile: &Path) -> Url {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Ok(url) = firefox_companion::read_bidi_url_from_profile_dir(profile) {
                return url;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("Firefox published its BiDi endpoint")
}

/// Start Firefox on the profile and pair its sideloaded companion.
async fn enroll(env: &Env, profile: &Path) -> (Browser, EnrolledFirefoxProfile, Url) {
    let enrollment = cli::start_firefox_profile_enrollment(
        cli::FirefoxProfileEnrollmentConfig {
            companion_bind: "127.0.0.1:0".parse().unwrap(),
            descriptor_path: env.descriptor(),
            timeout: TIMEOUT,
            pairing_code_ttl: TIMEOUT,
            attachment_ttl: Duration::from_secs(300),
        },
        Arc::new(|_| {}),
    )
    .await
    .expect("start enrollment");
    let (browser, bidi_url) = Browser::launch(env, profile).await;
    let enrolled = enrollment
        .wait()
        .await
        .expect("the sideloaded companion pairs");
    (browser, enrolled, bidi_url)
}

/// The runtime `bobby serve` composes for the enrolled profile.
fn runtime(
    env: &Env,
    profile: &Path,
    enrolled: EnrolledFirefoxProfile,
    bidi_url: &Url,
) -> impl FnOnce(&AppConfig) -> Arc<dyn worker_pool::WorkerFactory> {
    let profile_id = enrolled.profile_id().0.to_string();
    let selection = BrowserSelectionConfig {
        preference: EnginePreferenceConfig::Exact {
            engine: BrowserEngineConfig::Firefox,
            profile_id: Some(profile_id.clone()),
        },
        firefox: vec![FirefoxCompanionConfig {
            profile_id,
            bidi_url: bidi_url.to_string(),
            profile_dir: profile.to_path_buf(),
            companion_bind: "127.0.0.1:0".into(),
            descriptor_path: env.descriptor(),
            timeout_ms: TIMEOUT.as_millis() as u64,
            pairing_code_ttl_ms: TIMEOUT.as_millis() as u64,
            attachment_ttl_ms: 300_000,
        }],
    };
    move |config| {
        cli::compose_worker_factory_with_enrolled_firefox(
            config,
            selection,
            Arc::new(|_| {}),
            enrolled,
        )
        .expect("compose the enrolled Firefox runtime")
    }
}

async fn page_site() -> FixtureSite {
    FixtureSite::spawn(vec![(
        "/ready",
        Route::Html(
            "<!doctype html><html><head><title>Ready</title></head><body><main><h1>Ready</h1><button>Go</button></main></body></html>"
                .into(),
        ),
    )])
    .await
}

/// `workflow_start` completes on the page and its snapshot is served.
async fn serve_session(rig: &Rig, site: &FixtureSite) {
    let live = Live::open(rig, &site.url("/ready")).await;
    let snapshot = live.snapshot(serde_json::json!({})).await;
    support::rig::assert_node(&snapshot, "button", Some("Go"));
    live.close().await;
}

async fn connected_build(server: &CompanionServerHandle, profile: &ProfileId) -> Option<String> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Some(connection) = server.extension_connection(profile).await {
                return connection.build_id().map(str::to_owned);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the companion is connected")
}

async fn stale_build_is_replaced(old: Option<&str>) {
    let env = Env::load();
    let installed = env.installed_build();
    let profile = new_profile(&env);
    let running = install_extension(&env, profile.path(), Some(old));
    assert_ne!(running.as_deref(), Some(installed.as_str()));
    let (browser, enrolled, bidi_url) = enroll(&env, profile.path()).await;
    let profile_id = enrolled.profile_id().clone();
    let server = enrolled.companion_server();
    assert_eq!(connected_build(&server, &profile_id).await, running);

    // An install replaces the companion on disk while Firefox runs the old one.
    install_extension(&env, profile.path(), None);
    let rig = Rig::firefox_composed(runtime(&env, profile.path(), enrolled, &bidi_url)).await;
    let site = page_site().await;
    serve_session(&rig, &site).await;

    assert_eq!(
        connected_build(&server, &profile_id).await.as_deref(),
        Some(installed.as_str())
    );
    drop(rig);
    browser.stop().await;
}

#[tokio::test]
#[ignore = "requires installed Firefox and the scoped test native host"]
async fn a_stale_companion_is_reloaded_to_the_installed_build() {
    stale_build_is_replaced(Some(OLD_BUILD)).await;
}

#[tokio::test]
#[ignore = "requires installed Firefox and the scoped test native host"]
async fn a_companion_without_a_build_id_is_replaced_by_restarting_firefox() {
    stale_build_is_replaced(None).await;
}

#[tokio::test]
#[ignore = "requires installed Firefox and the scoped test native host"]
async fn workflow_start_starts_the_enrolled_firefox_when_it_is_not_running() {
    let env = Env::load();
    let installed = env.installed_build();
    let profile = new_profile(&env);
    install_extension(&env, profile.path(), None);
    let (mut browser, enrolled, bidi_url) = enroll(&env, profile.path()).await;
    let profile_id = enrolled.profile_id().clone();
    let server = enrolled.companion_server();
    browser.stop_started().await;
    assert!(
        profile_owner_pids(profile.path()).is_empty(),
        "Firefox is not running"
    );

    let rig = Rig::firefox_composed(runtime(&env, profile.path(), enrolled, &bidi_url)).await;
    let site = page_site().await;
    serve_session(&rig, &site).await;

    assert!(
        !profile_owner_pids(profile.path()).is_empty(),
        "the runtime started Firefox"
    );
    assert_eq!(
        connected_build(&server, &profile_id).await.as_deref(),
        Some(installed.as_str())
    );
    drop(rig);
    browser.stop().await;
}
