//! Managed runtime child lifecycle.
//!
//! A [`ManagedProcess`] owns one runtime child, drains both output pipes for
//! its whole lifetime and applies the platform lifecycle policy from
//! ADR-0002. Unix children are process-group leaders and signals target the
//! group. Native Windows can only terminate the direct child; runtime
//! adapters must therefore launch a single process there until Job Objects
//! are introduced.

use crate::{ByteTailRing, CommandSpec};
use std::io;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout, timeout_at};

/// Defaults used by the process supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupervisorConfig {
    /// Maximum retained stdout bytes.
    pub stdout_capacity: usize,
    /// Maximum retained stderr bytes.
    pub stderr_capacity: usize,
    /// Maximum time for a process to become ready.
    pub startup_timeout: Duration,
    /// Maximum duration of one readiness or health probe.
    pub probe_timeout: Duration,
    /// Delay between unsuccessful readiness probes.
    pub probe_interval: Duration,
    /// Grace after TERM before KILL.
    pub shutdown_grace: Duration,
    /// Maximum wait after KILL.
    pub post_kill_timeout: Duration,
    /// Maximum wait for both pipe pumps to observe EOF.
    pub pipe_eof_timeout: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            stdout_capacity: 64 * 1024,
            stderr_capacity: 64 * 1024,
            startup_timeout: Duration::from_secs(60),
            probe_timeout: Duration::from_secs(2),
            probe_interval: Duration::from_millis(100),
            shutdown_grace: Duration::from_secs(5),
            post_kill_timeout: Duration::from_secs(10),
            pipe_eof_timeout: Duration::from_secs(10),
        }
    }
}

impl SupervisorConfig {
    fn validate(self) -> Result<Self, SupervisorError> {
        if self.stdout_capacity == 0 {
            return Err(SupervisorError::InvalidConfig(
                "stdout_capacity must be non-zero",
            ));
        }
        if self.stderr_capacity == 0 {
            return Err(SupervisorError::InvalidConfig(
                "stderr_capacity must be non-zero",
            ));
        }
        for (name, value) in [
            ("startup_timeout", self.startup_timeout),
            ("probe_timeout", self.probe_timeout),
            ("probe_interval", self.probe_interval),
            ("shutdown_grace", self.shutdown_grace),
            ("post_kill_timeout", self.post_kill_timeout),
            ("pipe_eof_timeout", self.pipe_eof_timeout),
        ] {
            if value.is_zero() {
                return Err(SupervisorError::InvalidConfig(match name {
                    "startup_timeout" => "startup_timeout must be non-zero",
                    "probe_timeout" => "probe_timeout must be non-zero",
                    "probe_interval" => "probe_interval must be non-zero",
                    "shutdown_grace" => "shutdown_grace must be non-zero",
                    "post_kill_timeout" => "post_kill_timeout must be non-zero",
                    _ => "pipe_eof_timeout must be non-zero",
                }));
            }
        }
        Ok(self)
    }
}

/// Errors raised while supervising a runtime child.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    /// Invalid supervisor configuration.
    #[error("invalid supervisor configuration: {0}")]
    InvalidConfig(&'static str),
    /// The process failed to spawn.
    #[error("failed to spawn runtime child: {0}")]
    Spawn(#[source] io::Error),
    /// A configured output pipe was not returned by the OS.
    #[error("spawned runtime child did not expose its {0} pipe")]
    MissingPipe(&'static str),
    /// Waiting for or inspecting the child failed.
    #[error("failed to wait for runtime child: {0}")]
    Wait(#[source] io::Error),
    /// Reading a child output pipe failed.
    #[error("failed to drain runtime child {stream}: {source}")]
    PipeRead {
        /// Pipe name.
        stream: &'static str,
        /// Read failure.
        #[source]
        source: io::Error,
    },
    /// A pipe pump task failed unexpectedly.
    #[error("runtime child {stream} pump task failed: {source}")]
    PipeTask {
        /// Pipe name.
        stream: &'static str,
        /// Join failure.
        #[source]
        source: tokio::task::JoinError,
    },
    /// Pipes stayed open beyond the settlement deadline.
    #[error("runtime child output pipes did not reach EOF before the deadline")]
    PipeEofTimeout,
    /// A platform signal could not be sent.
    #[error("failed to send {signal} to runtime child: {source}")]
    Signal {
        /// Signal name.
        signal: &'static str,
        /// OS failure.
        #[source]
        source: io::Error,
    },
    /// The child or a verified group member survived KILL.
    #[error("runtime child did not exit after forced termination")]
    KillTimeout,
    /// The child exited before readiness succeeded.
    #[error("runtime child exited before becoming ready")]
    ExitedBeforeReady,
    /// Readiness never succeeded before the configured startup deadline.
    #[error("runtime child did not become ready before the startup deadline")]
    ReadinessTimeout,
}

/// Thread-safe bounded snapshots of the two process output streams.
#[derive(Debug, Clone)]
pub struct ProcessLogs {
    stdout: Arc<Mutex<ByteTailRing>>,
    stderr: Arc<Mutex<ByteTailRing>>,
}

impl ProcessLogs {
    fn new(stdout_capacity: usize, stderr_capacity: usize) -> Result<Self, SupervisorError> {
        let stdout = ByteTailRing::new(stdout_capacity)
            .map_err(|_| SupervisorError::InvalidConfig("stdout_capacity must be non-zero"))?;
        let stderr = ByteTailRing::new(stderr_capacity)
            .map_err(|_| SupervisorError::InvalidConfig("stderr_capacity must be non-zero"))?;
        Ok(Self {
            stdout: Arc::new(Mutex::new(stdout)),
            stderr: Arc::new(Mutex::new(stderr)),
        })
    }

    /// Return the full retained stdout tail.
    #[must_use]
    pub fn stdout_tail(&self) -> Vec<u8> {
        let ring = lock_ring(&self.stdout);
        ring.snapshot(ring.capacity())
    }

    /// Return the full retained stderr tail.
    #[must_use]
    pub fn stderr_tail(&self) -> Vec<u8> {
        let ring = lock_ring(&self.stderr);
        ring.snapshot(ring.capacity())
    }

    /// Total stdout bytes drained, including evicted bytes.
    #[must_use]
    pub fn stdout_total(&self) -> u64 {
        lock_ring(&self.stdout).total_bytes()
    }

    /// Total stderr bytes drained, including evicted bytes.
    #[must_use]
    pub fn stderr_total(&self) -> u64 {
        lock_ring(&self.stderr).total_bytes()
    }
}

fn lock_ring(ring: &Mutex<ByteTailRing>) -> MutexGuard<'_, ByteTailRing> {
    ring.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// How an explicit shutdown completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// The process was already gone when shutdown began.
    AlreadyExited,
    /// Unix TERM completed within the grace period.
    Graceful,
    /// KILL (or native Windows direct termination) was required.
    Escalated,
}

/// Stable supervisor-level classification for a completed lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitClassification {
    /// Exit status was successful and no cancellation/timeout caused it.
    Clean,
    /// Unexpected non-success exit.
    Crash,
    /// Readiness did not succeed before the startup deadline.
    StartupTimeout,
    /// The owner explicitly cancelled the lifecycle.
    Cancelled,
    /// Forced termination did not settle before its hard deadline.
    KillTimeout,
    /// Stderr contains a conservative GPU allocation/OOM signature.
    LikelyGpuOom,
}

impl ExitClassification {
    /// Map this outcome into the shared domain failure catalog. Clean exits
    /// and explicit cancellation are not failures.
    #[must_use]
    pub const fn failure_class(self) -> Option<model_serving_domain::model::FailureClass> {
        use model_serving_domain::model::FailureClass;
        match self {
            Self::Clean | Self::Cancelled => None,
            Self::LikelyGpuOom => Some(FailureClass::GpuOomLikely),
            Self::StartupTimeout => Some(FailureClass::StartupTimeout),
            Self::Crash | Self::KillTimeout => Some(FailureClass::ProcessCrash),
        }
    }
}

/// Captured final process facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitReport {
    /// Lifecycle classification.
    pub classification: ExitClassification,
    /// Numeric exit code when the platform provides one.
    pub exit_code: Option<i32>,
    /// Bounded stdout tail.
    pub stdout_tail: Vec<u8>,
    /// Bounded stderr tail.
    pub stderr_tail: Vec<u8>,
}

/// Result of a testable readiness/health probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStatus {
    /// Runtime is serving and may be published.
    Ready,
    /// Runtime is alive but not ready yet.
    Pending,
}

/// Owns and supervises one runtime subprocess.
#[derive(Debug)]
pub struct ManagedProcess {
    child: Option<Child>,
    pid: u32,
    config: SupervisorConfig,
    logs: ProcessLogs,
    stdout_pump: Option<JoinHandle<Result<(), io::Error>>>,
    stderr_pump: Option<JoinHandle<Result<(), io::Error>>>,
    status: Option<ExitStatus>,
    #[cfg(target_os = "linux")]
    group_members: Vec<GroupMember>,
}

impl ManagedProcess {
    /// Spawn a command without a shell and immediately begin draining both
    /// output streams.
    ///
    /// # Errors
    ///
    /// Returns a configuration, spawn, or missing-pipe error.
    pub fn spawn(spec: &CommandSpec, config: SupervisorConfig) -> Result<Self, SupervisorError> {
        let config = config.validate()?;
        let logs = ProcessLogs::new(config.stdout_capacity, config.stderr_capacity)?;
        let mut command = spec.to_std_command();
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        let mut child = command.spawn().map_err(SupervisorError::Spawn)?;
        let pid = child.id().ok_or_else(|| {
            SupervisorError::Spawn(io::Error::other("spawned child has no process id"))
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or(SupervisorError::MissingPipe("stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or(SupervisorError::MissingPipe("stderr"))?;
        let stdout_pump = tokio::spawn(drain_pipe(stdout, Arc::clone(&logs.stdout)));
        let stderr_pump = tokio::spawn(drain_pipe(stderr, Arc::clone(&logs.stderr)));
        Ok(Self {
            child: Some(child),
            pid,
            config,
            logs,
            stdout_pump: Some(stdout_pump),
            stderr_pump: Some(stderr_pump),
            status: None,
            #[cfg(target_os = "linux")]
            group_members: Vec::new(),
        })
    }

    /// Direct child PID (and Unix process-group id).
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Cloneable live log view.
    #[must_use]
    pub fn logs(&self) -> ProcessLogs {
        self.logs.clone()
    }

    /// Observe child exit without blocking.
    ///
    /// # Errors
    ///
    /// Returns [`SupervisorError::Wait`] when the OS status query fails.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, SupervisorError> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let Some(child) = self.child.as_mut() else {
            return Ok(self.status);
        };
        let status = child.try_wait().map_err(SupervisorError::Wait)?;
        if let Some(status) = status {
            self.status = Some(status);
        } else {
            self.refresh_group_snapshot();
        }
        Ok(status)
    }

    /// Wait for the direct child and require both output pumps to reach EOF.
    ///
    /// # Errors
    ///
    /// Returns a wait, pipe-read, pipe-task, or pipe-EOF timeout error.
    pub async fn wait(&mut self) -> Result<ExitStatus, SupervisorError> {
        let status = self.wait_child().await?;
        // `shutdown` is descendant-aware even though the leader has already
        // exited: a snapshot member can still own the pipes and process group.
        let _ = self.shutdown().await?;
        Ok(status)
    }

    /// Poll an injected readiness operation until success, process exit, or
    /// the startup deadline. Each individual probe has its own deadline.
    ///
    /// Both failure paths clean up the child before returning: if it exited
    /// before becoming ready, its output pipes are settled first; if the
    /// startup deadline expires, the full ADR-0002 shutdown (TERM, grace,
    /// KILL, pipe settle) runs, so a failed startup cannot orphan a process.
    ///
    /// # Errors
    ///
    /// Returns [`SupervisorError::ExitedBeforeReady`]
    /// (after pipe settlement) or [`SupervisorError::ReadinessTimeout`]
    /// (after shutdown); shutdown and pipe-settle failures propagate as
    /// their own [`SupervisorError`] variants.
    pub async fn wait_ready<F, Fut, E>(&mut self, mut probe: F) -> Result<(), SupervisorError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<ProbeStatus, E>>,
    {
        let deadline = Instant::now() + self.config.startup_timeout;
        loop {
            if self.try_wait()?.is_some() {
                let _ = self.shutdown().await?;
                return Err(SupervisorError::ExitedBeforeReady);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.shutdown().await?;
                return Err(SupervisorError::ReadinessTimeout);
            }
            let per_probe = remaining.min(self.config.probe_timeout);
            if let Ok(Ok(ProbeStatus::Ready)) = timeout(per_probe, probe()).await {
                if self.try_wait()?.is_none() {
                    return Ok(());
                }
                let _ = self.shutdown().await?;
                return Err(SupervisorError::ExitedBeforeReady);
            }
            if Instant::now() >= deadline {
                self.shutdown().await?;
                return Err(SupervisorError::ReadinessTimeout);
            }
            sleep(self.config.probe_interval.min(remaining)).await;
        }
    }

    /// Execute one ongoing health probe under its configured deadline.
    ///
    /// # Errors
    ///
    /// Returns [`SupervisorError::Wait`] when child inspection fails.
    pub async fn probe_health<F, Fut, E>(&mut self, probe: F) -> Result<bool, SupervisorError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<ProbeStatus, E>>,
    {
        if self.try_wait()?.is_some() {
            return Ok(false);
        }
        Ok(matches!(
            timeout(self.config.probe_timeout, probe()).await,
            Ok(Ok(ProbeStatus::Ready))
        ))
    }

    /// Gracefully terminate then force-kill as required by ADR-0002.
    ///
    /// # Errors
    ///
    /// Returns signal, wait, pipe, or hard kill-timeout failures.
    pub async fn shutdown(&mut self) -> Result<ShutdownOutcome, SupervisorError> {
        let leader_gone = self.try_wait()?.is_some();
        #[cfg(unix)]
        if leader_gone && self.verified_group_has_live_member() {
            self.send_owned_group_signal(libc::SIGKILL, "KILL")?;
            self.wait_for_forced_settlement().await?;
            return Ok(ShutdownOutcome::Escalated);
        }
        if leader_gone {
            self.settle_pipes().await?;
            return Ok(ShutdownOutcome::AlreadyExited);
        }

        #[cfg(unix)]
        self.send_owned_group_signal(libc::SIGTERM, "TERM")?;
        #[cfg(windows)]
        {
            self.child_mut()?
                .start_kill()
                .map_err(|source| SupervisorError::Signal {
                    signal: "TerminateProcess",
                    source,
                })?;
        }

        let grace_deadline = Instant::now() + self.config.shutdown_grace;
        loop {
            let leader_gone = self.try_wait()?.is_some();
            #[cfg(unix)]
            let settled = leader_gone && !self.verified_group_has_live_member();
            #[cfg(windows)]
            let settled = leader_gone;
            if settled {
                self.settle_pipes().await?;
                return Ok(ShutdownOutcome::Graceful);
            }
            if Instant::now() >= grace_deadline {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }

        #[cfg(unix)]
        self.send_owned_group_signal(libc::SIGKILL, "KILL")?;
        #[cfg(windows)]
        self.child_mut()?
            .start_kill()
            .map_err(|source| SupervisorError::Signal {
                signal: "TerminateProcess",
                source,
            })?;

        self.wait_for_forced_settlement().await?;
        Ok(ShutdownOutcome::Escalated)
    }

    async fn wait_for_forced_settlement(&mut self) -> Result<(), SupervisorError> {
        let hard_deadline = Instant::now() + self.config.post_kill_timeout;
        loop {
            let leader_gone = self.try_wait()?.is_some();
            #[cfg(unix)]
            let settled = leader_gone && !self.verified_group_has_live_member();
            #[cfg(windows)]
            let settled = leader_gone;
            if settled {
                self.settle_pipes().await?;
                return Ok(());
            }
            if Instant::now() >= hard_deadline {
                return Err(SupervisorError::KillTimeout);
            }
            sleep(Duration::from_millis(20)).await;
        }
    }

    /// Terminate due to cancellation and return a final bounded report.
    ///
    /// # Errors
    ///
    /// Returns failures encountered while terminating or settling pipes.
    pub async fn cancel(&mut self) -> Result<ExitReport, SupervisorError> {
        match self.shutdown().await {
            Ok(_) => Ok(self.report(ExitClassification::Cancelled)),
            Err(SupervisorError::KillTimeout) => Ok(self.report(ExitClassification::KillTimeout)),
            Err(error) => Err(error),
        }
    }

    /// Terminate a startup that exceeded its deadline and report timeout.
    ///
    /// # Errors
    ///
    /// Returns failures encountered while terminating or settling pipes.
    pub async fn startup_timed_out(&mut self) -> Result<ExitReport, SupervisorError> {
        match self.shutdown().await {
            Ok(_) => Ok(self.report(ExitClassification::StartupTimeout)),
            Err(SupervisorError::KillTimeout) => Ok(self.report(ExitClassification::KillTimeout)),
            Err(error) => Err(error),
        }
    }

    /// Classify a naturally completed process using status plus bounded
    /// stderr. OOM is deliberately signature-based; exit 137 alone is not
    /// enough to trigger eviction.
    #[must_use]
    pub fn classify_exit(&self, status: ExitStatus) -> ExitReport {
        let stderr = self.logs.stderr_tail();
        let classification = if status.success() {
            ExitClassification::Clean
        } else if has_likely_oom_signature(&stderr) {
            ExitClassification::LikelyGpuOom
        } else {
            ExitClassification::Crash
        };
        self.report(classification)
    }

    fn report(&self, classification: ExitClassification) -> ExitReport {
        ExitReport {
            classification,
            exit_code: self.status.and_then(|status| status.code()),
            stdout_tail: self.logs.stdout_tail(),
            stderr_tail: self.logs.stderr_tail(),
        }
    }

    async fn wait_child(&mut self) -> Result<ExitStatus, SupervisorError> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            sleep(Duration::from_millis(20)).await;
        }
    }

    #[cfg(windows)]
    fn child_mut(&mut self) -> Result<&mut Child, SupervisorError> {
        self.child
            .as_mut()
            .ok_or_else(|| SupervisorError::Wait(io::Error::other("child already released")))
    }

    async fn settle_pipes(&mut self) -> Result<(), SupervisorError> {
        let deadline = Instant::now() + self.config.pipe_eof_timeout;
        join_pump(&mut self.stdout_pump, "stdout", deadline).await?;
        join_pump(&mut self.stderr_pump, "stderr", deadline).await
    }

    #[cfg(unix)]
    #[allow(unsafe_code)]
    fn send_owned_group_signal(
        &mut self,
        signal: libc::c_int,
        name: &'static str,
    ) -> Result<(), SupervisorError> {
        let leader_alive = self.try_wait()?.is_none();
        // `try_wait` is the single liveness path: it stores any observed exit
        // and, while the leader is alive, refreshes the group snapshot. The
        // ownership gate below therefore reads a snapshot verified only while
        // the leader lived.
        if !leader_alive && !self.verified_group_is_ours() {
            return Ok(());
        }
        let pgid = i32::try_from(self.pid).map_err(|_| SupervisorError::Signal {
            signal: name,
            source: io::Error::other("child pid does not fit platform pid type"),
        })?;
        // SAFETY: while the group leader is alive, pid == pgid proves group
        // ownership. After leader exit, verified_group_is_ours checks a
        // (pid,starttime,pgrp) snapshot captured only while it was alive.
        let result = unsafe { libc::kill(-pgid, signal) };
        if result == 0 {
            Ok(())
        } else {
            let source = io::Error::last_os_error();
            if source.raw_os_error() == Some(libc::ESRCH) {
                Ok(())
            } else {
                Err(SupervisorError::Signal {
                    signal: name,
                    source,
                })
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[allow(clippy::unused_self)]
    fn refresh_group_snapshot(&mut self) {}

    #[cfg(target_os = "linux")]
    fn refresh_group_snapshot(&mut self) {
        self.group_members = scan_group_members(self.pid);
    }

    #[cfg(target_os = "linux")]
    fn verified_group_is_ours(&self) -> bool {
        self.group_members
            .iter()
            .any(|member| member_matches(*member, self.pid, false))
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    fn verified_group_is_ours(&self) -> bool {
        false
    }

    #[cfg(target_os = "linux")]
    fn verified_group_has_live_member(&self) -> bool {
        self.group_members
            .iter()
            .any(|member| member_matches(*member, self.pid, true))
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    fn verified_group_has_live_member(&self) -> bool {
        false
    }
}

impl Drop for ManagedProcess {
    #[cfg_attr(unix, allow(unsafe_code))]
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let alive = self.status.is_none() && matches!(child.try_wait(), Ok(None));
        if alive {
            #[cfg(unix)]
            if let Ok(pgid) = i32::try_from(self.pid) {
                // SAFETY: the direct group leader was just confirmed alive;
                // pid == pgid still proves ownership.
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
            #[cfg(windows)]
            let _ = child.start_kill();
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = child.wait().await;
            });
        } else {
            let _ = child.start_kill();
        }
    }
}

async fn drain_pipe<R>(mut pipe: R, ring: Arc<Mutex<ByteTailRing>>) -> Result<(), io::Error>
where
    R: AsyncRead + Unpin,
{
    let mut chunk = [0u8; 8192];
    loop {
        let count = pipe.read(&mut chunk).await?;
        if count == 0 {
            return Ok(());
        }
        lock_ring(&ring).append(&chunk[..count]);
    }
}

async fn join_pump(
    task: &mut Option<JoinHandle<Result<(), io::Error>>>,
    stream: &'static str,
    deadline: Instant,
) -> Result<(), SupervisorError> {
    let Some(mut handle) = task.take() else {
        return Ok(());
    };
    match timeout_at(deadline, &mut handle).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(source))) => Err(SupervisorError::PipeRead { stream, source }),
        Ok(Err(source)) => Err(SupervisorError::PipeTask { stream, source }),
        Err(_) => {
            *task = Some(handle);
            Err(SupervisorError::PipeEofTimeout)
        }
    }
}

fn has_likely_oom_signature(stderr: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    [
        "cuda out of memory",
        "hip out of memory",
        "failed to allocate device memory",
        "cublas_status_alloc_failed",
    ]
    .iter()
    .any(|signature| text.contains(signature))
}

use std::future::Future;

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
struct GroupMember {
    pid: u32,
    starttime: u64,
}

#[cfg(target_os = "linux")]
fn scan_group_members(pgid: u32) -> Vec<GroupMember> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(read_proc_stat)
        .filter(|(_, _, member_pgid, _)| *member_pgid == pgid)
        .map(|(pid, _, _, starttime)| GroupMember { pid, starttime })
        .collect()
}

#[cfg(target_os = "linux")]
fn member_matches(member: GroupMember, pgid: u32, require_live: bool) -> bool {
    read_proc_stat(member.pid).is_some_and(|(_, state, current_pgid, starttime)| {
        starttime == member.starttime && current_pgid == pgid && (!require_live || state != 'Z')
    })
}

#[cfg(target_os = "linux")]
fn read_proc_stat(pid: u32) -> Option<(u32, char, u32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let end = stat.rfind(')')?;
    let fields: Vec<&str> = stat[end + 1..].split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    let pgrp = fields.get(2)?.parse().ok()?;
    let starttime = fields.get(19)?.parse().ok()?;
    Some((pid, state, pgrp, starttime))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oom_classification_is_conservative() {
        assert!(has_likely_oom_signature(
            b"CUDA out of memory while loading"
        ));
        assert!(!has_likely_oom_signature(b"process killed with status 137"));
        assert!(!has_likely_oom_signature(b"host out of memory"));
    }

    #[test]
    fn default_memory_bounds_are_explicit() {
        let config = SupervisorConfig::default();
        assert_eq!(config.stdout_capacity, 64 * 1024);
        assert_eq!(config.stderr_capacity, 64 * 1024);
    }

    #[test]
    fn zero_probe_interval_is_rejected() {
        let config = SupervisorConfig {
            probe_interval: Duration::ZERO,
            ..SupervisorConfig::default()
        };
        assert!(matches!(
            config.validate(),
            Err(SupervisorError::InvalidConfig(
                "probe_interval must be non-zero"
            ))
        ));
    }

    #[test]
    fn exit_classification_maps_to_the_domain_catalog() {
        use model_serving_domain::model::FailureClass;
        assert_eq!(ExitClassification::Clean.failure_class(), None);
        assert_eq!(ExitClassification::Cancelled.failure_class(), None);
        assert_eq!(
            ExitClassification::LikelyGpuOom.failure_class(),
            Some(FailureClass::GpuOomLikely)
        );
        assert_eq!(
            ExitClassification::StartupTimeout.failure_class(),
            Some(FailureClass::StartupTimeout)
        );
        assert_eq!(
            ExitClassification::Crash.failure_class(),
            Some(FailureClass::ProcessCrash)
        );
        assert_eq!(
            ExitClassification::KillTimeout.failure_class(),
            Some(FailureClass::ProcessCrash)
        );
    }

    #[tokio::test]
    async fn pipe_timeout_keeps_handle_for_a_later_settlement_attempt() {
        let mut task = Some(tokio::spawn(async {
            sleep(Duration::from_millis(80)).await;
            Ok(())
        }));
        let first = join_pump(
            &mut task,
            "stdout",
            Instant::now() + Duration::from_millis(5),
        )
        .await;
        assert!(matches!(first, Err(SupervisorError::PipeEofTimeout)));
        assert!(task.is_some(), "timed-out pump must remain joinable");
        join_pump(&mut task, "stdout", Instant::now() + Duration::from_secs(1))
            .await
            .expect("second settlement joins the original pump");
        assert!(task.is_none());
    }
}
