//! Bounded, shell-free executable probing (`docs/architecture.md` §4).
//!
//! A runtime is registered as an executable path; before it can be used the
//! control plane must learn *what* it is. [`probe`] runs the executable twice
//! — once with `--version`, once with `--help` — under a strict budget, and
//! returns a [`ProbeSnapshot`] carrying the raw version text, the raw help
//! text and a [`Capabilities`] value pre-filled with the version string.
//!
//! Safety properties:
//!
//! - **No shell.** The executable is spawned directly with an argv vector;
//!   the configured arguments are never re-parsed by a shell.
//! - **Bounded time.** Each invocation is killed and reaped when it exceeds
//!   [`ProbeConfig::timeout`]. The deadline starts *before* spawn and covers
//!   waiting for the child, killing it, and draining both pipes to EOF, so a
//!   descendant process that inherits the pipes and outlives the child can
//!   never make [`probe`] hang.
//! - **Bounded output.** stdout and stderr are drained concurrently (so a
//!   chatty child can never deadlock on a full pipe) but only the first
//!   [`ProbeConfig::max_stdout_bytes`] / [`ProbeConfig::max_stderr_bytes`]
//!   bytes are retained. The first excess byte kills the child and fails with
//!   [`ProbeError::OutputTooLarge`] — the probe never buffers unbounded data.
//! - **Deterministic cleanup.** Every failure path kills and reaps the child
//!   and aborts the pipe pump tasks, so no task is ever awaited after a kill
//!   and no zombie or detached reader is left behind.
//! - **Deterministic failure taxonomy.** Every failure is one of the typed
//!   [`ProbeError`] variants, not a stringly-typed I/O error, so `doctor`
//!   (and future API error mapping) can branch on the cause.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use model_serving_domain::model::Capabilities;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt as _;
use tokio::sync::Notify;

/// Default per-invocation probe timeout.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Default retained-bytes cap for each stream.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Which of the two probe invocations failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStage {
    /// The `--version` invocation.
    Version,
    /// The `--help` invocation.
    Help,
}

impl std::fmt::Display for ProbeStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Version => formatter.write_str("version"),
            Self::Help => formatter.write_str("help"),
        }
    }
}

/// Which captured stream exceeded its byte budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

impl std::fmt::Display for OutputStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdout => formatter.write_str("stdout"),
            Self::Stderr => formatter.write_str("stderr"),
        }
    }
}

/// Budget and argument configuration for [`probe`].
///
/// The default is a 5-second timeout, 256 KiB per stream, and the conventional
/// `--version` / `--help` arguments, inheriting the parent environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeConfig {
    timeout: Duration,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
    version_arg: OsString,
    help_arg: OsString,
    env: Vec<(OsString, OsString)>,
    clear_env: bool,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_PROBE_TIMEOUT,
            max_stdout_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_stderr_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            version_arg: OsString::from("--version"),
            help_arg: OsString::from("--help"),
            env: Vec::new(),
            clear_env: false,
        }
    }
}

impl ProbeConfig {
    /// A new configuration with the documented defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the per-invocation timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the retained-bytes cap for both streams.
    #[must_use]
    pub fn with_max_output_bytes(mut self, max_bytes: usize) -> Self {
        self.max_stdout_bytes = max_bytes;
        self.max_stderr_bytes = max_bytes;
        self
    }

    /// Set the retained-bytes cap for stdout.
    #[must_use]
    pub fn with_max_stdout_bytes(mut self, max_bytes: usize) -> Self {
        self.max_stdout_bytes = max_bytes;
        self
    }

    /// Set the retained-bytes cap for stderr.
    #[must_use]
    pub fn with_max_stderr_bytes(mut self, max_bytes: usize) -> Self {
        self.max_stderr_bytes = max_bytes;
        self
    }

    /// Override the argument used for the version invocation.
    #[must_use]
    pub fn with_version_arg(mut self, arg: impl Into<OsString>) -> Self {
        self.version_arg = arg.into();
        self
    }

    /// Override the argument used for the help invocation.
    #[must_use]
    pub fn with_help_arg(mut self, arg: impl Into<OsString>) -> Self {
        self.help_arg = arg.into();
        self
    }

    /// Add an environment entry for the probed child (order preserved).
    #[must_use]
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Set whether the child environment is cleared before `env` is applied.
    #[must_use]
    pub fn with_clear_env(mut self, clear_env: bool) -> Self {
        self.clear_env = clear_env;
        self
    }

    /// The per-invocation timeout.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The retained stdout cap.
    #[must_use]
    pub fn max_stdout_bytes(&self) -> usize {
        self.max_stdout_bytes
    }

    /// The retained stderr cap.
    #[must_use]
    pub fn max_stderr_bytes(&self) -> usize {
        self.max_stderr_bytes
    }

    /// The version invocation argument.
    #[must_use]
    pub fn version_arg(&self) -> &OsString {
        &self.version_arg
    }

    /// The help invocation argument.
    #[must_use]
    pub fn help_arg(&self) -> &OsString {
        &self.help_arg
    }

    /// The configured child environment entries.
    #[must_use]
    pub fn env(&self) -> &[(OsString, OsString)] {
        &self.env
    }

    /// Whether the child environment is cleared before `env` is applied.
    #[must_use]
    pub fn clears_env(&self) -> bool {
        self.clear_env
    }
}

/// A typed, deterministic probe failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProbeError {
    /// The executable path does not exist.
    #[error("runtime executable not found: {}", path.display())]
    Missing {
        /// The path that was probed.
        path: PathBuf,
    },
    /// The path exists but is not a regular file (e.g. a directory).
    #[error("runtime path is not a regular file: {}", path.display())]
    NotFile {
        /// The path that was probed.
        path: PathBuf,
    },
    /// The path is a regular file but has no execute permission (Unix).
    #[error("runtime path is not executable: {}", path.display())]
    NotExecutable {
        /// The path that was probed.
        path: PathBuf,
    },
    /// The path is empty or not absolute. A bare name would be resolved by
    /// the OS through `PATH`, so the control plane forbids it before spawning.
    #[error(
        "runtime executable path must be a non-empty absolute path: {}",
        path.display()
    )]
    NotAbsolute {
        /// The path that was probed.
        path: PathBuf,
    },
    /// The process could not be spawned.
    #[error("failed to spawn {}: {message}", path.display())]
    Spawn {
        /// The path that was probed.
        path: PathBuf,
        /// A concrete, human-readable cause.
        message: String,
    },
    /// The invocation exceeded its deadline and was killed and reaped. This
    /// also covers a child that exited while a descendant kept its pipes open:
    /// the deadline bounds the complete invocation, including pipe draining.
    #[error("runtime {stage} probe timed out after {timeout:?}: {}", path.display())]
    Timeout {
        /// The path that was probed.
        path: PathBuf,
        /// Which invocation exceeded its deadline.
        stage: ProbeStage,
        /// The configured timeout.
        timeout: Duration,
    },
    /// The invocation exited with a non-zero status.
    #[error("runtime exited with {status} during the {stage} probe ({}): {detail}", path.display())]
    NonZero {
        /// The path that was probed.
        path: PathBuf,
        /// Which invocation failed.
        stage: ProbeStage,
        /// The numeric exit code, if the process exited normally.
        exit_code: Option<i32>,
        /// A rendered status, e.g. `exit code 7` or `signal 9`.
        status: String,
        /// Captured stderr (or stdout when stderr was empty).
        detail: String,
    },
    /// A captured stream exceeded its configured byte cap.
    #[error("runtime {stage} {stream} output exceeded {limit} bytes: {}", path.display())]
    OutputTooLarge {
        /// The path that was probed.
        path: PathBuf,
        /// Which invocation overflowed.
        stage: ProbeStage,
        /// Which stream overflowed.
        stream: OutputStream,
        /// The configured cap.
        limit: usize,
    },
    /// Reading a captured stream failed (not a clean EOF).
    #[error("runtime {stage} {stream} could not be read: {message}: {}", path.display())]
    PipeRead {
        /// The path that was probed.
        path: PathBuf,
        /// Which invocation hit the read error.
        stage: ProbeStage,
        /// Which stream failed.
        stream: OutputStream,
        /// A concrete, human-readable cause.
        message: String,
    },
    /// The captured output was not valid UTF-8.
    #[error("runtime {stage} output was not valid UTF-8: {}", path.display())]
    Malformed {
        /// The path that was probed.
        path: PathBuf,
        /// Which invocation produced the malformed output.
        stage: ProbeStage,
        /// A concrete, human-readable cause.
        message: String,
    },
    /// The executable ran but is not a compatible engine (version too old,
    /// unrecognized help shape, ...). Produced by adapters after inspecting a
    /// [`ProbeSnapshot`]; the generic probe itself never emits it.
    #[error("runtime is incompatible ({}): {message}", path.display())]
    Incompatible {
        /// The path that was probed.
        path: PathBuf,
        /// Why the executable is considered incompatible.
        message: String,
    },
}

impl ProbeError {
    /// Build an [`ProbeError::Incompatible`] for adapter-level checks.
    #[must_use]
    pub fn incompatible(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Incompatible {
            path: path.into(),
            message: message.into(),
        }
    }

    /// The probed path, whatever the cause.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Missing { path }
            | Self::NotFile { path }
            | Self::NotExecutable { path }
            | Self::NotAbsolute { path }
            | Self::Spawn { path, .. }
            | Self::Timeout { path, .. }
            | Self::NonZero { path, .. }
            | Self::OutputTooLarge { path, .. }
            | Self::PipeRead { path, .. }
            | Self::Malformed { path, .. }
            | Self::Incompatible { path, .. } => path,
        }
    }
}

/// The result of a successful [`probe`].
///
/// `capabilities.version` is pre-filled from `version_text`; engine-specific
/// endpoint flags are left `false` for the adapter to derive from `help_text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeSnapshot {
    /// The probed executable.
    pub executable: PathBuf,
    /// The trimmed version text reported by `--version`.
    pub version_text: String,
    /// The trimmed help text reported by `--help`.
    pub help_text: String,
    /// Capabilities base value carried to adapters.
    pub capabilities: Capabilities,
}

impl ProbeSnapshot {
    /// The raw version text.
    #[must_use]
    pub fn version_text(&self) -> &str {
        &self.version_text
    }

    /// The raw help text.
    #[must_use]
    pub fn help_text(&self) -> &str {
        &self.help_text
    }

    /// The capability base value.
    #[must_use]
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }
}

/// Probe `executable` for its version and help output under `config`.
///
/// Runs the executable twice (version, then help) with a controlled, shell-free
/// argv. Both invocations are individually subject to the configured timeout
/// and output caps.
///
/// # Errors
///
/// Returns a [`ProbeError`] variant describing the first failure: the path is
/// empty / not absolute / missing / not a file / not executable, the process
/// could not be spawned,
/// the invocation timed out, exited non-zero, exceeded an output cap, or
/// produced non-UTF-8 output. See [`ProbeError`].
pub async fn probe(executable: &Path, config: &ProbeConfig) -> Result<ProbeSnapshot, ProbeError> {
    check_path(executable)?;

    let version_args = std::slice::from_ref(&config.version_arg);
    let version = run(executable, version_args, config, ProbeStage::Version).await?;
    let version_text = stream_text(executable, &version, ProbeStage::Version)?;

    let help_args = std::slice::from_ref(&config.help_arg);
    let help = run(executable, help_args, config, ProbeStage::Help).await?;
    let help_text = stream_text(executable, &help, ProbeStage::Help)?;

    let capabilities = Capabilities {
        version: if version_text.is_empty() {
            None
        } else {
            Some(version_text.clone())
        },
        ..Capabilities::default()
    };

    Ok(ProbeSnapshot {
        executable: executable.to_path_buf(),
        version_text,
        help_text,
        capabilities,
    })
}

/// Validate the path *before* spawning so the common misconfigurations map to
/// precise variants and we never rely on a platform-specific spawn error.
fn check_path(path: &Path) -> Result<(), ProbeError> {
    // A bare name would be resolved through `PATH` by the spawn call, so an
    // empty or relative path is rejected up front rather than accepted as a
    // look-up. This also covers the empty path, which is never absolute.
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(ProbeError::NotAbsolute {
            path: path.to_path_buf(),
        });
    }

    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProbeError::Missing {
                path: path.to_path_buf(),
            });
        }
        Err(error) => {
            return Err(ProbeError::Spawn {
                path: path.to_path_buf(),
                message: error.to_string(),
            });
        }
    };

    if !metadata.is_file() {
        return Err(ProbeError::NotFile {
            path: path.to_path_buf(),
        });
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(ProbeError::NotExecutable {
                path: path.to_path_buf(),
            });
        }
    }

    Ok(())
}

/// Captured streams of one completed invocation.
#[derive(Debug)]
struct Captured {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Output retained by one pipe pump.
#[derive(Debug)]
struct Pumped {
    data: Vec<u8>,
    overflowed: bool,
    /// A pipe read error, preserved as a typed failure instead of being
    /// silently treated as EOF.
    error: Option<std::io::Error>,
}

impl Pumped {
    /// An empty capture, used when a pump task was cancelled.
    fn empty() -> Self {
        Self {
            data: Vec::new(),
            overflowed: false,
            error: None,
        }
    }
}

/// What terminated the `select!` in [`run`].
#[derive(Debug)]
enum Ended {
    /// The child was waited on (successfully or with an I/O error).
    Exited(std::io::Result<ExitStatus>),
    /// A stream exceeded its cap and the child is being killed.
    Overflow(OutputStream),
    /// The overall deadline elapsed.
    TimedOut,
}

/// Spawn one invocation, drain both pipes concurrently, and enforce the
/// timeout and output caps. Kills and reaps the child on every failure path.
///
/// The `deadline` computed here covers spawning, waiting for the child, and
/// draining both pipes to EOF. When the child exits but a descendant inherited
/// the pipes and keeps them open, draining is cut off at the same deadline and
/// the pump tasks are aborted, so `run` can never hang.
async fn run(
    executable: &Path,
    args: &[OsString],
    config: &ProbeConfig,
    stage: ProbeStage,
) -> Result<Captured, ProbeError> {
    // The deadline starts before spawn so it covers the whole invocation.
    let deadline = Instant::now() + config.timeout;

    let mut command = tokio::process::Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if config.clear_env {
        command.env_clear();
    }
    command.envs(config.env.iter().map(|(key, value)| (key, value)));

    let mut child = command
        .spawn()
        .map_err(|error| spawn_error(executable, &error))?;

    let stdout = child.stdout.take().ok_or_else(|| ProbeError::Spawn {
        path: executable.to_path_buf(),
        message: "stdout pipe was not created".to_owned(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| ProbeError::Spawn {
        path: executable.to_path_buf(),
        message: "stderr pipe was not created".to_owned(),
    })?;

    let out_overflow = Arc::new(Notify::new());
    let err_overflow = Arc::new(Notify::new());
    let out_task = tokio::spawn(pump(
        stdout,
        config.max_stdout_bytes,
        Arc::clone(&out_overflow),
    ));
    let err_task = tokio::spawn(pump(
        stderr,
        config.max_stderr_bytes,
        Arc::clone(&err_overflow),
    ));

    let ended = {
        let wait = child.wait();
        tokio::pin!(wait);
        let timeout = tokio::time::sleep_until(deadline.into());
        tokio::pin!(timeout);
        tokio::select! {
            biased;
            () = out_overflow.notified() => Ended::Overflow(OutputStream::Stdout),
            () = err_overflow.notified() => Ended::Overflow(OutputStream::Stderr),
            result = &mut wait => Ended::Exited(result),
            () = &mut timeout => Ended::TimedOut,
        }
    };

    match ended {
        Ended::Exited(Ok(status)) => {
            let (out, err) = drain(executable, stage, config, deadline, out_task, err_task).await?;
            if out.overflowed {
                return Err(overflow(executable, stage, OutputStream::Stdout, config));
            }
            if err.overflowed {
                return Err(overflow(executable, stage, OutputStream::Stderr, config));
            }
            if let Some(error) = out.error {
                return Err(pipe_read(executable, stage, OutputStream::Stdout, &error));
            }
            if let Some(error) = err.error {
                return Err(pipe_read(executable, stage, OutputStream::Stderr, &error));
            }
            if !status.success() {
                return Err(non_zero(executable, stage, status, &out.data, &err.data));
            }
            Ok(Captured {
                stdout: out.data,
                stderr: err.data,
            })
        }
        Ended::Exited(Err(error)) => {
            kill_and_reap(&mut child).await;
            abort_pumps(&out_task, &err_task);
            Err(ProbeError::Spawn {
                path: executable.to_path_buf(),
                message: error.to_string(),
            })
        }
        Ended::Overflow(stream) => {
            kill_and_reap(&mut child).await;
            abort_pumps(&out_task, &err_task);
            Err(overflow(executable, stage, stream, config))
        }
        Ended::TimedOut => {
            kill_and_reap(&mut child).await;
            abort_pumps(&out_task, &err_task);
            Err(ProbeError::Timeout {
                path: executable.to_path_buf(),
                stage,
                timeout: config.timeout,
            })
        }
    }
}

/// Drain both pump tasks, bounded by the invocation `deadline`.
///
/// On success the join results are unwrapped into [`Pumped`] values. When the
/// deadline elapses first (a descendant is holding the pipes open) both tasks
/// are aborted and a [`ProbeError::Timeout`] is returned. The tasks are never
/// awaited after being aborted, so this cannot hang.
async fn drain(
    executable: &Path,
    stage: ProbeStage,
    config: &ProbeConfig,
    deadline: Instant,
    mut out_task: tokio::task::JoinHandle<Pumped>,
    mut err_task: tokio::task::JoinHandle<Pumped>,
) -> Result<(Pumped, Pumped), ProbeError> {
    let joined = tokio::time::timeout_at(deadline.into(), async {
        let out = (&mut out_task).await;
        let err = (&mut err_task).await;
        (out, err)
    })
    .await;

    if let Ok((out, err)) = joined {
        Ok((join_pumped(out), join_pumped(err)))
    } else {
        abort_pumps(&out_task, &err_task);
        Err(ProbeError::Timeout {
            path: executable.to_path_buf(),
            stage,
            timeout: config.timeout,
        })
    }
}

/// Abort the pipes without awaiting them, so a task blocked on a held-open
/// pipe can never keep the caller waiting.
fn abort_pumps(
    out_task: &tokio::task::JoinHandle<Pumped>,
    err_task: &tokio::task::JoinHandle<Pumped>,
) {
    out_task.abort();
    err_task.abort();
}

/// Unwrap a pump join result, treating a cancelled task as an empty capture.
fn join_pumped(result: Result<Pumped, tokio::task::JoinError>) -> Pumped {
    result.unwrap_or_else(|_| Pumped::empty())
}

/// Best-effort kill followed by a reap so no zombie is left behind.
async fn kill_and_reap(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Read one pipe to EOF, retaining at most `limit` bytes but always draining
/// so the child can never block on a full pipe. Notifies `overflow` on the
/// first excess byte. A read error is preserved in [`Pumped::error`] rather
/// than being treated as a clean EOF.
async fn pump<R>(mut reader: R, limit: usize, overflow: Arc<Notify>) -> Pumped
where
    R: AsyncRead + Unpin,
{
    let mut data = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut overflowed = false;
    let mut signalled = false;

    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) => {
                return Pumped {
                    data,
                    overflowed,
                    error: Some(error),
                };
            }
        };

        if data.len() < limit {
            let room = limit - data.len();
            let keep = room.min(read);
            data.extend_from_slice(&buffer[..keep]);
            if keep < read {
                overflowed = true;
            }
        } else {
            overflowed = true;
        }

        if overflowed && !signalled {
            signalled = true;
            overflow.notify_one();
        }
    }

    Pumped {
        data,
        overflowed,
        error: None,
    }
}

/// Map a spawn error to a precise variant, covering the race where the file
/// changed between [`check_path`] and `spawn`.
fn spawn_error(path: &Path, error: &std::io::Error) -> ProbeError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ProbeError::Missing {
            path: path.to_path_buf(),
        },
        std::io::ErrorKind::PermissionDenied => ProbeError::NotExecutable {
            path: path.to_path_buf(),
        },
        _ => ProbeError::Spawn {
            path: path.to_path_buf(),
            message: error.to_string(),
        },
    }
}

/// Construct the overflow error for `stream`, using the matching cap.
fn overflow(
    path: &Path,
    stage: ProbeStage,
    stream: OutputStream,
    config: &ProbeConfig,
) -> ProbeError {
    let limit = match stream {
        OutputStream::Stdout => config.max_stdout_bytes,
        OutputStream::Stderr => config.max_stderr_bytes,
    };
    ProbeError::OutputTooLarge {
        path: path.to_path_buf(),
        stage,
        stream,
        limit,
    }
}

/// Construct the typed pipe-read error for `stream`.
fn pipe_read(
    path: &Path,
    stage: ProbeStage,
    stream: OutputStream,
    error: &std::io::Error,
) -> ProbeError {
    ProbeError::PipeRead {
        path: path.to_path_buf(),
        stage,
        stream,
        message: error.to_string(),
    }
}

/// Construct the non-zero-exit error, preferring stderr for the detail line.
fn non_zero(
    path: &Path,
    stage: ProbeStage,
    status: ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> ProbeError {
    let stderr_text = String::from_utf8_lossy(stderr);
    let stdout_text = String::from_utf8_lossy(stdout);
    let detail = if stderr_text.trim().is_empty() {
        stdout_text.trim().to_owned()
    } else {
        stderr_text.trim().to_owned()
    };
    ProbeError::NonZero {
        path: path.to_path_buf(),
        stage,
        exit_code: status.code(),
        status: render_status(status),
        detail,
    }
}

/// Render an [`ExitStatus`] deterministically across platforms.
fn render_status(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "signal".to_owned(),
    }
}

/// Choose the primary text stream and require it to be UTF-8.
fn stream_text(path: &Path, captured: &Captured, stage: ProbeStage) -> Result<String, ProbeError> {
    let stdout = std::str::from_utf8(&captured.stdout).map_err(|error| ProbeError::Malformed {
        path: path.to_path_buf(),
        stage,
        message: format!("stdout: {error}"),
    })?;
    let stderr = std::str::from_utf8(&captured.stderr).map_err(|error| ProbeError::Malformed {
        path: path.to_path_buf(),
        stage,
        message: format!("stderr: {error}"),
    })?;

    let chosen = if stdout.trim().is_empty() {
        stderr
    } else {
        stdout
    };
    Ok(chosen.trim().to_owned())
}
