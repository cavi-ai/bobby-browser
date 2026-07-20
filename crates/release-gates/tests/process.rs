#[cfg(unix)]
mod unix {
    use release_gates::{run_process, ProcessFailure, ProcessSpec};
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn process_runner_bounds_time_and_combined_output() {
        let ok = ProcessSpec::new(
            "/bin/sh",
            ["-c", "printf 1234; printf 5678 >&2"],
            Duration::from_secs(1),
            8,
        );
        let outcome = run_process(&ok).await.unwrap();
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stdout, b"1234");
        assert_eq!(outcome.stderr, b"5678");

        let overflow = ProcessSpec::new(
            "/bin/sh",
            ["-c", "printf 1234; printf 5678 >&2"],
            Duration::from_secs(1),
            7,
        );
        let overflow_error = run_process(&overflow).await.unwrap_err();
        assert!(
            matches!(overflow_error, ProcessFailure::OutputLimit { limit: 7 }),
            "unexpected overflow result: {overflow_error:?}"
        );

        let timeout = ProcessSpec::new("/bin/sh", ["-c", "sleep 2"], Duration::from_millis(20), 16);
        assert!(matches!(
            run_process(&timeout).await,
            Err(ProcessFailure::Timeout)
        ));
    }

    #[tokio::test]
    async fn process_runner_drains_stdout_and_stderr_concurrently() {
        let spec = ProcessSpec::new(
            "/bin/sh",
            [
                "-c",
                "(head -c 65536 /dev/zero) & (head -c 65536 /dev/zero >&2) & wait",
            ],
            Duration::from_secs(1),
            131_072,
        );

        let outcome = run_process(&spec).await.unwrap();
        assert_eq!(outcome.stdout.len(), 65_536);
        assert_eq!(outcome.stderr.len(), 65_536);
    }

    #[tokio::test]
    async fn concurrent_stdout_and_stderr_reject_combined_limit_plus_one() {
        let spec = ProcessSpec::new(
            "/bin/sh",
            [
                "-c",
                "(head -c 8192 /dev/zero) & (head -c 8192 /dev/zero >&2) & wait",
            ],
            Duration::from_secs(1),
            16_383,
        );

        assert!(matches!(
            run_process(&spec).await,
            Err(ProcessFailure::OutputLimit { limit: 16_383 })
        ));
    }

    #[tokio::test]
    async fn timeout_kills_the_shell_and_its_background_grandchild() {
        let temp = tempfile::tempdir().unwrap();
        let shell_pid_path = temp.path().join("shell.pid");
        let grandchild_pid_path = temp.path().join("grandchild.pid");
        let args = vec![
            OsString::from("-c"),
            OsString::from(
                "printf '%s' \"$$\" > \"$1\"; sleep 30 & child=$!; printf '%s' \"$child\" > \"$2\"; wait",
            ),
            OsString::from("process-tree-fixture"),
            shell_pid_path.as_os_str().to_owned(),
            grandchild_pid_path.as_os_str().to_owned(),
        ];
        let spec = ProcessSpec::new("/bin/sh", args, Duration::from_millis(500), 1_024);
        let process = tokio::spawn(async move { run_process(&spec).await });
        let (shell_pid, grandchild_pid) =
            wait_for_pid_fixtures(&shell_pid_path, &grandchild_pid_path).await;

        assert!(matches!(
            process.await.unwrap(),
            Err(ProcessFailure::Timeout)
        ));

        assert_process_gone(shell_pid);
        assert_process_gone(grandchild_pid);
    }

    #[tokio::test]
    async fn output_overflow_kills_the_shell_and_its_background_grandchild() {
        let temp = tempfile::tempdir().unwrap();
        let shell_pid_path = temp.path().join("shell.pid");
        let grandchild_pid_path = temp.path().join("grandchild.pid");
        let trigger_path = temp.path().join("trigger");
        let args = vec![
            OsString::from("-c"),
            OsString::from(
                "printf '%s' \"$$\" > \"$1\"; sleep 30 & child=$!; printf '%s' \"$child\" > \"$2\"; while [ ! -f \"$3\" ]; do sleep 0.01; done; printf 0123456789; wait",
            ),
            OsString::from("process-tree-fixture"),
            shell_pid_path.as_os_str().to_owned(),
            grandchild_pid_path.as_os_str().to_owned(),
            trigger_path.as_os_str().to_owned(),
        ];
        let spec = ProcessSpec::new("/bin/sh", args, Duration::from_secs(2), 4);
        let process = tokio::spawn(async move { run_process(&spec).await });
        let (shell_pid, grandchild_pid) =
            wait_for_pid_fixtures(&shell_pid_path, &grandchild_pid_path).await;
        fs::write(&trigger_path, b"go").unwrap();

        assert!(matches!(
            process.await.unwrap(),
            Err(ProcessFailure::OutputLimit { limit: 4 })
        ));

        assert_process_gone(shell_pid);
        assert_process_gone(grandchild_pid);
    }

    #[tokio::test]
    async fn process_runner_rejects_zero_bounds_before_spawn() {
        let zero_timeout = ProcessSpec::new(
            "/definitely/not/a/program",
            std::iter::empty::<&str>(),
            Duration::ZERO,
            1,
        );
        assert!(matches!(
            run_process(&zero_timeout).await,
            Err(ProcessFailure::InvalidTimeout)
        ));

        let zero_output = ProcessSpec::new(
            "/definitely/not/a/program",
            std::iter::empty::<&str>(),
            Duration::from_secs(1),
            0,
        );
        assert!(matches!(
            run_process(&zero_output).await,
            Err(ProcessFailure::InvalidOutputLimit)
        ));
    }

    #[tokio::test]
    async fn process_runner_clears_secrets_and_copies_allowed_environment() {
        const ALLOWED: [&str; 5] = ["PATH", "HOME", "TMPDIR", "RUSTUP_HOME", "CARGO_HOME"];
        const SECRET: &str = "RELEASE_GATES_TEST_SECRET_DO_NOT_INHERIT";
        std::env::set_var(SECRET, "sensitive");
        let _guard = EnvGuard(SECRET);
        let expected = ALLOWED
            .into_iter()
            .filter_map(|name| {
                std::env::var_os(name).map(|value| {
                    (
                        name.as_bytes().to_vec(),
                        value.as_os_str().as_bytes().to_vec(),
                    )
                })
            })
            .collect::<BTreeMap<_, _>>();
        let spec = ProcessSpec::new("/usr/bin/env", ["-0"], Duration::from_secs(1), 16_384);

        let outcome = run_process(&spec).await.unwrap();
        assert_eq!(outcome.exit_code, Some(0));
        let actual = outcome
            .stdout
            .split(|byte| *byte == b'\0')
            .filter(|line| !line.is_empty())
            .map(|line| {
                let separator = line
                    .iter()
                    .position(|byte| *byte == b'=')
                    .expect("environment entry did not contain '='");
                (line[..separator].to_vec(), line[separator + 1..].to_vec())
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(actual, expected);
        assert!(!actual.contains_key(SECRET.as_bytes()));
    }

    #[tokio::test]
    async fn process_runner_uses_the_requested_current_directory() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("marker"), b"present").unwrap();
        let spec = ProcessSpec::new(
            "/bin/sh",
            ["-c", "test -f marker && printf cwd-ok"],
            Duration::from_secs(1),
            16,
        )
        .with_current_dir(temp.path());

        assert_eq!(run_process(&spec).await.unwrap().stdout, b"cwd-ok");
    }

    async fn wait_for_pid_fixtures(shell_path: &Path, grandchild_path: &Path) -> (i32, i32) {
        let readiness = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                if let (Some(shell_pid), Some(grandchild_pid)) =
                    (try_read_pid(shell_path), try_read_pid(grandchild_path))
                {
                    return (shell_pid, grandchild_pid);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;

        readiness.unwrap_or_else(|_| {
            panic!(
                "PID fixtures were not ready within 300ms: shell={} ({}), grandchild={} ({})",
                shell_path.display(),
                pid_fixture_state(shell_path),
                grandchild_path.display(),
                pid_fixture_state(grandchild_path)
            )
        })
    }

    fn try_read_pid(path: &Path) -> Option<i32> {
        fs::read_to_string(path).ok()?.parse().ok()
    }

    fn pid_fixture_state(path: &Path) -> String {
        match fs::read_to_string(path) {
            Ok(contents) => format!(
                "contents={contents:?}, parse_result={:?}",
                contents.parse::<i32>()
            ),
            Err(error) => format!("read_error={error}"),
        }
    }

    fn assert_process_gone(pid: i32) {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let result = unsafe { libc::kill(pid, 0) };
            if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "process {pid} survived process-runner failure"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    struct EnvGuard(&'static str);

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var(self.0);
        }
    }
}

#[cfg(not(unix))]
#[tokio::test]
async fn process_runner_fails_closed_on_unsupported_platforms() {
    use release_gates::{run_process, ProcessFailure, ProcessSpec};
    use std::time::Duration;

    let spec = ProcessSpec::new(
        "program-that-must-not-spawn",
        std::iter::empty::<&str>(),
        Duration::from_secs(1),
        16,
    );
    assert!(matches!(
        run_process(&spec).await,
        Err(ProcessFailure::UnsupportedPlatform)
    ));
}
