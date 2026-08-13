use fingerprinting::FingerprintConfig;
use types::{Evidence, PageId, SessionId};
use worker_pool::{ChromiumWorkerFactory, WorkerFactory};

fn chrome_config(root: &std::path::Path) -> config::BrowserConfig {
    config::BrowserConfig {
        executable: Some(std::path::PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        )),
        profiles_dir: root.join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![],
        downloads_dir: root.join("downloads"),
        artifacts_dir: root.join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    }
}

/// Evaluates every CreepJS likeHeadless flag in our environment and prints
/// exactly which ones trip, so the floor is measured, not guessed.
#[tokio::test]
#[ignore = "requires Chrome; no network needed"]
async fn print_like_headless_flag_state() {
    let root = tempfile::tempdir().unwrap();
    let fingerprint = FingerprintConfig::default()
        .with_enabled(true)
        .with_session_seed(777)
        .with_inject_chrome(true);
    let factory =
        ChromiumWorkerFactory::new(chrome_config(root.path())).with_fingerprint(fingerprint);
    let worker = factory.launch(&SessionId::new()).await.unwrap();

    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();
    worker
        .navigate(
            &page,
            &types::NavigateCommand {
                url: "data:text/html,<title>probe</title>".into(),
                wait_until: types::WaitUntil::Commit,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    let outcome = worker
        .evaluate_javascript(
            &page,
            &types::EvaluateJavaScriptCommand {
                expression: r#"(async () => {
  const notifPerm = ('Notification' in window) ? Notification.permission : 'n/a';
  let permQueryState = 'n/a';
  try { permQueryState = (await navigator.permissions.query({ name: 'notifications' })).state; } catch (_) {}
  const div = document.createElement('div');
  document.body.appendChild(div);
  div.setAttribute('style', 'background-color: ActiveText');
  const activeText = getComputedStyle(div).backgroundColor;
  div.remove();
  let uaDataPlatform = 'no-uaData';
  try { uaDataPlatform = (await navigator.userAgentData.getHighEntropyValues(['platform'])).platform; } catch (_) {}
  return {
    noChrome: !('chrome' in window),
    hasPermissionsBug: permQueryState === 'prompt' && notifPerm === 'denied',
    noPlugins: navigator.plugins.length === 0,
    noMimeTypes: Object.keys({ ...navigator.mimeTypes }).length === 0,
    notificationIsDenied: notifPerm === 'denied',
    hasKnownBgColor: activeText === 'rgb(255, 0, 0)',
    prefersLightColor: matchMedia('(prefers-color-scheme: light)').matches,
    uaDataIsBlank: (navigator.userAgentData?.platform === '') || uaDataPlatform === '',
    pdfIsDisabled: ('pdfViewerEnabled' in navigator && navigator.pdfViewerEnabled === false),
    noTaskbar: (screen.height === screen.availHeight && screen.width === screen.availWidth),
    hasVvpScreenRes: (innerWidth === screen.width && outerHeight === screen.height),
    noWebShare: !('share' in navigator) || !('canShare' in navigator),
    noContentIndex: !('ContentIndex' in window),
    noContactsManager: !('ContactsManager' in window),
    noDownlinkMax: !('downlinkMax' in (navigator.connection || {})),
  };
})()"#
                .into(),
                await_promise: true,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    let flags = outcome
        .iter()
        .find_map(|item| match item {
            Evidence::JavaScriptResult { value, .. } => Some(value.clone()),
            _ => None,
        })
        .expect("javascript flags evidence");
    eprintln!("FLAGS: {}", serde_json::to_string_pretty(&flags).unwrap());
    let true_flags: Vec<_> = flags
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, v)| v.as_bool() == Some(true))
        .map(|(k, _)| k.clone())
        .collect();
    eprintln!("TRUE FLAGS ({}/15): {:?}", true_flags.len(), true_flags);
}
