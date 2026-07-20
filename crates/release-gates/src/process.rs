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
            kill_and_reap(&mut child, &mut process_group).await?;
            Err(ProcessFailure::Timeout)
        }
        Ok(Err(InternalFailure::OutputLimit)) => {
            kill_and_reap(&mut child, &mut process_group).await?;
            Err(ProcessFailure::OutputLimit {
                limit: spec.max_output_bytes,
            })
        }
        Ok(Err(InternalFailure::Wait { source })) => {
            kill_and_reap(&mut child, &mut process_group).await?;
            Err(ProcessFailure::Wait { source })
        }
        Ok(Err(InternalFailure::Read { stream, source })) => {
            kill_and_reap(&mut child, &mut process_group).await?;
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
async fn kill_and_reap(
    child: &mut tokio::process::Child,
    process_group: &mut ProcessGroupGuard,
) -> Result<(), ProcessFailure> {
    let initial_group_result = process_group.kill();
    if initial_group_result.is_err() {
        let _ = child.start_kill();
    }
    child
        .wait()
        .await
        .map_err(|source| ProcessFailure::Cleanup { source })?;

    // Darwin may report EPERM when the only remaining group member is an
    // exited child awaiting reap. Retry after reaping: ESRCH then proves the
    // group is gone, while a live group still must accept SIGKILL.
    if initial_group_result.is_err() {
        process_group
            .kill()
            .map_err(|source| ProcessFailure::Cleanup { source })?;
    }
    Ok(())
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
