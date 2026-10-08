//! Operator context commands use the same configured limits as the runtime.
use std::{io::Write, process::Command};

#[tokio::test]
async fn context_list_honors_configured_limits_without_changing_the_file() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("memory");
    let (store, _) = context_store::ContextStore::open(&root, "profile")
        .await
        .unwrap();
    store
        .upsert_site("site", context_store::SiteContext::default())
        .await;
    assert!(store.flush().await.is_empty());
    let path = store.root().join("73697465.json");
    drop(store);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(&vec![b' '; 2 * 1024 * 1024]).unwrap();
    drop(file);
    let original = std::fs::read(&path).unwrap();
    let config = temp.path().join("config.toml");
    std::fs::write(
        &config,
        "[context]\ndir = 'memory'\n[context.limits]\nmax_file_bytes = 3145728\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_bobby"))
        .current_dir(temp.path())
        .arg("context")
        .arg("--config")
        .arg(config)
        .args(["list", "--profile", "profile"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "site");
    assert!(!String::from_utf8(output.stderr)
        .unwrap()
        .contains("skipped unreadable"));
    assert_eq!(std::fs::read(path).unwrap(), original);
}
