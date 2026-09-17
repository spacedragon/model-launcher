//! llama.cpp (`llama-server`) runtime adapter.
//!
//! Maps a [`LoadConfig`] to a deterministic, shell-free [`CommandSpec`] and
//! derives [`Capabilities`] from a probed `llama-server` build
//! (`docs/architecture.md` §4). The minimum supported build is the ADR-0003
//! candidate `b5555`; keep this constant and
//! `fixtures/runtimes/manifest.json` in sync.
//!
//! # Determinism
//!
//! [`LlamaCppAdapter::command`] emits the argument vector in one documented
//! order:
//!
//! 1. `--model <path>` — the GGUF artifact path.
//! 2. `--host 127.0.0.1` — always loopback, never administrator-supplied.
//! 3. `--port <port>` — the port reserved by the control plane.
//! 4. `--alias <model.key>` — the identity served by `/v1/models`.
//! 5. `--ctx-size <context_length>`.
//! 6. `--parallel <max_concurrency>` when set.
//! 7. `--batch-size <eval_batch_size>` when set.
//! 8. `--flash-attn` only for `flash_attention == Some(true)`; `Some(false)`
//!    and `None` omit it because the pinned build's default is disabled and the
//!    flag takes no value.
//! 9. `--no-kv-offload` only for `offload_kv_cache_to_gpu == Some(false)`;
//!    KV offload is on by default, so `Some(true)` and `None` omit it.
//! 10. `--n-gpu-layers <n_gpu_layers>` when set.
//! 11. The administrator's `Runtime::fixed_args` verbatim, in order, **last**.
//!     Appending last is the documented, deterministic location so a later
//!     flag can override an earlier default (llama.cpp's last-wins parsing).
//!
//! Every element is pushed as a separate `OsString`; the executable and its
//! arguments never pass through a shell.

use std::path::Path;

use model_serving_domain::error::{DomainError, ErrorCode, Result as DomainResult};
use model_serving_domain::model::{
    ArtifactKind, Capabilities, LoadConfig, Model, Runtime, RuntimeKind,
};
use model_serving_runtime::{
    CommandSpec, DoctorReport, ProbeConfig, ProbeError, ProbeSnapshot, diagnose_with,
    fixed_arg_collision,
};

pub mod lifecycle;

pub use lifecycle::{LifecycleConfig, LifecycleError, LlamaCppLifecycle, LoadResult};

/// Minimum supported `llama.cpp` build number (`ADR-0003` candidate `b5555`).
pub const MIN_SUPPORTED_BUILD: u32 = 5555;

/// Loopback host every engine binds to. Never configurable.
pub const LOOPBACK_HOST: &str = "127.0.0.1";

/// Help flags a compatible `llama-server` build must advertise. A binary that
/// cannot show all of them is not treated as a supported `llama-server`.
pub const REQUIRED_HELP_FLAGS: &[&str] = &[
    "--model",
    "--host",
    "--port",
    "--alias",
    "--ctx-size",
    "--parallel",
    "--batch-size",
    "--flash-attn",
    "--no-kv-offload",
    "--n-gpu-layers",
    "--chat-template",
    "--embeddings",
];

/// Flags [`LlamaCppAdapter::command`] owns, in every spelling the pinned
/// `--help` advertises. Administrators must not supply these through
/// `Runtime::fixed_args`: because fixed args are appended last, a collision
/// would let them override the loopback host, reserved port, model identity,
/// or generated configuration. Aliases are listed next to their long form so
/// the short spellings (`-m`, `-ngl`, ...) cannot bypass collision protection.
pub const OWNED_FLAGS: &[&str] = &[
    "--model",
    "-m",
    "--host",
    "--port",
    "--alias",
    "-a",
    "--ctx-size",
    "-c",
    "--parallel",
    "-np",
    "--batch-size",
    "-b",
    "--flash-attn",
    "-fa",
    "--no-kv-offload",
    "-nkvo",
    "--n-gpu-layers",
    "--gpu-layers",
    "-ngl",
];

/// The health endpoint the adapter polls (`docs/architecture.md` §4).
pub const HEALTH_ENDPOINT: &str = "/health";

/// The llama.cpp engine adapter.
///
/// Stateless: every operation is an associated function taking the probed
/// snapshot or launch context it needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LlamaCppAdapter;

/// Everything [`LlamaCppAdapter::command`] needs to build a launch argv.
#[derive(Debug, Clone, Copy)]
pub struct LaunchContext<'a> {
    /// The model being loaded.
    pub model: &'a Model,
    /// The resolved load configuration.
    pub config: &'a LoadConfig,
    /// The registered runtime (executable path and fixed args).
    pub runtime: &'a Runtime,
    /// The loopback port reserved for this instance.
    pub port: u16,
}

impl LlamaCppAdapter {
    /// The engine family this adapter implements.
    #[must_use]
    pub const fn kind() -> RuntimeKind {
        RuntimeKind::LlamaCpp
    }

    /// Run `doctor` for `executable` using this adapter's compatibility rules.
    ///
    /// Unlike the generic [`model_serving_runtime::diagnose`], an engine that
    /// probes cleanly but is too old, is the wrong product, or is missing
    /// required flags is reported as
    /// [`DoctorStatus::Incompatible`](model_serving_runtime::DoctorStatus)
    /// instead of `Ready`, and a ready report carries this adapter's derived
    /// capabilities.
    pub async fn diagnose(executable: &Path, config: &ProbeConfig) -> DoctorReport {
        diagnose_with(executable, config, Self::capabilities).await
    }

    /// Map an already-obtained probe snapshot to a doctor report using this
    /// adapter's compatibility rules.
    ///
    /// This is the synchronous core of [`Self::diagnose`] and is used directly
    /// by the fixture-driven doctor tests.
    #[must_use]
    pub fn diagnose_snapshot(snapshot: &ProbeSnapshot) -> DoctorReport {
        match Self::capabilities(snapshot) {
            Ok(capabilities) => DoctorReport::ready_with(snapshot, capabilities),
            Err(error) => DoctorReport::from_error(&snapshot.executable, &error),
        }
    }

    /// Derive capabilities from a successful [`ProbeSnapshot`].
    ///
    /// # Errors
    ///
    /// Returns [`ProbeError::Incompatible`] when the snapshot's version or help
    /// text does not describe a supported `llama-server` build.
    pub fn capabilities(snapshot: &ProbeSnapshot) -> Result<Capabilities, ProbeError> {
        Self::capabilities_from_text(
            &snapshot.executable,
            &snapshot.version_text,
            &snapshot.help_text,
        )
    }

    /// Derive capabilities from raw `--version` / `--help` text.
    ///
    /// This is the pure core of [`Self::capabilities`], used directly by the
    /// fixture-driven tests.
    ///
    /// # Errors
    ///
    /// Returns [`ProbeError::Incompatible`] with `executable` as the path when:
    /// the version text is malformed, the build is older than
    /// [`MIN_SUPPORTED_BUILD`], or a flag in [`REQUIRED_HELP_FLAGS`] is absent.
    pub fn capabilities_from_text(
        executable: &Path,
        version_text: &str,
        help_text: &str,
    ) -> Result<Capabilities, ProbeError> {
        let build = parse_build_number(version_text).map_err(|message| {
            ProbeError::incompatible(
                executable,
                format!("unrecognized llama.cpp version text {version_text:?}: {message}"),
            )
        })?;
        if build < MIN_SUPPORTED_BUILD {
            return Err(ProbeError::incompatible(
                executable,
                format!(
                    "llama.cpp build b{build} is older than the minimum supported \
                     b{MIN_SUPPORTED_BUILD}"
                ),
            ));
        }
        for flag in REQUIRED_HELP_FLAGS {
            if !help_text.contains(flag) {
                return Err(ProbeError::incompatible(
                    executable,
                    format!(
                        "required help flag {flag} is missing; this is not a supported \
                         llama-server build"
                    ),
                ));
            }
        }

        Ok(Capabilities {
            supports_chat_completions: help_text.contains("--chat-template"),
            // The presence of the server flags (`--port`) is what makes this a
            // serving binary; a bare completion library would not expose them.
            supports_completions: help_text.contains("--port"),
            supports_embeddings: help_text.contains("--embeddings"),
            version: Some(version_text.trim().to_owned()),
            health_endpoint: Some(HEALTH_ENDPOINT.to_owned()),
        })
    }

    /// Validate that `model` and `config` are loadable by llama.cpp.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidModel`] when the artifact is not GGUF, and
    /// [`ErrorCode::UnsupportedField`] when `engine_config.ninfer` is present
    /// (llama.cpp has no NInfer-specific parameters).
    pub fn validate(model: &Model, config: &LoadConfig) -> DomainResult<()> {
        if model.artifact_kind != ArtifactKind::Gguf {
            return Err(DomainError::with_message(
                ErrorCode::InvalidModel,
                format!(
                    "llama.cpp runtime requires a gguf artifact, got {:?}",
                    model.artifact_kind
                ),
            ));
        }
        if config.ninfer_config().is_some() {
            return Err(DomainError::with_message(
                ErrorCode::UnsupportedField,
                "engine_config.ninfer is NInfer-specific and is not supported by the \
                 llama.cpp runtime",
            ));
        }
        Ok(())
    }

    /// Build the deterministic `llama-server` argv documented at module level.
    ///
    /// # Errors
    ///
    /// Propagates [`Self::validate`] failures.
    pub fn command(ctx: &LaunchContext<'_>) -> DomainResult<CommandSpec> {
        Self::validate(ctx.model, ctx.config)?;
        validate_runtime(ctx.runtime)?;
        let config = ctx.config;

        let mut spec = CommandSpec::new(ctx.runtime.executable_path.as_str())
            .with_arg("--model")
            .with_arg(ctx.model.path.as_str())
            .with_arg("--host")
            .with_arg(LOOPBACK_HOST)
            .with_arg("--port")
            .with_arg(ctx.port.to_string())
            .with_arg("--alias")
            .with_arg(ctx.model.key.as_str())
            .with_arg("--ctx-size")
            .with_arg(config.context_length.to_string());

        if let Some(parallel) = config.max_concurrency {
            spec = spec.with_arg("--parallel").with_arg(parallel.to_string());
        }
        if let Some(batch) = config.eval_batch_size {
            spec = spec.with_arg("--batch-size").with_arg(batch.to_string());
        }
        // The pinned build accepts `--flash-attn` with no value and defaults to
        // disabled, so only `Some(true)` emits it.
        if config.flash_attention == Some(true) {
            spec = spec.with_arg("--flash-attn");
        }
        // KV offload is enabled by default; only an explicit `false` changes it.
        if config.offload_kv_cache_to_gpu == Some(false) {
            spec = spec.with_arg("--no-kv-offload");
        }
        if let Some(layers) = config.n_gpu_layers {
            spec = spec.with_arg("--n-gpu-layers").with_arg(layers.to_string());
        }
        // Administrator fixed args go last (documented location above).
        for arg in &ctx.runtime.fixed_args {
            spec = spec.with_arg(arg.as_str());
        }
        Ok(spec)
    }

    /// Launch a llama.cpp instance using [`LlamaCppLifecycle`].
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError`] if argv validation or child process spawn fails.
    pub fn launch(
        ctx: &LaunchContext<'_>,
        config: lifecycle::LifecycleConfig,
    ) -> Result<lifecycle::LlamaCppLifecycle, lifecycle::LifecycleError> {
        lifecycle::LlamaCppLifecycle::launch(ctx, config)
    }

    /// Classify process termination using exit status and stderr output
    /// (`docs/architecture.md` §4, §6).
    #[must_use]
    pub fn classify_exit(
        exit: std::process::ExitStatus,
        stderr_tail: &str,
    ) -> model_serving_domain::model::FailureClass {
        if exit.success() {
            model_serving_domain::model::FailureClass::ProcessCrash
        } else {
            Self::classify_stderr(stderr_tail)
        }
    }

    /// Classify stderr output for known failure patterns (OOM, port conflict, invalid model).
    #[must_use]
    pub fn classify_stderr(stderr_tail: &str) -> model_serving_domain::model::FailureClass {
        use model_serving_domain::model::FailureClass;
        let lower = stderr_tail.to_ascii_lowercase();
        if lower.contains("cuda out of memory")
            || lower.contains("failed to allocate")
            || lower.contains("cudamalloc failed")
            || lower.contains("cublas_status_alloc_failed")
            || lower.contains("ggml_cuda_init: failed to allocate")
        {
            FailureClass::GpuOomLikely
        } else if lower.contains("cannot bind")
            || lower.contains("address already in use")
            || lower.contains("failed to bind")
            || lower.contains("wsaeaddrinuse")
            || lower.contains("eaddrinuse")
        {
            FailureClass::PortConflict
        } else if lower.contains("failed to load model")
            || lower.contains("error loading model")
            || lower.contains("invalid model")
            || lower.contains("not a valid gguf")
            || lower.contains("unknown model architecture")
            || lower.contains("unsupported format")
            || lower.contains("failed to parse")
        {
            FailureClass::InvalidModel
        } else {
            FailureClass::ProcessCrash
        }
    }
}

/// Validate that `runtime` is served by this adapter, has an absolute
/// executable, and that its fixed args do not override adapter-owned flags.
///
/// # Errors
///
/// Returns [`ErrorCode::InvalidRequest`] when the runtime kind is not
/// [`RuntimeKind::LlamaCpp`] or `executable_path` is empty/relative, and
/// [`ErrorCode::UnsupportedField`] when a fixed arg collides with a flag in
/// [`OWNED_FLAGS`] (including aliases and `--flag=value` spellings).
fn validate_runtime(runtime: &Runtime) -> DomainResult<()> {
    if runtime.kind != LlamaCppAdapter::kind() {
        return Err(DomainError::with_message(
            ErrorCode::InvalidRequest,
            format!(
                "llama.cpp adapter cannot serve runtime kind {:?}; expected {:?}",
                runtime.kind,
                LlamaCppAdapter::kind()
            ),
        ));
    }
    if !Path::new(&runtime.executable_path).is_absolute() {
        return Err(DomainError::with_message(
            ErrorCode::InvalidRequest,
            format!(
                "runtime.executable_path {:?} must be a non-empty absolute path to the \
                 llama-server binary",
                runtime.executable_path
            ),
        ));
    }
    if let Some(arg) = fixed_arg_collision(&runtime.fixed_args, OWNED_FLAGS) {
        return Err(DomainError::with_message(
            ErrorCode::UnsupportedField,
            format!(
                "runtime.fixed_args {arg:?} collides with an adapter-owned flag and cannot \
                 be overridden; adapter-owned flags are {OWNED_FLAGS:?}"
            ),
        ));
    }
    Ok(())
}

/// Parse the build number from `llama-server --version` text.
///
/// The pinned shape is `version: 5555 (803f8baf)`; the parser accepts any text
/// containing `version` followed by a colon and a run of ASCII digits.
///
/// # Errors
///
/// Returns a human-readable message when no `version` token or no leading
/// digits follow it.
pub fn parse_build_number(version_text: &str) -> Result<u32, String> {
    let lower = version_text.to_ascii_lowercase();
    let start = lower
        .find("version")
        .ok_or_else(|| "no `version` token".to_owned())?
        + "version".len();
    let rest = version_text[start..]
        .trim_start()
        .trim_start_matches(':')
        .trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return Err("no build number after `version:`".to_owned());
    }
    digits
        .parse::<u32>()
        .map_err(|error| format!("invalid build number {digits:?}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{MIN_SUPPORTED_BUILD, parse_build_number};

    #[test]
    fn parses_the_pinned_version_line() {
        let parsed = parse_build_number("version: 5555 (803f8baf)\nbuilt with cc");
        assert_eq!(parsed, Ok(MIN_SUPPORTED_BUILD));
    }

    #[test]
    fn rejects_text_without_a_version_token() {
        assert!(parse_build_number("who knows (deadbeef)").is_err());
    }

    #[test]
    fn rejects_a_version_without_digits() {
        assert!(parse_build_number("version: abc").is_err());
    }
}
