//! `bobby engine` shows and sets the default browser engine in a scoped
//! selection file. Nothing reads or writes the real user scope.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::{json, Value};

const PROFILE_ID: &str = "5d8a3c1e-2f4b-4c6d-8e9f-0a1b2c3d4e5f";

fn bobby(scope: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bobby"))
        .env("BOBBY_BROWSER_SCOPE_DIR", scope)
        .env_remove("AUTOMATION_RUNTIME_BROWSER_SELECTION")
        .args(args)
        .output()
        .expect("run bobby")
}

fn stdout(output: &Output) -> String {
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn selection_path(scope: &Path) -> std::path::PathBuf {
    scope.join("browser-selection.json")
}

fn read_selection(scope: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(selection_path(scope)).unwrap()).unwrap()
}

fn enroll_firefox(scope: &Path) {
    let selection = json!({
        "preference":{"mode":"exact","engine":"firefox","profileId":PROFILE_ID},
        "firefox":[{
            "profileId":PROFILE_ID,
            "bidiUrl":"ws://127.0.0.1:9/session",
            "profileDir":scope.join("firefox"),
            "companionBind":"127.0.0.1:0",
            "descriptorPath":scope.join("descriptor.json"),
        }]
    });
    std::fs::write(selection_path(scope), selection.to_string()).unwrap();
}

#[test]
fn setting_the_engine_keeps_enrolled_firefox_profiles() {
    let scope = tempfile::tempdir().unwrap();
    enroll_firefox(scope.path());

    let set = stdout(&bobby(scope.path(), &["engine", "chrome"]));
    assert!(set.contains("default engine: chromium"), "{set}");
    assert!(set.contains("bobby runtime restart"), "{set}");
    let selection = read_selection(scope.path());
    assert_eq!(selection["preference"], json!({"mode":"managedChromium"}));
    assert_eq!(selection["firefox"][0]["profileId"], PROFILE_ID);

    let shown = stdout(&bobby(scope.path(), &["engine"]));
    assert!(shown.contains("default engine: chromium"), "{shown}");
    assert!(shown.contains("chromium, firefox"), "{shown}");

    stdout(&bobby(scope.path(), &["engine", "firefox"]));
    assert_eq!(
        read_selection(scope.path())["preference"],
        json!({"mode":"exact","engine":"firefox","profileId":PROFILE_ID})
    );
}

#[test]
fn firefox_without_an_enrolled_profile_is_refused() {
    let scope = tempfile::tempdir().unwrap();

    let output = bobby(scope.path(), &["engine", "firefox"]);

    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no Firefox profile is enrolled"),
        "{output:?}"
    );
    assert!(!selection_path(scope.path()).exists());
}

#[test]
fn an_environment_selection_refuses_a_change_it_would_override() {
    let scope = tempfile::tempdir().unwrap();
    enroll_firefox(scope.path());
    let before = read_selection(scope.path());

    let output = Command::new(env!("CARGO_BIN_EXE_bobby"))
        .env("BOBBY_BROWSER_SCOPE_DIR", scope.path())
        .env(
            "AUTOMATION_RUNTIME_BROWSER_SELECTION",
            r#"{"preference":{"mode":"managedChromium"}}"#,
        )
        .args(["engine", "chromium"])
        .output()
        .expect("run bobby");

    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("AUTOMATION_RUNTIME_BROWSER_SELECTION"),
        "{output:?}"
    );
    assert_eq!(read_selection(scope.path()), before);
}
