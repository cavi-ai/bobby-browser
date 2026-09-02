#[test]
fn acp_gateway_version_prints_package_version() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_acp-gateway"))
        .arg("--version")
        .output()
        .expect("spawn acp-gateway");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        env!("CARGO_PKG_VERSION")
    );
    assert!(
        output.stderr.is_empty(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}
