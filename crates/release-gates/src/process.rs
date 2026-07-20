use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;

#[derive(Clone, Debug)]
pub struct ProcessSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub current_dir: Option<PathBuf>,
}

impl ProcessSpec {
    pub fn new<P, I, A>(program: P, args: I, timeout: Duration, max_output_bytes: usize) -> Self
    where
        P: Into<OsString>,
        I: IntoIterator<Item = A>,
        A: Into<OsString>,
    {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            timeout,
            max_output_bytes,
            current_dir: None,
        }
    }

    pub fn with_current_dir<P>(mut self, current_dir: P) -> Self
    where
        P: AsRef<Path>,
    {
        self.current_dir = Some(current_dir.as_ref().to_owned());
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOutcome {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum ProcessFailure {
    #[error("process timeout must be greater than zero")]
    InvalidTimeout,
    #[error("process output limit must be greater than zero")]
    InvalidOutputLimit,
    #[error("secure process-tree containment is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("failed to spawn process")]
    Spawn {
        #[source]
        source: std::io::Error,
    },
    #[error("spawned process did not expose a process identifier")]
    MissingProcessId,
    #[error("spawned process did not expose piped {stream}")]
    MissingPipe { stream: &'static str },
    #[error("failed to wait for process")]
    Wait {
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read process {stream}")]
    Read {
        stream: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to terminate or reap process tree")]
    Cleanup {
        #[source]
        source: std::io::Error,
    },
    #[error("process timed out")]
    Timeout,
    #[error("process exceeded the combined output limit of {limit} bytes")]
    OutputLimit { limit: usize },
}

#[cfg(not(unix))]
pub async fn run_process(_spec: &ProcessSpec) -> Result<ProcessOutcome, ProcessFailure> {
    Err(ProcessFailure::UnsupportedPlatform)
}

#[cfg(unix)]
pub async fn run_process(spec: &ProcessSpec) -> Result<ProcessOutcome, ProcessFailure> {
    use std::process::Stdio;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    use tokio::process::Command;

    if spec.timeout.is_zero() {
        return Err(ProcessFailure::InvalidTimeout);
    }
    if spec.max_output_bytes == 0 {
        return Err(ProcessFailure::InvalidOutputLimit);
    }

    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env_clear()
        .process_group(0);

    for name in ["PATH", "HOME", "TMPDIR", "RUSTUP_HOME", "CARGO_HOME"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    if let Some(current_dir) = &spec.current_dir {
        command.current_dir(current_dir);
    }

    let mut child = command
        .spawn()
        .map_err(|source| ProcessFailure::Spawn { source })?;
    let process_id = child.id().ok_or(ProcessFailure::MissingProcessId)?;
    let process_group_id =
        i32::try_from(process_id).map_err(|_| ProcessFailure::MissingProcessId)?;
    let mut process_group = ProcessGroupGuard::new(process_group_id);
    let stdout = child
        .stdout
        .take()
        .ok_or(ProcessFailure::MissingPipe { stream: "stdout" })?;
    let stderr = child
        .stderr
        .take()
        .ok_or(ProcessFailure::MissingPipe { stream: "stderr" })?;
    let budget = Arc::new(AtomicUsize::new(0));

    let completion = async {
        let status = async {
            child
                .wait()
                .await
                .map_err(|source| InternalFailure::Wait { source })
        };
        let stdout = read_stream(stdout, "stdout", Arc::clone(&budget), spec.max_output_bytes);
        let stderr = read_stream(stderr, "stderr", Arc::clone(&budget), spec.max_output_bytes);
        // Poll the readers before wait so an output breach is observed and
        // the process group is signaled before the direct child is reaped.
        tokio::try_join!(stdout, stderr, status)
    };

    let completed = tokio::time::timeout(spec.timeout, completion).await;
    match completed {
        Err(_) => {
            cleanup_process_tree(&mut child, &mut process_group, CLEANUP_TIMEOUT).await?;
            Err(ProcessFailure::Timeout)
        }
        Ok(Err(InternalFailure::OutputLimit)) => {
            cleanup_process_tree(&mut child, &mut process_group, CLEANUP_TIMEOUT).await?;
            Err(ProcessFailure::OutputLimit {
                limit: spec.max_output_bytes,
            })
        }
        Ok(Err(InternalFailure::Wait { source })) => {
            cleanup_process_tree(&mut child, &mut process_group, CLEANUP_TIMEOUT).await?;
            Err(ProcessFailure::Wait { source })
        }
        Ok(Err(InternalFailure::Read { stream, source })) => {
            cleanup_process_tree(&mut child, &mut process_group, CLEANUP_TIMEOUT).await?;
            Err(ProcessFailure::Read { stream, source })
        }
        Ok(Ok((stdout, stderr, status))) => {
            process_group
                .kill()
                .map_err(|source| ProcessFailure::Cleanup { source })?;
            Ok(ProcessOutcome {
                exit_code: status.code(),
                stdout,
                stderr,
            })
        }
    }
}

#[cfg(unix)]
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

#[cfg(unix)]
enum InternalFailure {
    Wait {
        source: std::io::Error,
    },
    Read {
        stream: &'static str,
        source: std::io::Error,
    },
    OutputLimit,
}

#[cfg(unix)]
async fn read_stream<R>(
    mut stream: R,
    stream_name: &'static str,
    budget: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    limit: usize,
) -> Result<Vec<u8>, InternalFailure>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncReadExt;

    let mut output = Vec::with_capacity(limit.min(8 * 1024));
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|source| InternalFailure::Read {
                stream: stream_name,
                source,
            })?;
        if read == 0 {
            return Ok(output);
        }

        let reserved = budget.fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(read).filter(|total| *total <= limit)
        });
        if reserved.is_err() {
            return Err(InternalFailure::OutputLimit);
        }
        output.extend_from_slice(&chunk[..read]);
    }
}

#[cfg(unix)]
async fn cleanup_process_tree<C, G>(
    child: &mut C,
    process_group: &mut G,
    cleanup_timeout: Duration,
) -> Result<(), ProcessFailure>
where
    C: CleanupChild,
    G: CleanupGroup,
{
    let initial_group_error = process_group.kill().err();
    let direct_kill_error = if initial_group_error.is_some() {
        child.start_kill().err()
    } else {
        None
    };

    let reap = tokio::time::timeout(cleanup_timeout, child.wait()).await;
    match reap {
        Err(_) => {
            return Err(cleanup_failure(
                std::io::ErrorKind::TimedOut,
                format!(
                    "direct child reap timed out after {cleanup_timeout:?}; {}",
                    cleanup_attempts(&initial_group_error, &direct_kill_error)
                ),
            ));
        }
        Ok(Err(wait_error)) => {
            return Err(cleanup_failure(
                wait_error.kind(),
                format!(
                    "direct child reap failed: {wait_error}; {}",
                    cleanup_attempts(&initial_group_error, &direct_kill_error)
                ),
            ));
        }
        Ok(Ok(())) => {}
    }

    // Darwin may report EPERM when the only remaining group member is an
    // exited child awaiting reap. Retry after reaping: ESRCH then proves the
    // group is gone, while a live group still must accept SIGKILL.
    if let Some(initial_group_error) = initial_group_error {
        if let Err(retry_error) = process_group.kill() {
            return Err(cleanup_failure(
                retry_error.kind(),
                format!(
                    "process-group signal failed before reap: {initial_group_error}; \
                     process-group signal retry after reap failed: {retry_error}; {}",
                    direct_kill_attempt(&direct_kill_error)
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn cleanup_attempts(
    group_error: &Option<std::io::Error>,
    direct_kill_error: &Option<std::io::Error>,
) -> String {
    let group = match group_error {
        Some(error) => format!("process-group signal failed: {error}"),
        None => "process-group signal succeeded".to_owned(),
    };
    let direct = match group_error {
        Some(_) => direct_kill_attempt(direct_kill_error),
        None => "direct child kill not required".to_owned(),
    };
    format!("{group}; {direct}")
}

#[cfg(unix)]
fn direct_kill_attempt(error: &Option<std::io::Error>) -> String {
    match error {
        Some(error) => format!("direct child kill failed: {error}"),
        None => "direct child kill succeeded".to_owned(),
    }
}

#[cfg(unix)]
fn cleanup_failure(kind: std::io::ErrorKind, message: String) -> ProcessFailure {
    ProcessFailure::Cleanup {
        source: std::io::Error::new(kind, message),
    }
}

#[cfg(unix)]
trait CleanupChild {
    fn start_kill(&mut self) -> std::io::Result<()>;
    fn wait(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + '_>>;
}

#[cfg(unix)]
impl CleanupChild for tokio::process::Child {
    fn start_kill(&mut self) -> std::io::Result<()> {
        tokio::process::Child::start_kill(self)
    }

    fn wait(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + '_>> {
        Box::pin(async move { tokio::process::Child::wait(self).await.map(|_| ()) })
    }
}

#[cfg(unix)]
trait CleanupGroup {
    fn kill(&mut self) -> std::io::Result<()>;
}

#[cfg(unix)]
struct ProcessGroupGuard {
    process_group_id: i32,
    armed: bool,
}

#[cfg(unix)]
impl ProcessGroupGuard {
    fn new(process_group_id: i32) -> Self {
        Self {
            process_group_id,
            armed: true,
        }
    }

    fn kill(&mut self) -> std::io::Result<()> {
        let result = signal_process_group(self.process_group_id);
        if result.is_ok() {
            self.armed = false;
        }
        result
    }
}

#[cfg(unix)]
impl CleanupGroup for ProcessGroupGuard {
    fn kill(&mut self) -> std::io::Result<()> {
        ProcessGroupGuard::kill(self)
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = signal_process_group(self.process_group_id);
        }
    }
}

#[cfg(unix)]
fn signal_process_group(process_group_id: i32) -> std::io::Result<()> {
    const SIGKILL: i32 = 9;
    const ESRCH: i32 = 3;

    extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }

    let result = unsafe { kill(-process_group_id, SIGKILL) };
    if result == 0 {
        return Ok(());
    }

    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{cleanup_process_tree, CleanupChild, CleanupGroup, ProcessFailure};
    use std::collections::VecDeque;
    use std::future::{pending, ready, Future};
    use std::io::{self, ErrorKind};
    use std::pin::Pin;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn cleanup_reap_is_explicitly_bounded() {
        let mut child = FakeChild::pending();
        let mut group = FakeGroup::success();
        let started = Instant::now();

        let failure = cleanup_process_tree(&mut child, &mut group, Duration::from_millis(10))
            .await
            .unwrap_err();

        assert!(started.elapsed() < Duration::from_millis(200));
        match failure {
            ProcessFailure::Cleanup { source } => {
                assert_eq!(source.kind(), ErrorKind::TimedOut);
                assert!(source.to_string().contains("reap timed out"));
            }
            other => panic!("unexpected cleanup failure: {other:?}"),
        }
    }

    #[tokio::test]
    async fn cleanup_reports_group_signal_and_direct_kill_failures() {
        let mut child = FakeChild::pending_with_kill_error(ErrorKind::BrokenPipe);
        let mut group =
            FakeGroup::errors([ErrorKind::PermissionDenied, ErrorKind::PermissionDenied]);

        let failure = cleanup_process_tree(&mut child, &mut group, Duration::from_millis(10))
            .await
            .unwrap_err();

        match failure {
            ProcessFailure::Cleanup { source } => {
                let message = source.to_string();
                assert_eq!(source.kind(), ErrorKind::TimedOut);
                assert!(message.contains("process-group signal failed"));
                assert!(message.contains("direct child kill failed"));
                assert!(message.contains("reap timed out"));
            }
            other => panic!("unexpected cleanup failure: {other:?}"),
        }
    }

    #[tokio::test]
    async fn cleanup_retries_group_signal_after_reaping_an_exited_child() {
        let mut child = FakeChild::ready_with_kill_error(ErrorKind::InvalidInput);
        let mut group = FakeGroup::error_then_success(ErrorKind::PermissionDenied);

        cleanup_process_tree(&mut child, &mut group, Duration::from_millis(10))
            .await
            .unwrap();

        assert_eq!(group.calls, 2);
    }

    #[tokio::test]
    async fn cleanup_reports_a_failed_group_signal_retry_after_reap() {
        let mut child = FakeChild::ready();
        let mut group =
            FakeGroup::errors([ErrorKind::PermissionDenied, ErrorKind::PermissionDenied]);

        let failure = cleanup_process_tree(&mut child, &mut group, Duration::from_millis(10))
            .await
            .unwrap_err();

        match failure {
            ProcessFailure::Cleanup { source } => {
                let message = source.to_string();
                assert_eq!(source.kind(), ErrorKind::PermissionDenied);
                assert!(message.contains("signal failed before reap"));
                assert!(message.contains("signal retry after reap failed"));
            }
            other => panic!("unexpected cleanup failure: {other:?}"),
        }
    }

    struct FakeChild {
        pending_wait: bool,
        kill_error: Option<ErrorKind>,
    }

    impl FakeChild {
        fn ready() -> Self {
            Self {
                pending_wait: false,
                kill_error: None,
            }
        }

        fn pending() -> Self {
            Self {
                pending_wait: true,
                kill_error: None,
            }
        }

        fn pending_with_kill_error(kind: ErrorKind) -> Self {
            Self {
                pending_wait: true,
                kill_error: Some(kind),
            }
        }

        fn ready_with_kill_error(kind: ErrorKind) -> Self {
            Self {
                pending_wait: false,
                kill_error: Some(kind),
            }
        }
    }

    impl CleanupChild for FakeChild {
        fn start_kill(&mut self) -> io::Result<()> {
            match self.kill_error {
                Some(kind) => Err(io::Error::from(kind)),
                None => Ok(()),
            }
        }

        fn wait(&mut self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            if self.pending_wait {
                Box::pin(pending())
            } else {
                Box::pin(ready(Ok(())))
            }
        }
    }

    struct FakeGroup {
        results: VecDeque<Option<ErrorKind>>,
        calls: usize,
    }

    impl FakeGroup {
        fn success() -> Self {
            Self {
                results: VecDeque::from([None]),
                calls: 0,
            }
        }

        fn errors<const N: usize>(errors: [ErrorKind; N]) -> Self {
            Self {
                results: errors.into_iter().map(Some).collect(),
                calls: 0,
            }
        }

        fn error_then_success(error: ErrorKind) -> Self {
            Self {
                results: VecDeque::from([Some(error), None]),
                calls: 0,
            }
        }
    }

    impl CleanupGroup for FakeGroup {
        fn kill(&mut self) -> io::Result<()> {
            self.calls += 1;
            match self.results.pop_front().flatten() {
                Some(kind) => Err(io::Error::from(kind)),
                None => Ok(()),
            }
        }
    }
}
