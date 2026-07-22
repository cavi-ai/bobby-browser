use companion_protocol::{BrowserEngine, InteractionPath};
use release_gates::evaluate_native_browser_proof;
use runtime_tests::{run_installed_firefox_workflow, InstalledFirefoxConfig};

#[test]
fn installed_firefox_config_names_the_first_missing_variable() {
    let saved = [
        ("BOBBY_FIREFOX_BIN", std::env::var_os("BOBBY_FIREFOX_BIN")),
        (
            "BOBBY_FIREFOX_PROFILE",
            std::env::var_os("BOBBY_FIREFOX_PROFILE"),
        ),
        (
            "BOBBY_COMPANION_EXTENSION",
            std::env::var_os("BOBBY_COMPANION_EXTENSION"),
        ),
    ];
    for (name, _) in &saved {
        std::env::remove_var(name);
    }
    assert_eq!(
        InstalledFirefoxConfig::from_env().unwrap_err(),
        "BOBBY_FIREFOX_BIN"
    );
    std::env::set_var("BOBBY_FIREFOX_BIN", "/missing/firefox");
    assert_eq!(
        InstalledFirefoxConfig::from_env().unwrap_err(),
        "BOBBY_FIREFOX_PROFILE"
    );
    std::env::set_var("BOBBY_FIREFOX_PROFILE", "/missing/profile");
    assert_eq!(
        InstalledFirefoxConfig::from_env().unwrap_err(),
        "BOBBY_COMPANION_EXTENSION"
    );
    for (name, value) in saved {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
}

#[tokio::test]
#[ignore = "requires an installed headed Firefox and paired test profile"]
async fn installed_firefox_completes_verified_native_input_workflow() {
    let config = InstalledFirefoxConfig::from_env().expect("installed Firefox test configuration");
    let proof = run_installed_firefox_workflow(config)
        .await
        .expect("installed Firefox workflow");
    evaluate_native_browser_proof(&proof).expect("native Firefox release proof");
    assert_eq!(
        proof.browser.as_ref().unwrap().engine,
        BrowserEngine::Firefox
    );
    assert!(proof.operations.iter().any(|operation| {
        operation.name == "typeText"
            && operation.interaction_path == InteractionPath::EngineNative
            && operation.postcondition_verified
    }));
    assert!(proof.operations.iter().any(|operation| {
        operation.name == "click"
            && operation.interaction_path == InteractionPath::EngineNative
            && operation.postcondition_verified
    }));
    assert_eq!(proof.confirmation_text, "Submitted");
    assert!(proof.redaction_findings.is_empty());
}
