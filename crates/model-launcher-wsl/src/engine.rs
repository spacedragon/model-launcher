use crate::{
    ExecutableIdentity, PortAllocator, ProbeSnapshot, Signal, capture_version, launch_argv,
    parse_identity, probe_argv, signal_argv, stat_argv, windows_to_wsl_path,
};
use async_trait::async_trait;
use model_launcher_core::{
    AppError, EngineCapabilities, EngineFuture, EngineLogFramer, EngineProcess, EngineSpec,
    InferenceEngine, LaunchSettings, LogLevel, LogSource, LogStore, MAX_ENGINE_LOG_LINE_BYTES,
    ModelId, ModelRecord,
};
use std::{
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, ChildStderr, ChildStdout, Command},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}
#[derive(Debug, Error)]
pub enum WslError {
    #[error("command failed: {0}")]
    Command(String),
    #[error("invalid model path: {0}")]
    Path(String),
    #[error("process spawning is unsupported by this runner")]
    SpawnUnsupported,
    #[error("internal port is already in use")]
    AddressInUse,
    #[error("internal endpoint is not owned by the launched process")]
    NonOwnedEndpoint,
}

#[async_trait]
pub trait WslChild: Send {
    async fn pid_control_line(&mut self) -> Result<String, WslError>;
    async fn wait_ready(&mut self, timeout: Duration) -> Result<(), WslError>;
    async fn check_health(&mut self) -> Result<(), WslError>;
    async fn endpoint_responding(&mut self) -> bool {
        self.check_health().await.is_ok()
    }
    async fn wait_for_exit(&mut self) -> Result<i32, WslError>;
}

/// Implementations must make `spawn` cancellation-safe: a dropped future retains ownership and
/// terminates any child it created until the returned `WslChild` transfers that ownership.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn output(&self, program: &str, argv: &[String]) -> Result<CommandOutput, WslError>;
    async fn spawn(&self, _program: &str, _argv: &[String]) -> Result<Box<dyn WslChild>, WslError> {
        Err(WslError::SpawnUnsupported)
    }
}

#[derive(Default)]
pub struct TokioCommandRunner {
    logs: Option<(LogStore, Option<u64>, Option<ModelId>)>,
}
impl TokioCommandRunner {
    #[must_use]
    pub fn with_log_store(
        store: LogStore,
        generation: Option<u64>,
        model_id: Option<ModelId>,
    ) -> Self {
        Self {
            logs: Some((store, generation, model_id)),
        }
    }
}

#[async_trait]
impl CommandRunner for TokioCommandRunner {
    async fn output(&self, program: &str, argv: &[String]) -> Result<CommandOutput, WslError> {
        let output = Command::new(program)
            .args(argv)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| WslError::Command(error.to_string()))?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
    async fn spawn(&self, program: &str, argv: &[String]) -> Result<Box<dyn WslChild>, WslError> {
        let port = argv
            .windows(2)
            .find(|pair| pair[0] == "--port")
            .and_then(|pair| pair[1].parse().ok())
            .ok_or_else(|| WslError::Command("missing internal port".into()))?;
        let distribution = argv
            .get(1)
            .cloned()
            .ok_or_else(|| WslError::Command("missing distribution".into()))?;
        let mut child = Command::new(program)
            .args(argv)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|error| WslError::Command(error.to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| WslError::Command("missing stdout pipe".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| WslError::Command("missing stderr pipe".into()))?;
        Ok(Box::new(TokioWslChild {
            child,
            stdout: Some(BufReader::new(stdout)),
            stderr: Some(stderr),
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            logs: self.logs.clone(),
            drains: Vec::new(),
            distribution,
            linux_pid: None,
        }))
    }
}

struct TokioWslChild {
    child: Child,
    stdout: Option<BufReader<ChildStdout>>,
    stderr: Option<ChildStderr>,
    addr: SocketAddr,
    logs: Option<(LogStore, Option<u64>, Option<ModelId>)>,
    drains: Vec<tokio::task::JoinHandle<()>>,
    distribution: String,
    linux_pid: Option<u32>,
}
#[async_trait]
impl WslChild for TokioWslChild {
    async fn pid_control_line(&mut self) -> Result<String, WslError> {
        let mut line = String::new();
        let count = self
            .stdout
            .as_mut()
            .ok_or_else(|| WslError::Command("stdout unavailable".into()))?
            .read_line(&mut line)
            .await
            .map_err(|e| WslError::Command(e.to_string()))?;
        if count == 0 {
            return Err(WslError::Command("missing PID control line".into()));
        }
        self.linux_pid = Some(
            crate::parse_pid_control_line(&line)
                .map_err(|error| WslError::Command(error.to_string()))?,
        );
        if let Some(mut stdout) = self.stdout.take() {
            let logs = self.logs.clone();
            self.drains.push(tokio::spawn(async move {
                drain_stream(&mut stdout, logs, LogSource::EngineStdout, LogLevel::Info).await;
            }));
        }
        if let Some(mut stderr) = self.stderr.take() {
            let logs = self.logs.clone();
            self.drains.push(tokio::spawn(async move {
                drain_stream(&mut stderr, logs, LogSource::EngineStderr, LogLevel::Error).await;
            }));
        }
        Ok(line)
    }
    async fn wait_ready(&mut self, timeout: Duration) -> Result<(), WslError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|e| WslError::Command(e.to_string()))?
            {
                return Err(WslError::Command(format!(
                    "engine exited before readiness: {status}"
                )));
            }
            if tokio::net::TcpStream::connect(self.addr).await.is_ok() {
                return if self.endpoint_is_owned().await? {
                    Ok(())
                } else {
                    Err(WslError::NonOwnedEndpoint)
                };
            }
            if Instant::now() >= deadline {
                return Err(WslError::Command("readiness timed out".into()));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    async fn check_health(&mut self) -> Result<(), WslError> {
        tokio::net::TcpStream::connect(self.addr)
            .await
            .map_err(|e| WslError::Command(e.to_string()))?;
        if self.endpoint_is_owned().await? {
            Ok(())
        } else {
            Err(WslError::NonOwnedEndpoint)
        }
    }
    async fn endpoint_responding(&mut self) -> bool {
        tokio::net::TcpStream::connect(self.addr).await.is_ok()
    }
    async fn wait_for_exit(&mut self) -> Result<i32, WslError> {
        let status = self
            .child
            .wait()
            .await
            .map_err(|e| WslError::Command(e.to_string()))?;
        for drain in self.drains.drain(..) {
            let _ = tokio::time::timeout(Duration::from_secs(1), drain).await;
        }
        Ok(status.code().unwrap_or(-1))
    }
}

impl TokioWslChild {
    async fn endpoint_is_owned(&self) -> Result<bool, WslError> {
        let pid = self
            .linux_pid
            .ok_or_else(|| WslError::Command("PID handshake not established".into()))?;
        let port = format!(":{}", self.addr.port());
        let output = Command::new("wsl.exe")
            .args([
                "-d",
                &self.distribution,
                "--",
                "ss",
                "-H",
                "-ltnp",
                "sport",
                "=",
                &port,
            ])
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| WslError::Command(error.to_string()))?;
        if !output.status.success() {
            return Err(WslError::Command(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        let needle = format!("pid={pid},");
        Ok(String::from_utf8_lossy(&output.stdout).contains(&needle))
    }
}

async fn drain_stream<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    logs: Option<(LogStore, Option<u64>, Option<ModelId>)>,
    source: LogSource,
    level: LogLevel,
) {
    let Some((store, generation, model_id)) = logs else {
        let mut sink = tokio::io::sink();
        let _ = tokio::io::copy(reader, &mut sink).await;
        return;
    };
    let Ok(mut framer) = EngineLogFramer::new(
        store,
        source,
        level,
        generation,
        model_id,
        MAX_ENGINE_LOG_LINE_BYTES,
    ) else {
        return;
    };
    let mut buffer = [0_u8; 8192];
    loop {
        match tokio::io::AsyncReadExt::read(reader, &mut buffer).await {
            Ok(0) => break,
            Ok(count) => framer.push(now_ms(), &buffer[..count]),
            Err(_) => break,
        }
    }
    framer.finish(now_ms());
}
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub struct WslProber {
    runner: Arc<dyn CommandRunner>,
}
impl WslProber {
    #[must_use]
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }
    pub async fn identity(
        &self,
        distribution: &str,
        executable: &str,
    ) -> Result<ExecutableIdentity, WslError> {
        let output = self
            .runner
            .output("wsl.exe", &stat_argv(distribution, executable))
            .await?;
        if !output.success {
            return Err(WslError::Command(output.stderr));
        }
        parse_identity(&output.stdout).map_err(|error| WslError::Command(error.to_string()))
    }
    pub async fn probe(
        &self,
        distribution: &str,
        executable: &str,
        cached: Option<ProbeSnapshot>,
    ) -> Result<ProbeSnapshot, WslError> {
        let identity = self.identity(distribution, executable).await?;
        if let Some(snapshot) =
            cached.filter(|snapshot| snapshot.is_valid_for(distribution, executable, &identity))
        {
            return Ok(snapshot);
        }
        let version = self
            .runner
            .output(
                "wsl.exe",
                &probe_argv(distribution, executable, "--version"),
            )
            .await?;
        let help = self
            .runner
            .output("wsl.exe", &probe_argv(distribution, executable, "--help"))
            .await?;
        if !version.success {
            return Err(WslError::Command(version.stderr));
        }
        if !help.success {
            return Err(WslError::Command(help.stderr));
        }
        Ok(ProbeSnapshot::new(
            distribution,
            executable,
            identity,
            version.stdout,
            help.stdout,
        ))
    }
}

pub struct ProbeCache {
    path: PathBuf,
    prober: WslProber,
}
impl ProbeCache {
    #[must_use]
    pub fn new(path: PathBuf, prober: WslProber) -> Self {
        Self { path, prober }
    }
    /// Revalidates executable identity on every call and atomically changes the durable snapshot
    /// only after a complete version/help reprobe succeeds.
    pub async fn refresh(
        &self,
        distribution: &str,
        executable: &str,
    ) -> Result<ProbeSnapshot, WslError> {
        let cached = ProbeSnapshot::load(&self.path).ok();
        let snapshot = self.prober.probe(distribution, executable, cached).await?;
        let temporary = self.path.with_extension("tmp");
        snapshot
            .save(&temporary)
            .map_err(|e| WslError::Command(e.to_string()))?;
        std::fs::rename(&temporary, &self.path).map_err(|e| WslError::Command(e.to_string()))?;
        Ok(snapshot)
    }
}

pub struct LlamaCppWslEngine {
    distribution: String,
    executable: String,
    runner: Arc<dyn CommandRunner>,
    allocator: Arc<dyn PortAllocator>,
}
impl LlamaCppWslEngine {
    #[must_use]
    pub fn new(
        distribution: impl Into<String>,
        executable: impl Into<String>,
        runner: Arc<dyn CommandRunner>,
    ) -> Self {
        Self {
            distribution: distribution.into(),
            executable: executable.into(),
            runner,
            allocator: Arc::new(crate::InternalPortAllocator),
        }
    }
    #[must_use]
    pub fn with_port_allocator(mut self, allocator: Arc<dyn PortAllocator>) -> Self {
        self.allocator = allocator;
        self
    }
    async fn probe(&self) -> Result<ProbeSnapshot, WslError> {
        WslProber::new(self.runner.clone())
            .probe(&self.distribution, &self.executable, None)
            .await
    }
}
fn app_error(error: impl std::error::Error + Send + Sync + 'static) -> AppError {
    AppError::EngineProcess(Box::new(error))
}

impl InferenceEngine for LlamaCppWslEngine {
    fn spec(&self) -> EngineFuture<'_, EngineSpec> {
        Box::pin(async move {
            let snapshot = self.probe().await.map_err(app_error)?;
            Ok(EngineSpec {
                id: "llama-cpp-wsl".into(),
                display_name: "llama.cpp (WSL)".into(),
                version: capture_version(&snapshot.version_raw),
            })
        })
    }
    fn probe_capabilities(&self) -> EngineFuture<'_, EngineCapabilities> {
        Box::pin(async move { Ok(self.probe().await.map_err(app_error)?.capabilities) })
    }
    fn validate_launch<'a>(
        &'a self,
        model: &'a ModelRecord,
        settings: &'a LaunchSettings,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            let _ = windows_to_wsl_path(
                model
                    .path
                    .to_str()
                    .ok_or_else(|| app_error(io::Error::other("non-Unicode model path")))?,
            )
            .map_err(|e| app_error(io::Error::other(e.to_string())))?;
            let caps = self.probe().await.map_err(app_error)?.capabilities;
            if settings.render(&caps).unsupported.is_empty() {
                Ok(())
            } else {
                Err(AppError::InvalidSetting("unsupported_by_engine"))
            }
        })
    }
    fn spawn<'a>(
        &'a self,
        model: &'a ModelRecord,
        settings: &'a LaunchSettings,
    ) -> EngineFuture<'a, Box<dyn EngineProcess>> {
        Box::pin(async move {
            let snapshot = self.probe().await.map_err(app_error)?;
            let rendered = settings.render(&snapshot.capabilities);
            if !rendered.unsupported.is_empty() {
                return Err(AppError::InvalidSetting("unsupported_by_engine"));
            }
            let model_path = windows_to_wsl_path(
                model
                    .path
                    .to_str()
                    .ok_or_else(|| app_error(io::Error::other("non-Unicode model path")))?,
            )
            .map_err(|e| app_error(io::Error::other(e.to_string())))?;
            let reservation = self.allocator.reserve().map_err(app_error)?;
            let port = reservation.addr().port();
            let mut args = vec![
                "--host".into(),
                "127.0.0.1".into(),
                "--port".into(),
                port.to_string(),
            ];
            args.extend(rendered.args);
            let _released = reservation.release();
            let argv = launch_argv(&self.distribution, &self.executable, &model_path, &args);
            let mut child = self
                .runner
                .spawn("wsl.exe", &argv)
                .await
                .map_err(app_error)?;
            let line = child.pid_control_line().await.map_err(app_error)?;
            let pid = crate::parse_pid_control_line(&line).map_err(app_error)?;
            Ok(Box::new(WslEngineProcess {
                distribution: self.distribution.clone(),
                pid,
                runner: self.runner.clone(),
                child,
                retry: Some(RetryContext {
                    executable: self.executable.clone(),
                    model_path,
                    args,
                    allocator: self.allocator.clone(),
                }),
                owned_active: true,
            }) as Box<dyn EngineProcess>)
        })
    }
}

struct WslEngineProcess {
    distribution: String,
    pid: u32,
    runner: Arc<dyn CommandRunner>,
    child: Box<dyn WslChild>,
    retry: Option<RetryContext>,
    owned_active: bool,
}
struct RetryContext {
    executable: String,
    model_path: String,
    args: Vec<String>,
    allocator: Arc<dyn PortAllocator>,
}
impl EngineProcess for WslEngineProcess {
    fn wait_ready(&mut self, timeout: Duration) -> EngineFuture<'_, ()> {
        Box::pin(async move {
            let deadline = Instant::now() + timeout;
            for attempt in 1..=3 {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match self.child.wait_ready(remaining).await {
                    Ok(()) => {
                        self.retry = None;
                        return Ok(());
                    }
                    Err(WslError::AddressInUse | WslError::NonOwnedEndpoint) if attempt < 3 => {
                        self.stop_attempt().await?;
                        self.spawn_retry().await?;
                    }
                    Err(error) => {
                        self.stop_attempt().await?;
                        return Err(app_error(error));
                    }
                }
            }
            Err(app_error(WslError::AddressInUse))
        })
    }
    fn check_health(&mut self) -> EngineFuture<'_, ()> {
        Box::pin(async move { self.child.check_health().await.map_err(app_error) })
    }
    fn graceful_shutdown(&mut self) -> EngineFuture<'_, ()> {
        Box::pin(async move {
            let out = self
                .runner
                .output(
                    "wsl.exe",
                    &signal_argv(&self.distribution, self.pid, Signal::Term),
                )
                .await
                .map_err(app_error)?;
            if out.success {
                Ok(())
            } else {
                Err(app_error(WslError::Command(out.stderr)))
            }
        })
    }
    fn force_shutdown(&mut self) -> EngineFuture<'_, ()> {
        Box::pin(async move {
            let out = self
                .runner
                .output(
                    "wsl.exe",
                    &signal_argv(&self.distribution, self.pid, Signal::Kill),
                )
                .await
                .map_err(app_error)?;
            if out.success {
                Ok(())
            } else {
                Err(app_error(WslError::Command(out.stderr)))
            }
        })
    }
    fn wait_for_exit(&mut self) -> EngineFuture<'_, i32> {
        Box::pin(async move {
            let code = self.child.wait_for_exit().await.map_err(app_error)?;
            let deadline = Instant::now() + Duration::from_secs(1);
            while self.child.endpoint_responding().await {
                if Instant::now() >= deadline {
                    return Err(app_error(WslError::Command(
                        "old endpoint remained live after process exit".into(),
                    )));
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            self.owned_active = false;
            Ok(code)
        })
    }
}

impl WslEngineProcess {
    async fn stop_attempt(&mut self) -> Result<(), AppError> {
        let term = self
            .runner
            .output(
                "wsl.exe",
                &signal_argv(&self.distribution, self.pid, Signal::Term),
            )
            .await
            .map_err(app_error)?;
        if !term.success {
            return Err(app_error(WslError::Command(term.stderr)));
        }
        if tokio::time::timeout(Duration::from_millis(250), self.child.wait_for_exit())
            .await
            .is_err()
        {
            let kill = self
                .runner
                .output(
                    "wsl.exe",
                    &signal_argv(&self.distribution, self.pid, Signal::Kill),
                )
                .await
                .map_err(app_error)?;
            if !kill.success {
                return Err(app_error(WslError::Command(kill.stderr)));
            }
            tokio::time::timeout(Duration::from_secs(1), self.child.wait_for_exit())
                .await
                .map_err(|_| app_error(WslError::Command("owned process did not exit".into())))?
                .map_err(app_error)?;
        }
        let endpoint_deadline = Instant::now() + Duration::from_millis(250);
        while self.child.endpoint_responding().await {
            if Instant::now() >= endpoint_deadline {
                return Err(app_error(WslError::Command(
                    "owned endpoint remained live".into(),
                )));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.owned_active = false;
        Ok(())
    }
    async fn spawn_retry(&mut self) -> Result<(), AppError> {
        let retry = self
            .retry
            .as_ref()
            .ok_or_else(|| app_error(WslError::Command("retry context unavailable".into())))?;
        let reservation = retry.allocator.reserve().map_err(app_error)?;
        let port = reservation.addr().port();
        let mut args = retry.args.clone();
        if let Some(value) = args.windows(2).position(|pair| pair[0] == "--port") {
            args[value + 1] = port.to_string();
        }
        let _released = reservation.release();
        let argv = launch_argv(
            &self.distribution,
            &retry.executable,
            &retry.model_path,
            &args,
        );
        let mut child = self
            .runner
            .spawn("wsl.exe", &argv)
            .await
            .map_err(app_error)?;
        let line = child.pid_control_line().await.map_err(app_error)?;
        let pid = crate::parse_pid_control_line(&line).map_err(app_error)?;
        self.child = child;
        self.pid = pid;
        self.owned_active = true;
        Ok(())
    }
}

impl Drop for WslEngineProcess {
    fn drop(&mut self) {
        if !self.owned_active {
            return;
        }
        let runner = self.runner.clone();
        let distribution = self.distribution.clone();
        let pid = self.pid;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = runner
                    .output("wsl.exe", &signal_argv(&distribution, pid, Signal::Term))
                    .await;
                tokio::time::sleep(Duration::from_millis(250)).await;
                let _ = runner
                    .output("wsl.exe", &signal_argv(&distribution, pid, Signal::Kill))
                    .await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::{collections::VecDeque, net::SocketAddr};

    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<Vec<String>>>,
        fail_help: AtomicBool,
    }
    #[async_trait]
    impl CommandRunner for FakeRunner {
        async fn output(&self, program: &str, argv: &[String]) -> Result<CommandOutput, WslError> {
            assert_eq!(program, "wsl.exe");
            self.calls.lock().unwrap().push(argv.to_vec());
            let stdout = if argv.contains(&"stat".into()) {
                "1\t2\t3\t4\n"
            } else if argv.last().unwrap() == "--version" {
                "llama v1\n"
            } else {
                "--ctx-size --threads\n"
            };
            let failed = argv.last().unwrap() == "--help" && self.fail_help.load(Ordering::SeqCst);
            Ok(CommandOutput {
                success: !failed,
                stdout: stdout.into(),
                stderr: if failed {
                    "help failed".into()
                } else {
                    String::new()
                },
            })
        }
    }

    #[tokio::test]
    async fn prober_revalidates_identity_and_reuses_only_valid_snapshot() {
        let runner = Arc::new(FakeRunner::default());
        let prober = WslProber::new(runner.clone());
        let first = prober.probe("Ubuntu", "/bin/llama", None).await.unwrap();
        assert_eq!(runner.calls.lock().unwrap().len(), 3);
        let second = prober
            .probe("Ubuntu", "/bin/llama", Some(first.clone()))
            .await
            .unwrap();
        assert_eq!(second, first);
        assert_eq!(
            runner.calls.lock().unwrap().len(),
            4,
            "cache hit still stats executable"
        );
    }

    #[tokio::test]
    async fn failed_reprobe_preserves_previous_snapshot_on_disk() {
        let runner = Arc::new(FakeRunner::default());
        let cache_path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        let cache = ProbeCache::new(cache_path.to_path_buf(), WslProber::new(runner.clone()));
        let saved = cache.refresh("Ubuntu", "/bin/llama").await.unwrap();
        runner.fail_help.store(true, Ordering::SeqCst);
        let error = cache
            .refresh("Ubuntu", "/different/llama")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("help failed"));
        assert_eq!(ProbeSnapshot::load(&cache_path).unwrap(), saved);
    }

    #[test]
    fn inference_engine_is_object_safe() {
        fn accepts(_: &dyn model_launcher_core::InferenceEngine) {}
        let engine =
            LlamaCppWslEngine::new("Ubuntu", "/bin/llama", Arc::new(FakeRunner::default()));
        accepts(&engine);
    }

    struct FakeLease(u16);
    impl crate::PortLease for FakeLease {
        fn addr(&self) -> SocketAddr {
            SocketAddr::from(([127, 0, 0, 1], self.0))
        }
        fn release(self: Box<Self>) -> SocketAddr {
            self.addr()
        }
    }
    struct FakeAllocator(Mutex<VecDeque<u16>>);
    impl PortAllocator for FakeAllocator {
        fn reserve(&self) -> io::Result<Box<dyn crate::PortLease>> {
            Ok(Box::new(FakeLease(
                self.0.lock().unwrap().pop_front().unwrap(),
            )))
        }
    }
    struct AttemptChild {
        pid: u32,
        ready: Option<Result<(), WslError>>,
    }
    #[async_trait]
    impl WslChild for AttemptChild {
        async fn pid_control_line(&mut self) -> Result<String, WslError> {
            Ok(format!("MODEL_LAUNCHER_PID={}\n", self.pid))
        }
        async fn wait_ready(&mut self, _: Duration) -> Result<(), WslError> {
            self.ready.take().unwrap()
        }
        async fn check_health(&mut self) -> Result<(), WslError> {
            Err(WslError::Command("down".into()))
        }
        async fn wait_for_exit(&mut self) -> Result<i32, WslError> {
            Ok(0)
        }
    }
    struct AttemptRunner {
        children: Mutex<VecDeque<AttemptChild>>,
        signals: Mutex<Vec<Vec<String>>>,
    }
    #[async_trait]
    impl CommandRunner for AttemptRunner {
        async fn output(&self, _: &str, argv: &[String]) -> Result<CommandOutput, WslError> {
            self.signals.lock().unwrap().push(argv.to_vec());
            Ok(CommandOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        async fn spawn(&self, _: &str, _: &[String]) -> Result<Box<dyn WslChild>, WslError> {
            Ok(Box::new(self.children.lock().unwrap().pop_front().unwrap()))
        }
    }

    #[tokio::test]
    async fn stolen_port_terminates_owned_pid_then_retries_with_fresh_port() {
        let runner = Arc::new(AttemptRunner {
            children: Mutex::new(VecDeque::from([AttemptChild {
                pid: 22,
                ready: Some(Ok(())),
            }])),
            signals: Mutex::new(Vec::new()),
        });
        let allocator = Arc::new(FakeAllocator(Mutex::new(VecDeque::from([2002]))));
        let mut process = WslEngineProcess {
            distribution: "Ubuntu".into(),
            pid: 11,
            runner: runner.clone(),
            child: Box::new(AttemptChild {
                pid: 11,
                ready: Some(Err(WslError::NonOwnedEndpoint)),
            }),
            retry: Some(RetryContext {
                executable: "/llama".into(),
                model_path: "/model".into(),
                args: vec!["--port".into(), "2001".into()],
                allocator,
            }),
            owned_active: true,
        };
        process.wait_ready(Duration::from_secs(1)).await.unwrap();
        let signals = runner.signals.lock().unwrap();
        assert!(signals.iter().any(|argv| argv.ends_with(&[
            "-TERM".into(),
            "--".into(),
            "11".into()
        ])));
        assert_eq!(process.pid, 22);
    }

    #[tokio::test]
    async fn three_stolen_ports_exhaust_and_terminate_each_owned_pid_only() {
        let runner = Arc::new(AttemptRunner {
            children: Mutex::new(VecDeque::from([
                AttemptChild {
                    pid: 22,
                    ready: Some(Err(WslError::AddressInUse)),
                },
                AttemptChild {
                    pid: 33,
                    ready: Some(Err(WslError::NonOwnedEndpoint)),
                },
            ])),
            signals: Mutex::new(Vec::new()),
        });
        let allocator = Arc::new(FakeAllocator(Mutex::new(VecDeque::from([2002, 2003]))));
        let mut process = WslEngineProcess {
            distribution: "Ubuntu".into(),
            pid: 11,
            runner: runner.clone(),
            child: Box::new(AttemptChild {
                pid: 11,
                ready: Some(Err(WslError::AddressInUse)),
            }),
            retry: Some(RetryContext {
                executable: "/llama".into(),
                model_path: "/model".into(),
                args: vec!["--port".into(), "2001".into()],
                allocator,
            }),
            owned_active: true,
        };
        let error = process
            .wait_ready(Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(format!("{error:?}").contains("NonOwnedEndpoint"));
        let signals = runner.signals.lock().unwrap();
        let killed: Vec<_> = signals
            .iter()
            .filter_map(|argv| argv.last().cloned())
            .collect();
        assert_eq!(killed, ["11", "22", "33"]);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_owned_process_after_pid_handshake_cleans_exact_pid() {
        let runner = Arc::new(AttemptRunner {
            children: Mutex::new(VecDeque::new()),
            signals: Mutex::new(Vec::new()),
        });
        let process = WslEngineProcess {
            distribution: "Ubuntu".into(),
            pid: 77,
            runner: runner.clone(),
            child: Box::new(AttemptChild {
                pid: 77,
                ready: Some(Ok(())),
            }),
            retry: None,
            owned_active: true,
        };
        drop(process);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(251)).await;
        tokio::task::yield_now().await;
        let signals = runner.signals.lock().unwrap();
        assert_eq!(signals.len(), 2);
        assert!(signals[0].ends_with(&["-TERM".into(), "--".into(), "77".into()]));
        assert!(signals[1].ends_with(&["-KILL".into(), "--".into(), "77".into()]));
    }

    struct DelayedEndpointChild {
        checks: usize,
    }
    #[async_trait]
    impl WslChild for DelayedEndpointChild {
        async fn pid_control_line(&mut self) -> Result<String, WslError> {
            Ok("MODEL_LAUNCHER_PID=88\n".into())
        }
        async fn wait_ready(&mut self, _: Duration) -> Result<(), WslError> {
            Ok(())
        }
        async fn check_health(&mut self) -> Result<(), WslError> {
            if self.checks == 0 {
                Err(WslError::Command("down".into()))
            } else {
                self.checks -= 1;
                Ok(())
            }
        }
        async fn wait_for_exit(&mut self) -> Result<i32, WslError> {
            Ok(0)
        }
    }
    #[tokio::test(start_paused = true)]
    async fn wait_for_exit_waits_until_old_health_endpoint_is_down() {
        let runner = Arc::new(AttemptRunner {
            children: Mutex::new(VecDeque::new()),
            signals: Mutex::new(Vec::new()),
        });
        let mut process = WslEngineProcess {
            distribution: "Ubuntu".into(),
            pid: 88,
            runner,
            child: Box::new(DelayedEndpointChild { checks: 2 }),
            retry: None,
            owned_active: true,
        };
        let started = tokio::time::Instant::now();
        process.wait_for_exit().await.unwrap();
        assert!(!process.owned_active);
        assert!(tokio::time::Instant::now().duration_since(started) >= Duration::from_millis(50));
    }
}
