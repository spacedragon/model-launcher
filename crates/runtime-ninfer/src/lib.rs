//! `NInfer` (`ninfer-serve`) runtime adapter.
//!
//! Maps a [`LoadConfig`] to a deterministic, shell-free [`CommandSpec`] and
//! derives [`Capabilities`] from a probed `ninfer-serve` build
//! (`docs/architecture.md` §4). The minimum supported version is pinned in
//! `fixtures/runtimes/manifest.json` and `fixtures/runtimes/README.md`; keep
//! this constant in sync with them.
//!
//! # Determinism
//!
//! [`NinferAdapter::command`] emits the argument vector in one documented
//! order:
//!
//! 1. `<path>` — the positional `.ninfer` artifact path (first, before flags).
//! 2. `--host 127.0.0.1` — always loopback, never administrator-supplied.
//! 3. `--port <port>` — the port reserved by the control plane.
//! 4. `--model-id <model.key>` — the identity served by `/v1/models`.
//! 5. `--max-context <context_length>`.
//! 6. `--max-concurrency <max_concurrency>` when set.
//! 7. `--kv-capacity <auto|n>` when `engine_config.ninfer.kv_capacity` is set.
//! 8. `--prefill-chunk <n>` when set.
//! 9. `--kv-dtype <dtype>` when set (whitelist below).
//! 10. `--spec <mode>` when set, then `--draft-tokens <n>` when set.
//!     `draft_tokens` requires `spec`; see [`NinferAdapter::validate`].
//! 11. `--lm-head-draft` when `lm_head_draft == Some(true)`.
//! 12. Thinking mode: `disabled` → `--no-thinking`, `preserve` →
//!     `--preserve-thinking`, `enabled` → nothing (engine default).
//! 13. `--vision` when `vision == Some(true)`.
//! 14. The administrator's `Runtime::fixed_args` verbatim, in order, **last**
//!     (the documented deterministic location; last-wins overrides).
//!
//! Every element is pushed as a separate `OsString`; the executable and its
//! arguments never pass through a shell.

use std::path::Path;

use model_serving_domain::error::{DomainError, ErrorCode, Result as DomainResult};
use model_serving_domain::model::{
    ArtifactKind, Capabilities, KvCapacity, LoadConfig, Model, Runtime, RuntimeKind,
};
use model_serving_runtime::{
    CommandSpec, DoctorReport, ProbeConfig, ProbeError, ProbeSnapshot, diagnose_with,
    fixed_arg_collision,
};

/// Minimum supported `ninfer-serve` version as `(major, minor, patch)`.
pub const MIN_SUPPORTED_VERSION: (u32, u32, u32) = (0, 9, 0);

/// Loopback host every engine binds to. Never configurable.
pub const LOOPBACK_HOST: &str = "127.0.0.1";

/// Help flags a compatible `ninfer-serve` build must advertise.
pub const REQUIRED_HELP_FLAGS: &[&str] = &[
    "--host",
    "--port",
    "--model-id",
    "--max-context",
    "--kv-capacity",
    "--max-concurrency",
    "--prefill-chunk",
    "--kv-dtype",
    "--spec",
    "--draft-tokens",
    "--lm-head-draft",
    "--vision",
    "--no-thinking",
    "--preserve-thinking",
];

/// Accepted `--kv-dtype` values (pinned build whitelist).
pub const KV_DTYPES: &[&str] = &["bf16", "int8", "fp8", "nvfp4", "k8v4"];

/// Accepted `--spec` modes (pinned build whitelist).
pub const SPEC_MODES: &[&str] = &["mtp", "dflash", "dflash2"];

/// Accepted `thinking` values mapped by the adapter.
pub const THINKING_MODES: &[&str] = &["enabled", "disabled", "preserve"];

/// Flags [`NinferAdapter::command`] owns. Administrators must not supply these
/// through `Runtime::fixed_args`: because fixed args are appended last, a
/// collision would let them override the loopback host, reserved port, model
/// identity, or generated configuration.
pub const OWNED_FLAGS: &[&str] = &[
    "--host",
    "--port",
    "--model-id",
    "--max-context",
    "--max-concurrency",
    "--kv-capacity",
    "--prefill-chunk",
    "--kv-dtype",
    "--spec",
    "--draft-tokens",
    "--lm-head-draft",
    "--no-thinking",
    "--preserve-thinking",
    "--vision",
];

/// The health endpoint the adapter polls (`docs/architecture.md` §4).
pub const HEALTH_ENDPOINT: &str = "/health";

/// The `NInfer` engine adapter.
///
/// Stateless: every operation is an associated function taking the probed
/// snapshot or launch context it needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NinferAdapter;

/// Everything [`NinferAdapter::command`] needs to build a launch argv.
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

impl NinferAdapter {
    /// The engine family this adapter implements.
    #[must_use]
    pub const fn kind() -> RuntimeKind {
        RuntimeKind::Ninfer
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
    /// text does not describe a supported `ninfer-serve` build.
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
    /// the help text does not identify `ninfer-serve`, the version text is
    /// malformed, the version is older than [`MIN_SUPPORTED_VERSION`], or a
    /// flag in [`REQUIRED_HELP_FLAGS`] is absent.
    pub fn capabilities_from_text(
        executable: &Path,
        version_text: &str,
        help_text: &str,
    ) -> Result<Capabilities, ProbeError> {
        if !help_text.contains("ninfer-serve") {
            return Err(ProbeError::incompatible(
                executable,
                "help text does not identify a ninfer-serve executable",
            ));
        }
        let version = parse_version(version_text).map_err(|message| {
            ProbeError::incompatible(
                executable,
                format!("unrecognized ninfer-serve version text {version_text:?}: {message}"),
            )
        })?;
        if version < MIN_SUPPORTED_VERSION {
            return Err(ProbeError::incompatible(
                executable,
                format!(
                    "ninfer-serve {}.{}.{} is older than the minimum supported {}.{}.{}",
                    version.0,
                    version.1,
                    version.2,
                    MIN_SUPPORTED_VERSION.0,
                    MIN_SUPPORTED_VERSION.1,
                    MIN_SUPPORTED_VERSION.2
                ),
            ));
        }
        for flag in REQUIRED_HELP_FLAGS {
            if !help_text.contains(flag) {
                return Err(ProbeError::incompatible(
                    executable,
                    format!(
                        "required help flag {flag} is missing; this is not a supported \
                         ninfer-serve build"
                    ),
                ));
            }
        }

        Ok(Capabilities {
            supports_chat_completions: help_text.contains("Chat Completions"),
            // `ninfer-serve` serves the OpenAI Responses API and Chat
            // Completions, but not the legacy `/v1/completions` endpoint.
            supports_completions: false,
            // Embeddings are not offered by the pinned build.
            supports_embeddings: false,
            version: Some(version_text.trim().to_owned()),
            health_endpoint: Some(HEALTH_ENDPOINT.to_owned()),
        })
    }

    /// Validate that `model` and `config` are loadable by `NInfer`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidModel`] when the artifact is not `.ninfer`,
    /// and [`ErrorCode::UnsupportedField`] when a llama.cpp-only common field
    /// is set or an `NInfer` parameter is outside its pinned whitelist /
    /// dependency rules.
    pub fn validate(model: &Model, config: &LoadConfig) -> DomainResult<()> {
        if model.artifact_kind != ArtifactKind::Ninfer {
            return Err(DomainError::with_message(
                ErrorCode::InvalidModel,
                format!(
                    "NInfer runtime requires a .ninfer artifact, got {:?}",
                    model.artifact_kind
                ),
            ));
        }
        reject_llama_only(config)?;

        let Some(ninfer) = config.ninfer_config() else {
            return Ok(());
        };
        if let Some(dtype) = &ninfer.kv_dtype
            && !KV_DTYPES.contains(&dtype.as_str())
        {
            return Err(DomainError::with_message(
                ErrorCode::UnsupportedField,
                format!(
                    "engine_config.ninfer.kv_dtype {dtype:?} is not supported; \
                     expected one of {KV_DTYPES:?}"
                ),
            ));
        }
        if let Some(spec) = &ninfer.spec
            && !SPEC_MODES.contains(&spec.as_str())
        {
            return Err(DomainError::with_message(
                ErrorCode::UnsupportedField,
                format!(
                    "engine_config.ninfer.spec {spec:?} is not supported; \
                     expected one of {SPEC_MODES:?}"
                ),
            ));
        }
        if ninfer.draft_tokens.is_some() && ninfer.spec.is_none() {
            return Err(DomainError::with_message(
                ErrorCode::UnsupportedField,
                "engine_config.ninfer.draft_tokens requires engine_config.ninfer.spec",
            ));
        }
        if let Some(thinking) = &ninfer.thinking
            && !THINKING_MODES.contains(&thinking.as_str())
        {
            return Err(DomainError::with_message(
                ErrorCode::UnsupportedField,
                format!(
                    "engine_config.ninfer.thinking {thinking:?} is not supported; \
                     expected one of {THINKING_MODES:?}"
                ),
            ));
        }
        Ok(())
    }

    /// Build the deterministic `ninfer-serve` argv documented at module level.
    ///
    /// # Errors
    ///
    /// Propagates [`Self::validate`] failures.
    pub fn command(ctx: &LaunchContext<'_>) -> DomainResult<CommandSpec> {
        Self::validate(ctx.model, ctx.config)?;
        validate_runtime(ctx.runtime)?;
        let config = ctx.config;

        // The `.ninfer` artifact is the first positional argument.
        let mut spec = CommandSpec::new(ctx.runtime.executable_path.as_str())
            .with_arg(ctx.model.path.as_str())
            .with_arg("--host")
            .with_arg(LOOPBACK_HOST)
            .with_arg("--port")
            .with_arg(ctx.port.to_string())
            .with_arg("--model-id")
            .with_arg(ctx.model.key.as_str())
            .with_arg("--max-context")
            .with_arg(config.context_length.to_string());

        if let Some(concurrency) = config.max_concurrency {
            spec = spec
                .with_arg("--max-concurrency")
                .with_arg(concurrency.to_string());
        }

        if let Some(ninfer) = config.ninfer_config() {
            if let Some(capacity) = ninfer.kv_capacity {
                let value = match capacity {
                    KvCapacity::Auto => "auto".to_owned(),
                    KvCapacity::Value(tokens) => tokens.to_string(),
                };
                spec = spec.with_arg("--kv-capacity").with_arg(value);
            }
            if let Some(chunk) = ninfer.prefill_chunk {
                spec = spec.with_arg("--prefill-chunk").with_arg(chunk.to_string());
            }
            if let Some(dtype) = &ninfer.kv_dtype {
                spec = spec.with_arg("--kv-dtype").with_arg(dtype.as_str());
            }
            if let Some(mode) = &ninfer.spec {
                spec = spec.with_arg("--spec").with_arg(mode.as_str());
            }
            if let Some(draft_tokens) = ninfer.draft_tokens {
                spec = spec
                    .with_arg("--draft-tokens")
                    .with_arg(draft_tokens.to_string());
            }
            if ninfer.lm_head_draft == Some(true) {
                spec = spec.with_arg("--lm-head-draft");
            }
            match ninfer.thinking.as_deref() {
                Some("disabled") => spec = spec.with_arg("--no-thinking"),
                Some("preserve") => spec = spec.with_arg("--preserve-thinking"),
                _ => {}
            }
            if ninfer.vision == Some(true) {
                spec = spec.with_arg("--vision");
            }
        }

        // Administrator fixed args go last (documented location above).
        for arg in &ctx.runtime.fixed_args {
            spec = spec.with_arg(arg.as_str());
        }
        Ok(spec)
    }
}

/// Validate that `runtime` is served by this adapter, has an absolute
/// executable, and that its fixed args do not override adapter-owned flags.
///
/// # Errors
///
/// Returns [`ErrorCode::InvalidRequest`] when the runtime kind is not
/// [`RuntimeKind::Ninfer`] or `executable_path` is empty/relative, and
/// [`ErrorCode::UnsupportedField`] when a fixed arg collides with a flag in
/// [`OWNED_FLAGS`] (including `--flag=value` spellings).
fn validate_runtime(runtime: &Runtime) -> DomainResult<()> {
    if runtime.kind != NinferAdapter::kind() {
        return Err(DomainError::with_message(
            ErrorCode::InvalidRequest,
            format!(
                "NInfer adapter cannot serve runtime kind {:?}; expected {:?}",
                runtime.kind,
                NinferAdapter::kind()
            ),
        ));
    }
    if !Path::new(&runtime.executable_path).is_absolute() {
        return Err(DomainError::with_message(
            ErrorCode::InvalidRequest,
            format!(
                "runtime.executable_path {:?} must be a non-empty absolute path to the \
                 ninfer-serve binary",
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

/// Reject common llama.cpp-only fields instead of silently ignoring them
/// (`docs/architecture.md` §4: never silently misconfigure).
fn reject_llama_only(config: &LoadConfig) -> DomainResult<()> {
    let unsupported = [
        ("eval_batch_size", config.eval_batch_size.is_some()),
        ("flash_attention", config.flash_attention.is_some()),
        (
            "offload_kv_cache_to_gpu",
            config.offload_kv_cache_to_gpu.is_some(),
        ),
        ("n_gpu_layers", config.n_gpu_layers.is_some()),
    ];
    for (field, present) in unsupported {
        if present {
            return Err(DomainError::with_message(
                ErrorCode::UnsupportedField,
                format!("{field} is a llama.cpp field and is not supported by the NInfer runtime"),
            ));
        }
    }
    Ok(())
}

/// Parse `ninfer-serve --version` text into `(major, minor, patch)`.
///
/// The pinned shape is `ninfer-serve 0.9.2`; the parser accepts a version
/// immediately after the `ninfer-serve` token, then reads up to three dot
/// separated numeric components (missing trailing components default to 0).
///
/// # Errors
///
/// Returns a human-readable message when `ninfer-serve` or a leading numeric
/// component is absent.
pub fn parse_version(version_text: &str) -> Result<(u32, u32, u32), String> {
    let index = version_text
        .find("ninfer-serve")
        .ok_or_else(|| "no `ninfer-serve` token".to_owned())?
        + "ninfer-serve".len();
    let rest = version_text[index..].trim_start();
    let token: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if token.is_empty() {
        return Err("no version after `ninfer-serve`".to_owned());
    }
    let mut parts = token.split('.');
    let mut component = || -> Result<u32, String> {
        match parts.next() {
            Some(text) if !text.is_empty() => text
                .parse::<u32>()
                .map_err(|error| format!("invalid version component {text:?}: {error}")),
            _ => Ok(0),
        }
    };
    let major = component()?;
    let minor = component()?;
    let patch = component()?;
    Ok((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::{MIN_SUPPORTED_VERSION, parse_version};

    #[test]
    fn parses_the_pinned_version_line() {
        let parsed = parse_version("ninfer-serve 0.9.2\nbuild: fixture-pinned");
        assert_eq!(parsed, Ok((0, 9, 2)));
    }

    #[test]
    fn tolerates_missing_patch_component() {
        assert_eq!(parse_version("ninfer-serve 1.2"), Ok((1, 2, 0)));
    }

    #[test]
    fn rejects_text_without_an_identity_or_version() {
        assert!(parse_version("some-other-server 9.9.9").is_err());
        assert!(parse_version("ninfer-serve").is_err());
    }

    #[test]
    fn minimum_supported_version_is_pinned() {
        assert_eq!(MIN_SUPPORTED_VERSION, (0, 9, 0));
    }
}
