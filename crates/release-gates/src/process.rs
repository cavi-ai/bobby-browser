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
    #[error("failed to terminate or reap process tree: {source}")]
    Cleanup {
        #[source]
        source: std::io::Error,
    },
    #[error(
        "process completed while residual descendants remained in process group {process_group_id}"
    )]
    ResidualProcessGroup { process_group_id: i32 },
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
        let exited = async {
            wait_for_child_exit_without_reaping(process_group_id)
                .await
                .map_err(|source| InternalFailure::Wait { source })
        };
        let stdout = read_stream(stdout, "stdout", Arc::clone(&budget), spec.max_output_bytes);
        let stderr = read_stream(stderr, "stderr", Arc::clone(&budget), spec.max_output_bytes);
        // Poll readers before observing exit so an output breach wins without
        // ever reaping the process-group leader. The leader remains waitable
        // through successful-path residual membership checks.
        tokio::try_join!(stdout, stderr, exited)
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
        Ok(Ok((stdout, stderr, ()))) => {
            let status =
                finalize_completed_process(&mut child, &mut process_group, CLEANUP_TIMEOUT).await?;
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
const PROCESS_GROUP_POLL_INTERVAL: Duration = Duration::from_millis(10);

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

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn wait_for_child_exit_without_reaping(process_id: i32) -> std::io::Result<()> {
    use rustix::process::{waitid, Pid, WaitId, WaitIdOptions};

    let process_id = Pid::from_raw(process_id).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid child process ID")
    })?;
    loop {
        let observed = waitid(
            WaitId::Pid(process_id),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
        )
        .map(|status| status.is_some())
        .map_err(std::io::Error::from)?;
        if observed {
            return Ok(());
        }
        tokio::time::sleep(PROCESS_GROUP_POLL_INTERVAL).await;
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
async fn wait_for_child_exit_without_reaping(_process_id: i32) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "secure unreaped process-group inspection is unsupported on this Unix platform",
    ))
}

#[cfg(target_os = "macos")]
fn live_process_group_members(
    process_group_id: i32,
    leader_process_id: i32,
) -> std::io::Result<Vec<i32>> {
    use std::mem::{size_of, MaybeUninit};

    let mut capacity = 16_usize;
    let process_ids = loop {
        let mut process_ids = vec![0 as libc::pid_t; capacity];
        let buffer_size = process_ids
            .len()
            .checked_mul(size_of::<libc::pid_t>())
            .and_then(|size| i32::try_from(size).ok())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "process-group member buffer exceeded platform limits",
                )
            })?;
        let returned = unsafe {
            libc::proc_listpgrppids(
                process_group_id,
                process_ids.as_mut_ptr().cast(),
                buffer_size,
            )
        };
        if returned < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let returned = usize::try_from(returned).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "negative process-group member buffer length",
            )
        })?;
        if returned > capacity {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "process-group member listing exceeded its PID capacity",
            ));
        }
        let count = returned;
        if count == capacity {
            capacity = capacity.checked_mul(2).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "process-group member count overflowed",
                )
            })?;
            continue;
        }
        process_ids.truncate(count);
        break process_ids;
    };

    let mut live_members = Vec::new();
    for process_id in process_ids {
        if process_id <= 0 || process_id == leader_process_id {
            continue;
        }
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let info_size = i32::try_from(size_of::<libc::proc_bsdinfo>()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "process information structure exceeded platform limits",
            )
        })?;
        let returned = unsafe {
            libc::proc_pidinfo(
                process_id,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                info_size,
            )
        };
        if returned == 0 {
            let info_error = std::io::Error::last_os_error();
            if info_error.raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            let probe = unsafe { libc::kill(process_id, 0) };
            if probe == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            return Err(std::io::Error::new(
                info_error.kind(),
                format!("could not inspect process-group member {process_id}: {info_error}"),
            ));
        }
        if returned != info_size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "short process information for process-group member {process_id}: \
                     {returned} of {info_size} bytes"
                ),
            ));
        }
        let info = unsafe { info.assume_init() };
        if info.pbi_pgid == process_group_id as u32 && info.pbi_status != libc::SZOMB {
            live_members.push(process_id);
        }
    }
    live_members.sort_unstable();
    live_members.dedup();
    Ok(live_members)
}

#[cfg(target_os = "linux")]
fn live_process_group_members(
    process_group_id: i32,
    leader_process_id: i32,
) -> std::io::Result<Vec<i32>> {
    let mut live_members = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(process_id) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if process_id == leader_process_id {
            continue;
        }
        let stat = match std::fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(source),
        };
        let fields = stat
            .rsplit_once(") ")
            .map(|(_, fields)| fields)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("malformed /proc stat for process {process_id}"),
                )
            })?;
        let mut fields = fields.split_whitespace();
        let state = fields.next().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("missing state in /proc stat for process {process_id}"),
            )
        })?;
        let _parent_process_id = fields.next();
        let member_process_group_id = fields
            .next()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("missing process group in /proc stat for process {process_id}"),
                )
            })?
            .parse::<i32>()
            .map_err(|source| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "invalid process group in /proc stat for process {process_id}: {source}"
                    ),
                )
            })?;
        if member_process_group_id == process_group_id && !matches!(state, "Z" | "X" | "x") {
            live_members.push(process_id);
        }
    }
    live_members.sort_unstable();
    live_members.dedup();
    Ok(live_members)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn live_process_group_members(
    _process_group_id: i32,
    _leader_process_id: i32,
) -> std::io::Result<Vec<i32>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "secure process-group membership inspection is unsupported on this Unix platform",
    ))
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
    // A retry is safe only while the direct child still anchors the PGID.
    // Never signal this numeric PGID again after the child has been reaped.
    let group_retry_error = if initial_group_error.is_some() {
        process_group.kill().err()
    } else {
        None
    };
    let failed_group_signal_resolution = if initial_group_error.is_some()
        && group_retry_error.is_some()
        && direct_kill_error.is_none()
    {
        Some(
            process_group
                .confirm_no_live_members_after_leader_exit(cleanup_timeout)
                .await,
        )
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
                    cleanup_attempts(&initial_group_error, &group_retry_error, &direct_kill_error)
                ),
            ));
        }
        Ok(Err(wait_error)) => {
            process_group.disarm();
            return Err(cleanup_failure(
                wait_error.kind(),
                format!(
                    "direct child reap failed: {wait_error}; {}",
                    cleanup_attempts(&initial_group_error, &group_retry_error, &direct_kill_error)
                ),
            ));
        }
        Ok(Ok(())) => {}
    }
    process_group.disarm();

    if let (Some(initial_group_error), Some(retry_error)) = (initial_group_error, group_retry_error)
    {
        let resolution = match failed_group_signal_resolution {
            Some(Ok(true)) => return Ok(()),
            Some(Ok(false)) => {
                "same-PGID live members remained while the leader was unreaped".to_owned()
            }
            Some(Err(source)) => format!(
                "could not confirm same-PGID member absence while the leader was unreaped: {source}"
            ),
            None => "same-PGID member absence was not established".to_owned(),
        };
        return Err(cleanup_failure(
            retry_error.kind(),
            format!(
                "process-group signal failed before reap: {initial_group_error}; \
                 process-group signal retry before reap failed: {retry_error}; {}; \
                 {resolution}; no signal sent after reap",
                direct_kill_attempt(&direct_kill_error),
            ),
        ));
    }
    Ok(())
}

#[cfg(unix)]
async fn finalize_completed_process(
    child: &mut tokio::process::Child,
    process_group: &mut ProcessGroupGuard,
    cleanup_timeout: Duration,
) -> Result<std::process::ExitStatus, ProcessFailure> {
    let process_group_id = process_group.process_group_id;
    let stop_result = stop_process_group(process_group_id);
    let live_members = match live_process_group_members(process_group_id, process_group_id) {
        Ok(live_members) => live_members,
        Err(source) => {
            cleanup_completed_process_group(child, process_group, cleanup_timeout).await?;
            return Err(cleanup_failure(
                source.kind(),
                format!(
                    "could not inspect process group {process_group_id} while its leader was \
                     unreaped: {source}"
                ),
            ));
        }
    };

    if !live_members.is_empty() {
        cleanup_completed_process_group(child, process_group, cleanup_timeout).await?;
        return Err(ProcessFailure::ResidualProcessGroup { process_group_id });
    }

    match stop_result {
        Ok(ProcessGroupSignal::Delivered) => {
            let status = reap_completed_child(child, cleanup_timeout).await;
            process_group.disarm();
            status.map_err(|source| {
                cleanup_failure(
                    source.kind(),
                    format!("failed to reap observed direct child: {source}"),
                )
            })
        }
        Err(source) if source.kind() == std::io::ErrorKind::PermissionDenied => {
            // Darwin can reject a group-directed signal when every remaining
            // member is already a zombie. The unreaped leader prevents PGID
            // reuse while membership is inspected. Reap it, disarm the guard,
            // and require bounded ESRCH confirmation without another signal.
            let status = reap_completed_child(child, cleanup_timeout).await;
            process_group.disarm();
            let status = status.map_err(|reap_error| {
                cleanup_failure(
                    reap_error.kind(),
                    format!(
                        "failed to reap direct child after process-group signal was denied: \
                         {reap_error}; initial signal error: {source}"
                    ),
                )
            })?;
            wait_for_process_group_absence_without_signaling(
                process_group_id,
                cleanup_timeout,
                source,
            )
            .await?;
            Ok(status)
        }
        Ok(ProcessGroupSignal::Absent) => {
            cleanup_completed_process_group(child, process_group, cleanup_timeout).await?;
            Err(cleanup_failure(
                std::io::ErrorKind::NotFound,
                format!(
                    "process group {process_group_id} was absent while its leader remained unreaped"
                ),
            ))
        }
        Err(source) => {
            let kind = source.kind();
            cleanup_completed_process_group(child, process_group, cleanup_timeout).await?;
            Err(cleanup_failure(
                kind,
                format!(
                    "failed to stop process group {process_group_id} while its leader remained \
                     unreaped: {source}"
                ),
            ))
        }
    }
}

#[cfg(unix)]
async fn cleanup_completed_process_group(
    child: &mut tokio::process::Child,
    process_group: &mut ProcessGroupGuard,
    cleanup_timeout: Duration,
) -> Result<std::process::ExitStatus, ProcessFailure> {
    let process_group_id = process_group.process_group_id;
    let signal_failure = match signal_process_group(process_group_id) {
        Ok(ProcessGroupSignal::Delivered) => None,
        Ok(ProcessGroupSignal::Absent) => Some((
            std::io::ErrorKind::NotFound,
            "process group disappeared while its leader remained unreaped".to_owned(),
        )),
        Err(source) => Some((source.kind(), source.to_string())),
    };
    let drain_result = wait_for_live_process_group_members_to_exit(
        process_group_id,
        process_group_id,
        cleanup_timeout,
    )
    .await;
    let reap_result = reap_completed_child(child, cleanup_timeout).await;
    process_group.disarm();

    if let Err(source) = &reap_result {
        return Err(cleanup_failure(
            source.kind(),
            format!(
                "failed to reap observed direct child during completed-group cleanup: {source}; {}",
                completed_group_cleanup_attempts(signal_failure.as_ref(), drain_result.as_ref())
            ),
        ));
    }
    if let Some((kind, signal_error)) = signal_failure {
        return Err(cleanup_failure(
            kind,
            format!(
                "failed to signal completed process group {process_group_id}: {signal_error}; {}",
                completed_group_drain_attempt(drain_result.as_ref())
            ),
        ));
    }
    if let Err(source) = drain_result {
        return Err(cleanup_failure(
            source.kind(),
            format!(
                "process group {process_group_id} did not drain while its leader remained \
                 unreaped: {source}"
            ),
        ));
    }
    Ok(reap_result.expect("reap result checked above"))
}

#[cfg(unix)]
fn completed_group_cleanup_attempts(
    signal_failure: Option<&(std::io::ErrorKind, String)>,
    drain_result: Result<&(), &std::io::Error>,
) -> String {
    let signal = signal_failure
        .map(|(_, source)| format!("process-group signal failed: {source}"))
        .unwrap_or_else(|| "process-group signal succeeded".to_owned());
    format!("{signal}; {}", completed_group_drain_attempt(drain_result))
}

#[cfg(unix)]
fn completed_group_drain_attempt(drain_result: Result<&(), &std::io::Error>) -> String {
    match drain_result {
        Ok(()) => "live process-group members drained".to_owned(),
        Err(source) => format!("live process-group member drain failed: {source}"),
    }
}

#[cfg(unix)]
async fn reap_completed_child(
    child: &mut tokio::process::Child,
    cleanup_timeout: Duration,
) -> std::io::Result<std::process::ExitStatus> {
    tokio::time::timeout(cleanup_timeout, child.wait())
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("direct child reap timed out after {cleanup_timeout:?}"),
            )
        })?
}

#[cfg(unix)]
async fn wait_for_live_process_group_members_to_exit(
    process_group_id: i32,
    leader_process_id: i32,
    cleanup_timeout: Duration,
) -> std::io::Result<()> {
    let wait = async {
        loop {
            if live_process_group_members(process_group_id, leader_process_id)?.is_empty() {
                return Ok(());
            }
            tokio::time::sleep(PROCESS_GROUP_POLL_INTERVAL).await;
        }
    };
    tokio::time::timeout(cleanup_timeout, wait)
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "live members remained after {cleanup_timeout:?} while group leader was unreaped"
                ),
            )
        })?
}

#[cfg(unix)]
async fn wait_for_process_group_absence_without_signaling(
    process_group_id: i32,
    cleanup_timeout: Duration,
    initial_permission_error: std::io::Error,
) -> Result<(), ProcessFailure> {
    let wait = async {
        loop {
            match probe_process_group(process_group_id) {
                Ok(ProcessGroupPresence::Absent) => return Ok(()),
                Ok(ProcessGroupPresence::Present) => {}
                Err(source) if source.kind() == std::io::ErrorKind::PermissionDenied => {}
                Err(source) => return Err(source),
            }
            tokio::time::sleep(PROCESS_GROUP_POLL_INTERVAL).await;
        }
    };
    match tokio::time::timeout(cleanup_timeout, wait).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(source)) => Err(cleanup_failure(
            source.kind(),
            format!(
                "process-group probe failed after direct-child reap: {source}; initial \
                 pre-reap signal error: {initial_permission_error}; no post-reap signal sent"
            ),
        )),
        Err(_) => Err(cleanup_failure(
            std::io::ErrorKind::TimedOut,
            format!(
                "process group {process_group_id} did not reach ESRCH within {cleanup_timeout:?}; \
                 initial pre-reap signal error: {initial_permission_error}; no post-reap signal sent"
            ),
        )),
    }
}

#[cfg(unix)]
fn cleanup_attempts(
    group_error: &Option<std::io::Error>,
    group_retry_error: &Option<std::io::Error>,
    direct_kill_error: &Option<std::io::Error>,
) -> String {
    let group = match (group_error, group_retry_error) {
        (Some(error), Some(retry_error)) => format!(
            "process-group signal failed: {error}; process-group signal retry before reap \
             failed: {retry_error}"
        ),
        (Some(error), None) => format!(
            "process-group signal failed: {error}; process-group signal retry before reap succeeded"
        ),
        (None, _) => "process-group signal succeeded".to_owned(),
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
    fn disarm(&mut self);

    fn confirm_no_live_members_after_leader_exit(
        &mut self,
        _cleanup_timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<bool>> + Send + '_>>
    {
        Box::pin(std::future::ready(Ok(false)))
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessGroupSignal {
    Delivered,
    Absent,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessGroupPresence {
    Present,
    Absent,
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
        let result = signal_process_group(self.process_group_id).map(|_| ());
        if result.is_ok() {
            self.armed = false;
        }
        result
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl CleanupGroup for ProcessGroupGuard {
    fn kill(&mut self) -> std::io::Result<()> {
        ProcessGroupGuard::kill(self)
    }

    fn disarm(&mut self) {
        ProcessGroupGuard::disarm(self);
    }

    fn confirm_no_live_members_after_leader_exit(
        &mut self,
        cleanup_timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<bool>> + Send + '_>>
    {
        let process_group_id = self.process_group_id;
        Box::pin(async move {
            tokio::time::timeout(
                cleanup_timeout,
                wait_for_child_exit_without_reaping(process_group_id),
            )
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "direct child did not exit within {cleanup_timeout:?} after its kill signal"
                    ),
                )
            })??;
            Ok(live_process_group_members(process_group_id, process_group_id)?.is_empty())
        })
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
fn signal_process_group(process_group_id: i32) -> std::io::Result<ProcessGroupSignal> {
    send_process_group_signal(process_group_id, libc::SIGKILL)
}

#[cfg(unix)]
fn stop_process_group(process_group_id: i32) -> std::io::Result<ProcessGroupSignal> {
    send_process_group_signal(process_group_id, libc::SIGSTOP)
}

#[cfg(unix)]
fn probe_process_group(process_group_id: i32) -> std::io::Result<ProcessGroupPresence> {
    match send_process_group_signal(process_group_id, 0)? {
        ProcessGroupSignal::Delivered => Ok(ProcessGroupPresence::Present),
        ProcessGroupSignal::Absent => Ok(ProcessGroupPresence::Absent),
    }
}

#[cfg(unix)]
fn send_process_group_signal(
    process_group_id: i32,
    signal: i32,
) -> std::io::Result<ProcessGroupSignal> {
    let result = unsafe { libc::kill(-process_group_id, signal) };
    if result == 0 {
        return Ok(ProcessGroupSignal::Delivered);
    }

    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(ProcessGroupSignal::Absent)
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
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
    async fn cleanup_never_signals_the_process_group_after_reaping_its_leader() {
        let reaped = Arc::new(AtomicBool::new(false));
        let mut child = OrderedFakeChild {
            reaped: Arc::clone(&reaped),
        };
        let mut group = OrderedFakeGroup {
            reaped,
            results: VecDeque::from([
                Some(ErrorKind::PermissionDenied),
                Some(ErrorKind::PermissionDenied),
            ]),
            calls: 0,
            post_reap_calls: 0,
            disarmed: false,
        };

        let failure = cleanup_process_tree(&mut child, &mut group, Duration::from_millis(10))
            .await
            .unwrap_err();

        assert_eq!(group.calls, 2);
        assert_eq!(group.post_reap_calls, 0);
        assert!(group.disarmed);
        match failure {
            ProcessFailure::Cleanup { source } => {
                let message = source.to_string();
                assert_eq!(source.kind(), ErrorKind::PermissionDenied);
                assert!(message.contains("signal failed before reap"));
                assert!(message.contains("signal retry before reap failed"));
                assert!(message.contains("no signal sent after reap"));
            }
            other => panic!("unexpected cleanup failure: {other:?}"),
        }
    }

    #[test]
    fn cleanup_display_preserves_the_underlying_cause() {
        let failure = ProcessFailure::Cleanup {
            source: io::Error::new(
                ErrorKind::PermissionDenied,
                "process-group signal failed after successful completion",
            ),
        };

        let diagnostic = failure.to_string();
        assert!(diagnostic.contains("failed to terminate or reap process tree"));
        assert!(diagnostic.contains("process-group signal failed after successful completion"));
    }

    struct FakeChild {
        pending_wait: bool,
        kill_error: Option<ErrorKind>,
    }

    impl FakeChild {
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

    struct OrderedFakeChild {
        reaped: Arc<AtomicBool>,
    }

    impl CleanupChild for OrderedFakeChild {
        fn start_kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn wait(&mut self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            self.reaped.store(true, Ordering::SeqCst);
            Box::pin(ready(Ok(())))
        }
    }

    struct OrderedFakeGroup {
        reaped: Arc<AtomicBool>,
        results: VecDeque<Option<ErrorKind>>,
        calls: usize,
        post_reap_calls: usize,
        disarmed: bool,
    }

    impl CleanupGroup for OrderedFakeGroup {
        fn kill(&mut self) -> io::Result<()> {
            self.calls += 1;
            if self.reaped.load(Ordering::SeqCst) {
                self.post_reap_calls += 1;
            }
            match self.results.pop_front().flatten() {
                Some(kind) => Err(io::Error::from(kind)),
                None => Ok(()),
            }
        }

        fn disarm(&mut self) {
            self.disarmed = true;
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
    }

    impl CleanupGroup for FakeGroup {
        fn kill(&mut self) -> io::Result<()> {
            self.calls += 1;
            match self.results.pop_front().flatten() {
                Some(kind) => Err(io::Error::from(kind)),
                None => Ok(()),
            }
        }

        fn disarm(&mut self) {}
    }
}
