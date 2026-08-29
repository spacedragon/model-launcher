use crate::{
    ExecutableIdentity, ProbeSnapshot, Signal, capture_version, launch_argv, parse_identity,
    probe_argv, signal_argv, stat_argv, windows_to_wsl_path,
};
use async_trait::async_trait;
use model_launcher_core::{
    AppError, EngineCapabilities, EngineFuture, EngineProcess, EngineSpec, InferenceEngine,
    LaunchSettings, ModelRecord,
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
}

#[async_trait]
pub trait WslChild: Send {
    async fn pid_control_line(&mut self) -> Result<String, WslError>;
    async fn wait_ready(&mut self, timeout: Duration) -> Result<(), WslError>;
    async fn check_health(&mut self) -> Result<(), WslError>;
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
pub struct TokioCommandRunner;

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
        }))
    }
}

struct TokioWslChild {
    child: Child,
    stdout: Option<BufReader<ChildStdout>>,
    stderr: Option<ChildStderr>,
    addr: SocketAddr,
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
        // Drain both pipes after the control record so a verbose server cannot block. Applications
        // that need retained logs should inject their own runner and frame these streams to LogStore.
        if let Some(mut stdout) = self.stdout.take() {
            tokio::spawn(async move {
                let mut sink = tokio::io::sink();
                let _ = tokio::io::copy(&mut stdout, &mut sink).await;
            });
        }
        if let Some(mut stderr) = self.stderr.take() {
            tokio::spawn(async move {
                let mut sink = tokio::io::sink();
                let _ = tokio::io::copy(&mut stderr, &mut sink).await;
            });
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
                return Ok(());
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
            .map(|_| ())
            .map_err(|e| WslError::Command(e.to_string()))
    }
    async fn wait_for_exit(&mut self) -> Result<i32, WslError> {
        let status = self
            .child
            .wait()
            .await
            .map_err(|e| WslError::Command(e.to_string()))?;
        Ok(status.code().unwrap_or(-1))
    }
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
        }
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
            let reservation = crate::InternalPortAllocator.reserve().map_err(app_error)?;
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
            }) as Box<dyn EngineProcess>)
        })
    }
}

struct WslEngineProcess {
    distribution: String,
    pid: u32,
    runner: Arc<dyn CommandRunner>,
    child: Box<dyn WslChild>,
}
impl EngineProcess for WslEngineProcess {
    fn wait_ready(&mut self, timeout: Duration) -> EngineFuture<'_, ()> {
        Box::pin(async move { self.child.wait_ready(timeout).await.map_err(app_error) })
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
        Box::pin(async move { self.child.wait_for_exit().await.map_err(app_error) })
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
}
