//! The enrolled Firefox is brought to the companion build installed in its
//! profile without a manual step: a stale build is reloaded or Firefox is
//! restarted, and a Firefox that is not running is started. Each test uses
//! its own profile with the companion sideloaded the way `bobby install`
//! does, and the scoped test native host.
//!
//! Unix only: restarting the enrolled Firefox and the test's own process
//! cleanup go through `ps`, `lsof`, and signals.
#![cfg(unix)]

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
        // `RUST_LOG` prints the runtime's own events, such as each Firefox restart.
        std::mem::forget(observability::init_stdio());
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

/// A profile with the add-on prefs `bobby install` writes: it accepts the
/// unsigned companion as a profile sideload and scans it at startup.
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
            r#"user_pref("extensions.startupScanScopes", 1);"#,
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
fn install_extension(
    env: &Env,
    profile: &Path,
    layout: Layout,
    build: Option<Option<&str>>,
) -> Option<String> {
    let staging = tempfile::Builder::new()
        .prefix("lifecycle-extension-")
        .tempdir_in(&env.proof_dir)
        .expect("create extension staging dir");
    let staged = staging.path().join("extension");
    copy_dir(&env.extension, &staged);
    let stamped = env.installed_build();
    let reported = match build {
        None => Some(stamped),
        Some(build) => {
            let background = staged.join("background.js");
            let source = std::fs::read_to_string(&background).expect("read background.js");
            assert_eq!(source.matches(&stamped).count(), 1, "one stamped build id");
            std::fs::write(
                &background,
                source.replace(&stamped, build.unwrap_or(PLACEHOLDER)),
            )
            .expect("restamp background.js");
            match build {
                Some(build) => std::fs::write(
                    staged.join("build-id.json"),
                    format!(r#"{{"buildId":"{build}"}}"#),
                )
                .expect("restamp build-id.json"),
                None => std::fs::remove_file(staged.join("build-id.json")).expect("unstamp"),
            }
            build.map(str::to_owned)
        }
    };
    let extensions = profile.join("extensions");
    let unpacked = extensions.join(EXTENSION_ID);
    if unpacked.exists() {
        std::fs::remove_dir_all(&unpacked).expect("remove the unpacked companion");
    }
    match layout {
        Layout::Unpacked => copy_dir(&staged, &unpacked),
        Layout::Packed => {
            let xpi = staging.path().join("extension.xpi");
            pack_xpi(&staged, &xpi);
            std::fs::create_dir_all(&extensions).expect("create extensions dir");
            // `bobby install` copies the signed build over the installed file.
            std::fs::copy(&xpi, extensions.join(format!("{EXTENSION_ID}.xpi")))
                .expect("install the packed companion");
        }
    }
    reported
}

/// The two ways `bobby install` places the companion in the profile.
#[derive(Clone, Copy)]
enum Layout {
    /// `extensions/<id>/`, the unsigned sideload.
    Unpacked,
    /// `extensions/<id>.xpi`, the signed release build.
    Packed,
}

fn pack_xpi(source: &Path, xpi: &Path) {
    fn add(writer: &mut zip::ZipWriter<std::fs::File>, root: &Path, dir: &Path) {
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .expect("read extension dir")
            .map(|entry| entry.expect("extension entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                add(writer, root, &path);
                continue;
            }
            let name = path
                .strip_prefix(root)
                .expect("entry under the extension")
                .to_str()
                .expect("UTF-8 entry name")
                .replace(std::path::MAIN_SEPARATOR, "/");
            writer.start_file(name, options).expect("start xpi entry");
            std::io::Write::write_all(writer, &std::fs::read(&path).expect("read entry"))
                .expect("write xpi entry");
        }
    }
    let mut writer = zip::ZipWriter::new(std::fs::File::create(xpi).expect("create xpi"));
    add(&mut writer, source, source);
    writer.finish().expect("finish xpi");
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
        let log = profile.join("firefox.log");
        let output = std::fs::File::create(&log).expect("create the Firefox log");
        let child = command
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(output.try_clone().expect("share the Firefox log"))
            .stderr(output)
            .kill_on_drop(true)
            .spawn()
            .expect("launch Firefox");
        let mut browser = Self {
            profile: profile.to_path_buf(),
            child: Some(child),
        };
        let url = match tokio::time::timeout(TIMEOUT, wait_for_endpoint(profile)).await {
            Ok(url) => url,
            Err(_) => {
                let state = match browser.child.as_mut().map(|child| child.try_wait()) {
                    Some(Ok(Some(status))) => format!("exited with {status}"),
                    _ => "still running".to_owned(),
                };
                let output = std::fs::read_to_string(&log).unwrap_or_default();
                let lines: Vec<&str> = output.lines().collect();
                let tail = lines[lines.len().saturating_sub(40)..].join("\n");
                panic!("Firefox published no BiDi endpoint and is {state}; output:\n{tail}");
            }
        };
        (browser, url)
    }

    /// Stop the browser the test started.
    async fn stop_started(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    /// How the Firefox the test started exited.
    async fn started_exit(&mut self) -> std::process::ExitStatus {
        let child = self.child.as_mut().expect("the test started Firefox");
        tokio::time::timeout(TIMEOUT, child.wait())
            .await
            .expect("the started Firefox exited")
            .expect("wait for the started Firefox")
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
    loop {
        if let Ok(url) = firefox_companion::read_bidi_url_from_profile_dir(profile) {
            return url;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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
    let selection = selection(env, profile, enrolled.profile_id(), bidi_url);
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

/// The runtime `bobby serve` composes on a later start of the same profile.
fn restarted_runtime(
    env: &Env,
    profile: &Path,
    profile_id: &ProfileId,
    bidi_url: &Url,
) -> impl FnOnce(&AppConfig) -> Arc<dyn worker_pool::WorkerFactory> {
    let selection = selection(env, profile, profile_id, bidi_url);
    move |config| {
        firefox_companion::selection::compose_worker_factory_warm(config, selection)
            .expect("compose the restarted Firefox runtime")
    }
}

/// A runtime over `compose`, and its factory: `bobby serve` shuts the
/// factory down after its serve loop ends.
async fn serving(
    compose: impl FnOnce(&AppConfig) -> Arc<dyn worker_pool::WorkerFactory>,
) -> (Rig, Arc<dyn worker_pool::WorkerFactory>) {
    let mut factory = None;
    let rig = Rig::firefox_composed(|config| {
        let composed = compose(config);
        factory = Some(Arc::clone(&composed));
        composed
    })
    .await;
    (rig, factory.expect("the rig composed a factory"))
}

fn selection(
    env: &Env,
    profile: &Path,
    profile_id: &ProfileId,
    bidi_url: &Url,
) -> BrowserSelectionConfig {
    let profile_id = profile_id.0.to_string();
    BrowserSelectionConfig {
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
        chromium: Vec::new(),
    }
}

async fn page_site() -> FixtureSite {
    FixtureSite::spawn(vec![(
        "/ready",
        Route::Html(
            "<!doctype html><html><head><title>Ready</title></head><body><main><h1>Ready</h1><input aria-label=\"Name\"><button>Go</button></main></body></html>"
                .into(),
        ),
    )])
    .await
}

/// `workflow_start` completes on the page, then its snapshot, typing, and a
/// click are served in the same session.
async fn serve_session(rig: &Rig, site: &FixtureSite) {
    let live = Live::open(rig, &site.url("/ready")).await;
    let snapshot = live.snapshot(serde_json::json!({})).await;
    support::rig::assert_node(&snapshot, "button", Some("Go"));
    let typed = live
        .call(
            "type_text",
            serde_json::json!({"target":{"role":"textbox","accessibleName":"Name"},
                               "value":"bobby","clearFirst":true}),
        )
        .await;
    assert_eq!(typed["status"], "completed", "type_text: {typed}");
    let clicked = live
        .call(
            "click",
            serde_json::json!({"target":{"role":"button","accessibleName":"Go"}}),
        )
        .await;
    assert_eq!(clicked["status"], "completed", "click: {clicked}");
    live.close().await;
}

/// `/sign-in#<value>` stores a persistent `login` cookie; `/account` shows
/// the cookies the page sees in its heading.
async fn login_site() -> FixtureSite {
    FixtureSite::spawn(vec![
        (
            "/sign-in",
            Route::Html(
                "<!doctype html><html><head><title>Sign in</title><script>document.cookie='login='+location.hash.slice(1)+'; max-age=86400; path=/';</script></head><body><h1>Signed in</h1></body></html>"
                    .into(),
            ),
        ),
        (
            "/account",
            Route::Html(
                "<!doctype html><html><head><title>Account</title></head><body><h1 id=\"who\"></h1><script>document.getElementById('who').textContent='account '+document.cookie;</script></body></html>"
                    .into(),
            ),
        ),
    ])
    .await
}

async fn sign_in(rig: &Rig, site: &FixtureSite, login: &str) {
    let live = Live::open(rig, &format!("{}#{login}", site.url("/sign-in"))).await;
    support::rig::assert_node(
        &live.snapshot(serde_json::json!({})).await,
        "heading",
        Some("Signed in"),
    );
    live.close().await;
}

async fn assert_signed_in(rig: &Rig, site: &FixtureSite, login: &str) {
    let live = Live::open(rig, &site.url("/account")).await;
    let expected = format!("account login={login}");
    support::rig::assert_node(
        &live.snapshot(serde_json::json!({})).await,
        "heading",
        Some(&expected),
    );
    live.close().await;
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_millis() as u64
}

/// How often the companion started since `since`: each start spawns one
/// scoped test native host, which logs `start` next to the descriptor.
fn companion_starts_since(env: &Env, since: u64) -> usize {
    let log = env.proof_dir.join(cli::FIREFOX_NATIVE_HOST_LOG);
    let mut rotated = log.clone().into_os_string();
    rotated.push(".1");
    [PathBuf::from(rotated), log]
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .flat_map(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .collect::<Vec<_>>()
        })
        .filter(|event| {
            event["event"] == "start" && event["unixMs"].as_u64().is_some_and(|at| at >= since)
        })
        .count()
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

async fn stale_build_is_replaced(layout: Layout, old: Option<&str>) {
    let env = Env::load();
    let installed = env.installed_build();
    let profile = new_profile(&env);
    let running = install_extension(&env, profile.path(), layout, Some(old));
    assert_ne!(running.as_deref(), Some(installed.as_str()));
    let (browser, enrolled, bidi_url) = enroll(&env, profile.path()).await;
    let profile_id = enrolled.profile_id().clone();
    let server = enrolled.companion_server();
    assert_eq!(connected_build(&server, &profile_id).await, running);

    // An install replaces the companion on disk while Firefox runs the old one.
    install_extension(&env, profile.path(), layout, None);
    let rig = Rig::firefox_composed(runtime(&env, profile.path(), enrolled, &bidi_url)).await;
    let site = page_site().await;
    // The session that triggers the heal and a later one both run on the one
    // companion start the heal made.
    let healing = unix_ms();
    serve_session(&rig, &site).await;
    serve_session(&rig, &site).await;
    assert_eq!(companion_starts_since(&env, healing), 1);

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
    stale_build_is_replaced(Layout::Unpacked, Some(OLD_BUILD)).await;
}

#[tokio::test]
#[ignore = "requires installed Firefox and the scoped test native host"]
async fn a_companion_without_a_build_id_is_replaced_by_restarting_firefox() {
    stale_build_is_replaced(Layout::Unpacked, None).await;
}

#[tokio::test]
#[ignore = "requires installed Firefox and the scoped test native host"]
async fn a_stale_packed_companion_is_replaced_by_the_installed_xpi() {
    stale_build_is_replaced(Layout::Packed, Some(OLD_BUILD)).await;
}

#[tokio::test]
#[ignore = "requires installed Firefox and the scoped test native host"]
async fn workflow_start_starts_the_enrolled_firefox_when_it_is_not_running() {
    let env = Env::load();
    let installed = env.installed_build();
    let profile = new_profile(&env);
    install_extension(&env, profile.path(), Layout::Unpacked, None);
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

#[tokio::test]
#[ignore = "requires installed Firefox and the scoped test native host"]
async fn a_runtime_restart_keeps_firefox_and_its_logins() {
    let env = Env::load();
    let profile = new_profile(&env);
    install_extension(&env, profile.path(), Layout::Unpacked, None);
    let (browser, enrolled, bidi_url) = enroll(&env, profile.path()).await;
    let profile_id = enrolled.profile_id().clone();
    let firefox = profile_owner_pids(profile.path());
    let site = login_site().await;
    let login = uuid::Uuid::new_v4().simple().to_string();

    let (rig, factory) = serving(runtime(&env, profile.path(), enrolled, &bidi_url)).await;
    sign_in(&rig, &site, &login).await;
    drop(rig);
    factory.shutdown().await;

    let (rig, factory) = serving(restarted_runtime(
        &env,
        profile.path(),
        &profile_id,
        &bidi_url,
    ))
    .await;
    assert_signed_in(&rig, &site, &login).await;
    assert_eq!(
        profile_owner_pids(profile.path()),
        firefox,
        "the runtime restart kept the running Firefox"
    );
    drop(rig);
    factory.shutdown().await;
    browser.stop().await;
}

#[tokio::test]
#[ignore = "requires installed Firefox and the scoped test native host"]
async fn a_firefox_restart_exits_cleanly_and_keeps_logins() {
    let env = Env::load();
    let profile = new_profile(&env);
    // Only a Firefox restart replaces a companion that reports no build.
    install_extension(&env, profile.path(), Layout::Unpacked, Some(None));
    let (mut browser, enrolled, bidi_url) = enroll(&env, profile.path()).await;
    install_extension(&env, profile.path(), Layout::Unpacked, None);
    let site = login_site().await;
    let login = uuid::Uuid::new_v4().simple().to_string();
    let expiry = unix_ms() / 1000 + 86_400;
    let client = firefox_companion::BidiClient::connect_session(bidi_url.clone(), TIMEOUT)
        .await
        .expect("open a BiDi session");
    client
        .send(
            "storage.setCookie",
            serde_json::json!({"cookie":{"name":"login","value":{"type":"string","value":login},
                               "domain":"127.0.0.1","path":"/","expiry":expiry}}),
        )
        .await
        .expect("store the login cookie");
    client.end_session().await.expect("end the BiDi session");

    let (rig, factory) = serving(runtime(&env, profile.path(), enrolled, &bidi_url)).await;
    let live = Live::open(&rig, &site.url("/account")).await;
    let exit = browser.started_exit().await;
    assert!(
        exit.success(),
        "the restarted Firefox exited cleanly: {exit:?}"
    );
    support::rig::assert_node(
        &live.snapshot(serde_json::json!({})).await,
        "heading",
        Some(&format!("account login={login}")),
    );
    live.close().await;
    drop(rig);
    factory.shutdown().await;
    browser.stop().await;
}

#[tokio::test]
#[ignore = "requires installed Firefox and the scoped test native host"]
async fn a_hung_command_fails_alone_and_keeps_the_session() {
    let env = Env::load();
    let profile = new_profile(&env);
    let (browser, bidi_url) = Browser::launch(&env, profile.path()).await;
    let firefox = profile_owner_pids(profile.path());
    let client =
        firefox_companion::BidiClient::connect_session(bidi_url.clone(), Duration::from_secs(2))
            .await
            .expect("open a BiDi session");
    // A tab of its own: the startup tab may still be loading its first page.
    let created = client
        .send("browsingContext.create", serde_json::json!({"type": "tab"}))
        .await
        .expect("open a tab");
    let context = created["context"]
        .as_str()
        .expect("the new tab's context")
        .to_owned();
    let hung = client
        .send(
            "script.evaluate",
            serde_json::json!({"expression":"new Promise(() => {})","awaitPromise":true,
                               "target":{"context":context}}),
        )
        .await
        .expect_err("a script that never settles misses its deadline");
    assert_eq!(hung.code, types::ErrorCode::DeadlineExceeded, "{hung:?}");
    client
        .send(
            "browsingContext.getTree",
            serde_json::json!({"maxDepth": 0}),
        )
        .await
        .expect("the connection still serves commands");
    client
        .end_session()
        .await
        .expect("the session ends cleanly");
    let next = firefox_companion::BidiClient::connect_session(bidi_url, TIMEOUT)
        .await
        .expect("a new session starts on the same Firefox");
    next.end_session().await.expect("end the new session");
    assert_eq!(
        profile_owner_pids(profile.path()),
        firefox,
        "Firefox kept running"
    );
    browser.stop().await;
}
