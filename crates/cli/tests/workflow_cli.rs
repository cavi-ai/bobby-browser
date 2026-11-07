//! `bobby workflow validate|schema` against workflow files on disk.

use std::path::Path;
use std::process::{Command, Output};

fn bobby(args: &[&str], root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bobby"))
        .args(args)
        .env("BOBBY_BROWSER_SCOPE_DIR", root.join("scope"))
        .env_remove("BOBBY_BROWSER_CONFIG")
        .output()
        .unwrap()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn validate_checks_every_workflow_file_and_schema_prints_the_schema() {
    let root = tempfile::tempdir().unwrap();
    let valid = root.path().join("scope/workflows/contact.json");
    write(
        &valid,
        r#"{"name":"contact","inputs":{"url":{"type":"string"}},
            "steps":[{"id":"open","tool":"navigate","args":{"url":{"$input":"url"}}}]}"#,
    );
    let broken = root.path().join("extra/probe.json");
    write(
        &broken,
        r#"{"name":"probe","steps":[{"id":"x","tool":"no_such_tool"}]}"#,
    );
    let config = root.path().join("config.toml");
    write(&config, "[mcp]\nworkflow_dirs = [\"extra\"]\n");

    let all = bobby(
        &["workflow", "validate", "--config", config.to_str().unwrap()],
        root.path(),
    );
    let stdout = String::from_utf8_lossy(&all.stdout);
    assert!(!all.status.success(), "{stdout}");
    assert!(
        stdout.contains(&format!("ok {}", valid.display())),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "invalid {}: unknownTool at /steps/0/tool",
            broken.display()
        )),
        "{stdout}"
    );

    let one = bobby(
        &["workflow", "validate", valid.to_str().unwrap()],
        root.path(),
    );
    assert!(
        one.status.success(),
        "{}",
        String::from_utf8_lossy(&one.stderr)
    );

    let schema = bobby(&["workflow", "schema"], root.path());
    assert!(schema.status.success());
    let schema: serde_json::Value = serde_json::from_slice(&schema.stdout).unwrap();
    assert_eq!(schema["required"], serde_json::json!(["name", "steps"]));
}
