#[cfg(unix)]
#[test]
fn installer_recovers_after_a_waiting_process_is_killed_and_reuses_the_advisory_lock() {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::process::Command;
    use std::time::Duration;

    let root = tempfile::tempdir().unwrap();
    let wrapper = root.path().join("firefox-native-host");
    let manifest = root.path().join("com.bobby_browser.companion.json");
    let descriptor = root.path().join("dynamic-descriptor.json");
    let lock = manifest.with_extension("install.lock");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock)
        .unwrap();
    lock_file.lock().unwrap();

    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bobby"));
        command.args([
            "install-firefox-native-host",
            "--wrapper",
            wrapper.to_str().unwrap(),
            "--manifest",
            manifest.to_str().unwrap(),
            "--cli",
            "/bin/echo",
            "--descriptor",
            descriptor.to_str().unwrap(),
        ]);
        command
    };
    let mut blocked = command().spawn().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(blocked.try_wait().unwrap().is_none());
    blocked.kill().unwrap();
    blocked.wait().unwrap();
    lock_file.unlock().unwrap();

    assert!(command().status().unwrap().success());
    assert!(command().status().unwrap().success());
    assert_eq!(
        std::fs::metadata(&wrapper).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&manifest).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(lock.exists());
}

#[cfg(unix)]
#[test]
fn installer_never_follows_an_operator_owned_lock_symlink() {
    let root = tempfile::tempdir().unwrap();
    let wrapper = root.path().join("firefox-native-host");
    let manifest = root.path().join("com.bobby_browser.companion.json");
    let descriptor = root.path().join("dynamic-descriptor.json");
    let lock = manifest.with_extension("install.lock");
    let foreign = root.path().join("operator-owned");
    std::fs::write(&foreign, b"must-not-open-or-change").unwrap();
    std::os::unix::fs::symlink(&foreign, &lock).unwrap();

    let status = std::process::Command::new(env!("CARGO_BIN_EXE_bobby"))
        .args([
            "install-firefox-native-host",
            "--wrapper",
            wrapper.to_str().unwrap(),
            "--manifest",
            manifest.to_str().unwrap(),
            "--cli",
            "/bin/echo",
            "--descriptor",
            descriptor.to_str().unwrap(),
        ])
        .status()
        .unwrap();

    assert!(!status.success());
    assert_eq!(std::fs::read(&foreign).unwrap(), b"must-not-open-or-change");
    assert!(std::fs::symlink_metadata(&lock)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!wrapper.exists());
    assert!(!manifest.exists());
}
