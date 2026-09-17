//! llama.cpp lifecycle orchestration (`docs/architecture.md` §4, §5).
//!
//! [`LlamaCppLifecycle`] ties together the Job 5 adapter command builder
//! ([`LlamaCppAdapter::command`]) and the Job 6 process supervisor
//! ([`ManagedProcess`]) with loopback HTTP probes to implement the full
//! lifecycle:
//!
//! 1. **Launch**: Build a shell-free [`CommandSpec`] and spawn via
//!    [`ManagedProcess`] in an isolated process group.
//! 2. **Readiness**: Poll `GET /health` until `{"status":"ok"}`, bounded by
//!    the supervisor's startup deadline.
//! 3. **Identity**: `GET /v1/models` and verify that the expected `--alias`
//!    model key appears in the response.
//! 4. **Inference check**: A minimal `POST /v1/chat/completions` with
//!    `max_tokens: 1` to verify the engine can actually generate.
//! 5. **Unload**: ADR-0002 TERM→grace→KILL shutdown via [`ManagedProcess`].
//!
//! Every failure path ensures the child is cleaned up (shutdown + pipe settle)
//! before returning, so a failed lifecycle never orphans a process.
//!
//! # Security
//!
//! - The child always binds to `127.0.0.1` (loopback only).
//! - All HTTP has bounded I/O timeouts and connect timeouts.
//! - No shell is ever used; argv is passed directly.
//! - Cleanup runs on every failure path, including cancellation.

use std::time::Duration;

use model_serving_domain::error::DomainError;
use model_serving_domain::model::FailureClass;
use model_serving_runtime::{
    CommandSpec, ManagedProcess, ProbeStatus, SupervisorConfig, SupervisorError,
};

use crate::{LOOPBACK_HOST, LaunchContext, LlamaCppAdapter};

/// Configuration for the lifecycle orchestrator.
///
/// Timeouts for post-readiness HTTP checks are separate from the supervisor's
/// startup and probe deadlines, because these checks run after the child is
/// already serving (or should be).
#[derive(Debug, Clone)]
pub struct LifecycleConfig {
    /// Supervisor configuration (startup deadline, pipe buffers, shutdown).
    pub supervisor: SupervisorConfig,
    /// Per-request timeout for HTTP checks (identity, inference).
    pub http_timeout: Duration,
    /// Connect timeout for the HTTP client.
    pub http_connect_timeout: Duration,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            supervisor: SupervisorConfig::default(),
            http_timeout: Duration::from_secs(10),
            http_connect_timeout: Duration::from_secs(2),
        }
    }
}

/// Outcome of a successful load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadResult {
    /// Direct child PID.
    pub pid: u32,
    /// Loopback port the child is serving on.
    pub port: u16,
    /// The model key the child is serving (verified via `/v1/models`).
    pub model_key: String,
}

/// Errors from lifecycle operations.
///
/// Every variant maps to a domain [`FailureClass`] via
/// [`LifecycleError::failure_class`], so the caller can store a structured
/// failure on the instance without knowing the lifecycle internals.
#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    /// The [`CommandSpec`] could not be built (validation failure).
    #[error("failed to build launch command: {0}")]
    Command(#[source] DomainError),
    /// The process supervisor reported a lifecycle failure.
    #[error("supervisor error: {0}")]
    Supervisor(#[source] SupervisorError),
    /// The `/health` endpoint did not return `{"status":"ok"}` before the
    /// startup deadline. The child has already been shut down.
    #[error("health check failed: {0}")]
    HealthCheck(String),
    /// The `/v1/models` response did not contain the expected model key.
    #[error("identity mismatch: expected model key {expected:?}, got {got:?}")]
    IdentityMismatch {
        /// The model key we expected (`--alias`).
        expected: String,
        /// The model IDs found in the `/v1/models` response.
        got: Vec<String>,
    },
    /// The minimal inference check failed.
    #[error("inference check failed: {0}")]
    InferenceCheck(String),
    /// An HTTP request to the child loopback endpoint failed.
    #[error("http error communicating with child: {0}")]
    Http(String),
}

impl LifecycleError {
    /// Map this error to the domain failure catalog for structured storage.
    #[must_use]
    pub fn failure_class(&self) -> FailureClass {
        match self {
            Self::Command(error) => {
                if error.code == model_serving_domain::error::ErrorCode::InvalidModel {
                    FailureClass::InvalidModel
                } else {
                    FailureClass::ConfigError
                }
            }
            Self::Supervisor(error) => supervisor_failure_class(error),
            Self::HealthCheck(_) => FailureClass::StartupTimeout,
            Self::IdentityMismatch { .. } => FailureClass::ConfigError,
            Self::InferenceCheck(_) | Self::Http(_) => FailureClass::Unknown,
        }
    }

    /// A human-readable message for storing on `InstanceFailure`.
    #[must_use]
    pub fn message(&self) -> String {
        self.to_string()
    }
}

/// Map a supervisor error to its most appropriate domain failure class.
fn supervisor_failure_class(error: &SupervisorError) -> FailureClass {
    match error {
        SupervisorError::ReadinessTimeout => FailureClass::StartupTimeout,
        SupervisorError::ExitedBeforeReady | SupervisorError::KillTimeout => {
            FailureClass::ProcessCrash
        }
        _ => FailureClass::Unknown,
    }
}

/// Owns and orchestrates one llama.cpp engine lifecycle.
///
/// Created via [`LlamaCppLifecycle::launch`]; the caller drives readiness,
/// identity, inference, and unload explicitly. Every public method that can
/// fail ensures the child is cleaned up on failure.
#[derive(Debug)]
pub struct LlamaCppLifecycle {
    process: ManagedProcess,
    port: u16,
    model_key: String,
    config: LifecycleConfig,
}

impl LlamaCppLifecycle {
    /// Build the command, spawn the child, and return the lifecycle handle.
    ///
    /// The child is launched immediately but is not yet ready. Call
    /// [`wait_ready`](Self::wait_ready) to poll the health endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError::Command`] if argv validation fails, or
    /// [`LifecycleError::Supervisor`] if the spawn fails.
    pub fn launch(
        ctx: &LaunchContext<'_>,
        config: LifecycleConfig,
    ) -> Result<Self, LifecycleError> {
        let spec = LlamaCppAdapter::command(ctx).map_err(LifecycleError::Command)?;
        let process =
            ManagedProcess::spawn(&spec, config.supervisor).map_err(LifecycleError::Supervisor)?;
        Ok(Self {
            process,
            port: ctx.port,
            model_key: ctx.model.key.clone(),
            config,
        })
    }

    /// Spawn from an already-built [`CommandSpec`]. Used when the caller has
    /// already validated and built the command (e.g. tests with a fake binary).
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError::Supervisor`] if the spawn fails.
    pub fn launch_from_spec(
        spec: &CommandSpec,
        port: u16,
        model_key: String,
        config: LifecycleConfig,
    ) -> Result<Self, LifecycleError> {
        let process =
            ManagedProcess::spawn(spec, config.supervisor).map_err(LifecycleError::Supervisor)?;
        Ok(Self {
            process,
            port,
            model_key,
            config,
        })
    }

    /// Direct child PID.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.process.pid()
    }

    /// Loopback port the child was told to bind to.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The model key (`--alias`) the child should be serving.
    #[must_use]
    pub fn model_key(&self) -> &str {
        &self.model_key
    }

    /// Poll `GET /health` until `{"status":"ok"}`, bounded by the supervisor's
    /// startup deadline.
    ///
    /// On failure (timeout or child exit), the child is shut down and the
    /// error reflects the cause.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError::Supervisor`] wrapping the underlying
    /// readiness timeout or early-exit error.
    pub async fn wait_ready(&mut self) -> Result<(), LifecycleError> {
        let port = self.port;
        let probe_timeout = self.config.supervisor.probe_timeout;
        let connect_timeout = self.config.http_connect_timeout;
        self.process
            .wait_ready(|| health_probe(port, probe_timeout, connect_timeout))
            .await
            .map_err(LifecycleError::Supervisor)
    }

    /// After readiness, verify that `/v1/models` contains the expected
    /// model key.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError::IdentityMismatch`] when the key is absent,
    /// or [`LifecycleError::Http`] on connection/parse failure. On any error,
    /// the child is shut down.
    pub async fn verify_identity(&mut self) -> Result<(), LifecycleError> {
        let url = format!("http://{LOOPBACK_HOST}:{}/v1/models", self.port);
        let client = match build_http_client(&self.config) {
            Ok(client) => client,
            Err(error) => {
                self.shutdown_on_error().await;
                return Err(error);
            }
        };
        let response = match client.get(&url).send().await {
            Ok(response) => response,
            Err(error) => {
                self.shutdown_on_error().await;
                return Err(LifecycleError::Http(error.to_string()));
            }
        };
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let error = LifecycleError::Http(format!("/v1/models returned HTTP {status}: {body}"));
            self.shutdown_on_error().await;
            return Err(error);
        }
        let body: serde_json::Value = match response.json().await {
            Ok(body) => body,
            Err(error) => {
                self.shutdown_on_error().await;
                return Err(LifecycleError::Http(error.to_string()));
            }
        };
        let model_ids: Vec<String> = body
            .get("data")
            .and_then(serde_json::Value::as_array)
            .map(|data| {
                data.iter()
                    .filter_map(|entry| entry.get("id"))
                    .filter_map(serde_json::Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        if model_ids.iter().any(|id| id == &self.model_key) {
            Ok(())
        } else {
            let error = LifecycleError::IdentityMismatch {
                expected: self.model_key.clone(),
                got: model_ids,
            };
            self.shutdown_on_error().await;
            Err(error)
        }
    }

    /// After identity verification, run a minimal inference check: send a
    /// `POST /v1/chat/completions` with `max_tokens: 1`, parse the `OpenAI`
    /// response JSON, and verify non-empty completion content.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError::InferenceCheck`] if the request fails, returns
    /// a non-success HTTP status, produces malformed JSON, has an empty
    /// `choices` array, or has empty completion content. On any error, the
    /// child is shut down.
    pub async fn verify_inference(&mut self) -> Result<(), LifecycleError> {
        let url = format!("http://{LOOPBACK_HOST}:{}/v1/chat/completions", self.port);
        let client = match build_http_client(&self.config) {
            Ok(client) => client,
            Err(error) => {
                self.shutdown_on_error().await;
                return Err(error);
            }
        };
        let body = serde_json::json!({
            "model": self.model_key,
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1,
        });
        let response = match client.post(&url).json(&body).send().await {
            Ok(response) => response,
            Err(error) => {
                self.shutdown_on_error().await;
                return Err(LifecycleError::InferenceCheck(error.to_string()));
            }
        };
        if !response.status().is_success() {
            let status = response.status();
            let response_body = response.text().await.unwrap_or_default();
            let error = LifecycleError::InferenceCheck(format!(
                "/v1/chat/completions returned HTTP {status}: {response_body}"
            ));
            self.shutdown_on_error().await;
            return Err(error);
        }

        let response_text = match response.text().await {
            Ok(text) => text,
            Err(error) => {
                self.shutdown_on_error().await;
                return Err(LifecycleError::InferenceCheck(format!(
                    "failed to read /v1/chat/completions response body: {error}"
                )));
            }
        };

        let body: serde_json::Value = match serde_json::from_str(&response_text) {
            Ok(body) => body,
            Err(error) => {
                self.shutdown_on_error().await;
                return Err(LifecycleError::InferenceCheck(format!(
                    "malformed JSON in /v1/chat/completions response: {error}; body: {response_text}"
                )));
            }
        };

        let Some(choices) = body.get("choices").and_then(serde_json::Value::as_array) else {
            let error = LifecycleError::InferenceCheck(
                "missing or non-array 'choices' in /v1/chat/completions response".to_string(),
            );
            self.shutdown_on_error().await;
            return Err(error);
        };

        if choices.is_empty() {
            let error = LifecycleError::InferenceCheck(
                "empty 'choices' in /v1/chat/completions response".to_string(),
            );
            self.shutdown_on_error().await;
            return Err(error);
        }

        let has_non_empty_content = choices.iter().any(|choice| {
            let content = choice
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(serde_json::Value::as_str)
                .or_else(|| choice.get("text").and_then(serde_json::Value::as_str));
            content.is_some_and(|c| !c.trim().is_empty())
        });

        if !has_non_empty_content {
            let error = LifecycleError::InferenceCheck(
                "empty completion content in /v1/chat/completions response".to_string(),
            );
            self.shutdown_on_error().await;
            return Err(error);
        }

        Ok(())
    }

    /// Shut down the engine child cleanly (ADR-0002: TERM→grace→KILL).
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError::Supervisor`] if the shutdown sequence itself
    /// fails (e.g. the child survives KILL).
    pub async fn unload(&mut self) -> Result<(), LifecycleError> {
        self.process
            .shutdown()
            .await
            .map_err(LifecycleError::Supervisor)?;
        Ok(())
    }

    /// Run the complete lifecycle: readiness → identity → unload.
    /// Returns a [`LoadResult`] on success.
    ///
    /// The inference check is NOT run here — it is opt-in because it requires
    /// a real model. Use [`verify_inference`](Self::verify_inference) after
    /// this method if a real inference check is desired.
    ///
    /// # Errors
    ///
    /// Returns the first [`LifecycleError`] encountered.
    pub async fn wait_ready_and_verify(&mut self) -> Result<LoadResult, LifecycleError> {
        self.wait_ready().await?;
        self.verify_identity().await?;
        Ok(LoadResult {
            pid: self.process.pid(),
            port: self.port,
            model_key: self.model_key.clone(),
        })
    }

    /// Classify the child's exit using the adapter's exit classification logic
    /// with the captured exit status and stderr tail.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError::Supervisor`] if the status query fails.
    pub fn classify_exit(&mut self) -> Result<Option<FailureClass>, LifecycleError> {
        let status = self
            .process
            .try_wait()
            .map_err(LifecycleError::Supervisor)?;
        let Some(status) = status else {
            return Ok(None);
        };
        let stderr_tail = self.process.logs().stderr_tail();
        let stderr = String::from_utf8_lossy(&stderr_tail);
        Ok(Some(LlamaCppAdapter::classify_exit(status, &stderr)))
    }

    /// Classify a [`LifecycleError`] into a domain [`FailureClass`], inspecting
    /// child stderr if the failure was an early process exit.
    #[must_use]
    pub fn classify_lifecycle_error(&self, error: &LifecycleError) -> FailureClass {
        match error {
            LifecycleError::Command(domain_error) => {
                if domain_error.code == model_serving_domain::error::ErrorCode::InvalidModel {
                    FailureClass::InvalidModel
                } else {
                    FailureClass::ConfigError
                }
            }
            LifecycleError::Supervisor(SupervisorError::ReadinessTimeout)
            | LifecycleError::HealthCheck(_) => FailureClass::StartupTimeout,
            LifecycleError::Supervisor(SupervisorError::ExitedBeforeReady) => {
                let stderr_tail = self.process.logs().stderr_tail();
                let stderr = String::from_utf8_lossy(&stderr_tail);
                LlamaCppAdapter::classify_stderr(&stderr)
            }
            LifecycleError::Supervisor(SupervisorError::KillTimeout) => FailureClass::ProcessCrash,
            LifecycleError::IdentityMismatch { .. } => FailureClass::ConfigError,
            LifecycleError::Supervisor(_)
            | LifecycleError::InferenceCheck(_)
            | LifecycleError::Http(_) => FailureClass::Unknown,
        }
    }

    /// Stdout tail from the child, for diagnostic storage.
    #[must_use]
    pub fn stdout_tail(&self) -> Vec<u8> {
        self.process.logs().stdout_tail()
    }

    /// Stderr tail from the child, for diagnostic storage.
    #[must_use]
    pub fn stderr_tail(&self) -> Vec<u8> {
        self.process.logs().stderr_tail()
    }

    /// Best-effort shutdown, ignoring errors. Used on error paths.
    async fn shutdown_on_error(&mut self) {
        let _ = self.process.shutdown().await;
    }
}

/// Build a `reqwest::Client` with bounded timeouts, loopback-only.
fn build_http_client(config: &LifecycleConfig) -> Result<reqwest::Client, LifecycleError> {
    reqwest::Client::builder()
        .timeout(config.http_timeout)
        .connect_timeout(config.http_connect_timeout)
        // Disable connection pooling to avoid stale connections after unload.
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|error| LifecycleError::Http(error.to_string()))
}

/// Single health probe: `GET /health` → `ProbeStatus::Ready` when
/// `{"status":"ok"}`.
async fn health_probe(
    port: u16,
    timeout: Duration,
    connect_timeout: Duration,
) -> Result<ProbeStatus, String> {
    let url = format!("http://{LOOPBACK_HOST}:{port}/health");
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(connect_timeout)
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Ok(ProbeStatus::Pending);
    }
    let body: serde_json::Value = response.json().await.map_err(|error| error.to_string())?;
    if body.get("status").and_then(serde_json::Value::as_str) == Some("ok") {
        Ok(ProbeStatus::Ready)
    } else {
        Ok(ProbeStatus::Pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_error_maps_to_failure_class() {
        assert_eq!(
            LifecycleError::Command(DomainError::from(
                model_serving_domain::error::ErrorCode::InvalidModel
            ))
            .failure_class(),
            FailureClass::InvalidModel
        );
        assert_eq!(
            LifecycleError::Command(DomainError::from(
                model_serving_domain::error::ErrorCode::UnsupportedField
            ))
            .failure_class(),
            FailureClass::ConfigError
        );
        assert_eq!(
            LifecycleError::Supervisor(SupervisorError::ReadinessTimeout).failure_class(),
            FailureClass::StartupTimeout
        );
        assert_eq!(
            LifecycleError::Supervisor(SupervisorError::ExitedBeforeReady).failure_class(),
            FailureClass::ProcessCrash
        );
        assert_eq!(
            LifecycleError::IdentityMismatch {
                expected: "a".into(),
                got: vec!["b".into()],
            }
            .failure_class(),
            FailureClass::ConfigError
        );
        assert_eq!(
            LifecycleError::InferenceCheck("boom".into()).failure_class(),
            FailureClass::Unknown
        );
    }

    #[test]
    fn default_config_has_reasonable_timeouts() {
        let config = LifecycleConfig::default();
        assert_eq!(config.http_timeout, Duration::from_secs(10));
        assert_eq!(config.http_connect_timeout, Duration::from_secs(2));
    }
}
