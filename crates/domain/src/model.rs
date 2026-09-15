//! Core domain objects (`docs/architecture.md` §3, §4).
//!
//! `Model`, `Runtime`, `Instance`, `Operation` and `LoadConfig`, plus their
//! enums. This module stays a **pure domain library**: no I/O, HTTP, storage,
//! or async code. IDs are opaque stable strings; timestamps are
//! `chrono::DateTime<Utc>` (serialized as RFC 3339 UTC, `docs/api.md` §1) so a
//! malformed timestamp is rejected at the domain boundary, never re-served.
//!
//! Field names and wire casing follow `docs/api.md` so the same structs can be
//! serialized to the management / LM Studio / `OpenAI` responses directly.
//!
//! Encapsulation policy: data fields are public so sibling crates can construct
//! and read the types. The lifecycle `state` is the one exception — it is
//! private, readable via `state()`, and can only change through the validated
//! `with_state` helpers (which run the target through the state machines in
//! `crate::state_machine`), so an illegal transition is rejected, never stored.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{ErrorCode, Result};
use crate::state_machine::{transition_instance, transition_operation};

/// The artifact format on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    /// A llama.cpp GGUF file (`*.gguf`).
    Gguf,
    /// An `NInfer` artifact (`*.ninfer`).
    Ninfer,
}

/// The inference engine family an executable belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    /// A `llama-server` (llama.cpp) executable.
    LlamaCpp,
    /// A `ninfer-serve` executable.
    Ninfer,
}

/// A discovered, indexed model (`docs/architecture.md` §3 "Model").
///
/// Identity is the stable `id` (UUID) + `key` (readable API identifier), never
/// the file name alone. A `deleted` model was seen on a prior scan but is no
/// longer on disk; it is retained so a re-appearing file can reconcile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    /// Stable internal UUID.
    pub id: String,
    /// Unique readable identifier used by the API; generated on scan, may be
    /// overridden by an admin.
    pub key: String,
    /// Canonical absolute path of the artifact.
    pub path: String,
    /// Artifact format (`gguf` / `ninfer`).
    pub artifact_kind: ArtifactKind,
    /// Artifact size in bytes.
    pub size_bytes: u64,
    /// Last modification time (RFC 3339 UTC, `DateTime<Utc>`).
    pub mtime: DateTime<Utc>,
    /// Human display name (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Default runtime to load this model with (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_runtime_id: Option<String>,
    /// Default load configuration (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_load_config: Option<LoadConfig>,
    /// Opaque metadata read from the artifact, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// True when a prior scan saw this model but it is no longer on disk.
    #[serde(default)]
    pub deleted: bool,
}

impl Model {
    /// Whether the model is no longer present on disk.
    #[must_use]
    pub fn is_deleted(&self) -> bool {
        self.deleted
    }

    /// The readable API identifier.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }
}

/// Capabilities probed from a runtime executable
/// (`RuntimeAdapter::probe`, `docs/architecture.md` §4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Whether the engine exposes an `OpenAI` Chat Completions endpoint.
    #[serde(default)]
    pub supports_chat_completions: bool,
    /// Whether the engine exposes a Completions endpoint.
    #[serde(default)]
    pub supports_completions: bool,
    /// Whether the engine exposes an Embeddings endpoint (capability gate for
    /// `POST /v1/embeddings`).
    #[serde(default)]
    pub supports_embeddings: bool,
    /// Raw version string reported by the executable (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The health endpoint path the adapter should poll (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_endpoint: Option<String>,
}

/// A registered inference runtime (`docs/architecture.md` §3 "Runtime").
///
/// A runtime is an identified executable of a known kind; arbitrary shell
/// commands are never stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Runtime {
    /// Stable runtime id.
    pub id: String,
    /// Engine family.
    pub kind: RuntimeKind,
    /// Absolute path to the executable.
    pub executable_path: String,
    /// Whether the runtime may be used for loads.
    #[serde(default)]
    pub enabled: bool,
    /// Version text captured from a probe (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_text: Option<String>,
    /// Capabilities captured from a probe.
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Administrator-defined fixed argument template, appended to the
    /// adapter-generated load arguments. An argv array, never a shell string
    /// (`docs/architecture.md` §3: 固定参数模板, 不允许存储任意 shell command).
    #[serde(default)]
    pub fixed_args: Vec<String>,
}

impl Runtime {
    /// Whether this runtime is currently enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// `NInfer` `--kv-capacity` value: either a fixed token count or the engine
/// default (`auto`). Serialized as a bare JSON scalar (`262144` / `"auto"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KvCapacity {
    /// Explicit `auto` — the engine sizes the KV cache itself.
    #[serde(with = "kv_capacity_auto")]
    Auto,
    /// Fixed KV capacity (tokens).
    Value(u32),
}

/// Custom scalar (de)serializer for [`KvCapacity::Auto`], which must round-trip
/// as the bare string `"auto"` inside the untagged enum.
mod kv_capacity_auto {
    use serde::Deserialize as _;
    use serde::de::Error as _;

    pub(super) fn serialize<S: serde::Serializer>(serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("auto")
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<(), D::Error> {
        let value = String::deserialize(deserializer)?;
        if value == "auto" {
            Ok(())
        } else {
            Err(D::Error::unknown_variant(&value, &["auto"]))
        }
    }
}

/// NInfer-specific engine parameters (`engine_config.ninfer`,
/// `docs/api.md` §5, M4). Only fields actually present are set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NinferEngineConfig {
    /// `--kv-capacity` (optional; `auto` or a token count).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kv_capacity: Option<KvCapacity>,
    /// `--prefill-chunk` (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefill_chunk: Option<u32>,
    /// `--kv-dtype` (optional), e.g. `"int8"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kv_dtype: Option<String>,
    /// Speculative decoding mode, e.g. `"mtp"` (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
    /// Number of draft tokens for speculation (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_tokens: Option<u32>,
    /// Whether a draft LM head is used (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lm_head_draft: Option<bool>,
    /// Thinking mode, e.g. `"disabled"` (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Whether a vision input path is enabled (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
}

/// Engine-specific configuration, keyed by engine. `NInfer` is the only engine
/// with engine-specific parameters in the first release; llama.cpp's fields are
/// carried in the common [`LoadConfig`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineConfig {
    /// NInfer-specific parameters (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ninfer: Option<NinferEngineConfig>,
}

/// The load parameters that resolve into a runtime launch
/// (`docs/api.md` §4, §5). Common fields are shared across engines; engine
/// specifics live under [`LoadConfig::engine_config`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadConfig {
    /// Context length in tokens.
    pub context_length: u32,
    /// Max concurrent requests (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<u32>,
    /// Evaluation / decode batch size (optional; llama.cpp & LM Studio).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_batch_size: Option<u32>,
    /// Whether flash attention is enabled (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flash_attention: Option<bool>,
    /// Whether the KV cache is offloaded to the GPU (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offload_kv_cache_to_gpu: Option<bool>,
    /// Number of layers on the GPU (optional; primarily llama.cpp).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_gpu_layers: Option<u32>,
    /// Engine-specific parameters (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_config: Option<EngineConfig>,
}

impl LoadConfig {
    /// Context length in tokens.
    #[must_use]
    pub fn context_length(&self) -> u32 {
        self.context_length
    }

    /// The NInfer-specific parameters, if any.
    #[must_use]
    pub fn ninfer_config(&self) -> Option<&NinferEngineConfig> {
        self.engine_config.as_ref().and_then(|e| e.ninfer.as_ref())
    }
}

/// A GPU/exit failure signature
/// (`RuntimeAdapter::classify_exit`, `docs/architecture.md` §6).
///
/// Only [`FailureClass::GpuOomLikely`] triggers the auto-eviction retry; every
/// other class fails the load directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// A signature consistent with running out of GPU memory.
    GpuOomLikely,
    /// The instance did not become healthy before its start deadline.
    StartupTimeout,
    /// The inference process exited unexpectedly.
    ProcessCrash,
    /// The chosen port could not be bound.
    PortConflict,
    /// The model artifact was rejected by the runtime.
    InvalidModel,
    /// A launch/config error unrelated to resources.
    ConfigError,
    /// The upstream produced a malformed or protocol-invalid response.
    UpstreamProtocol,
    /// No known signature matched.
    Unknown,
}

/// The lifecycle state of an instance
/// (`docs/architecture.md` §5). See [`crate::state_machine`] for the legal
/// transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    /// No process is running for this model.
    Unloaded,
    /// A load has been accepted and is waiting to start.
    Queued,
    /// The child process is starting and being health-checked.
    Loading,
    /// The instance is serving requests.
    Ready,
    /// Requests are being drained before unload.
    Draining,
    /// The child process is being terminated.
    Unloading,
    /// The load/unload attempt failed and was cleaned up.
    Failed,
    /// The process exited unexpectedly while running.
    Crashed,
}

/// A point-in-time health probe result for an instance.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceHealth {
    /// Whether the last probe was healthy.
    #[serde(default)]
    pub ok: bool,
    /// Probe latency in milliseconds (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// When the probe ran, RFC 3339 UTC (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<DateTime<Utc>>,
}

/// The structured reason an instance ended in a failed/crashed state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceFailure {
    /// The classified failure.
    pub class: FailureClass,
    /// Child exit code, if the process exited (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Tail of the child's stderr, for diagnostics (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
    /// Short human summary (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A load/unload/rescan operation (`docs/architecture.md` §3 "Operation").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_field_names,
    reason = "`operation_id`/`instance_id` are the documented wire field names (docs/api.md); the prefix is intentional, not noise."
)]
pub struct Operation {
    /// Stable operation id (`op_...`).
    pub operation_id: String,
    /// What the operation does.
    pub kind: OperationKind,
    /// Progress state (private; read via [`Operation::state`], changed via
    /// [`Operation::with_state`]).
    state: OperationState,
    /// The instance this operation acts on / produces (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// The model the operation targets (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// When the operation was created, RFC 3339 UTC (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    /// When the operation reached a terminal state, RFC 3339 UTC (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    /// Structured result error, set when the operation is `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<OperationError>,
    /// Structured result payload, set when the operation is `succeeded`
    /// (e.g. a rescan's discovered models). Opaque JSON so each operation
    /// kind carries its own shape (`docs/api.md` §5: 结构化结果).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}

impl Operation {
    /// Create a freshly-accepted operation in the initial
    /// [`OperationState::Queued`]. The variable data (`instance_id`,
    /// `model_id`, `created_at`, `finished_at`, `error`, `result`) starts as
    /// `None` and is set by the caller; the lifecycle `state` is private and
    /// moves only via [`Self::with_state`].
    #[must_use]
    pub fn new(operation_id: impl Into<String>, kind: OperationKind) -> Self {
        Self {
            operation_id: operation_id.into(),
            kind,
            state: OperationState::Queued,
            instance_id: None,
            model_id: None,
            created_at: None,
            finished_at: None,
            error: None,
            result: None,
        }
    }

    /// The current progress state.
    #[must_use]
    pub fn state(&self) -> OperationState {
        self.state
    }

    /// Whether the operation is in a terminal state.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// The operation's id.
    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    /// Apply a validated operation-state transition and return the updated
    /// clone (see [`crate::state_machine::transition_operation`]).
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InvalidStateTransition`] when `to` is not reachable from
    /// the current state.
    pub fn with_state(self, to: OperationState) -> Result<Self> {
        let state = transition_operation(self.state, to)?;
        let mut this = self;
        this.state = state;
        Ok(this)
    }
}

/// What an operation does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    /// Load a model into a new instance.
    Load,
    /// Drain and unload an instance.
    Unload,
    /// Rescan a model root for models.
    Rescan,
}

/// The progress state of an operation. See [`crate::state_machine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    /// Accepted, not started.
    Queued,
    /// In progress.
    Running,
    /// Finished successfully (terminal).
    Succeeded,
    /// Finished with an error (terminal).
    Failed,
    /// Cancelled before or during work (terminal).
    Cancelled,
}

/// The structured error attached to a failed operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationError {
    /// The stable error code.
    pub code: ErrorCode,
    /// Human-readable detail.
    pub message: String,
}

/// An evictable / placeable instance
/// (`docs/architecture.md` §3 "Instance", §6).
///
/// `pid` and `port` are only trustworthy once the process supervisor has
/// confirmed them; they stay `None` until then (`docs/architecture.md` §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_field_names,
    reason = "`instance_id` is the documented wire field name (docs/api.md); the prefix is intentional, not noise."
)]
pub struct Instance {
    /// Unique id for this load instance; also used by LM Studio unload.
    pub instance_id: String,
    /// The model loaded in this instance.
    pub model_id: String,
    /// The runtime used to load it.
    pub runtime_id: String,
    /// The resolved load configuration.
    pub load_config: LoadConfig,
    /// Current lifecycle state (private; read via [`Instance::state`],
    /// changed via [`Instance::with_state`]).
    state: InstanceState,
    /// Child PID, set once the supervisor confirms it (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Loopback port, set once the supervisor confirms it (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// GPU device indices this instance occupies (empty = CPU / unassigned).
    /// Reserved for multi-GPU placement & resource accounting
    /// (`docs/architecture.md` §6: 数据模型保留 `device_ids`).
    #[serde(default)]
    pub device_ids: Vec<u32>,
    /// When the instance started, RFC 3339 UTC (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// Last time the instance served a request, RFC 3339 UTC (optional);
    /// drives LRU idle eviction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
    /// Number of in-flight requests (hard eviction-protection signal).
    #[serde(default)]
    pub active_requests: u32,
    /// Last health probe (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<InstanceHealth>,
    /// Structured failure, set in `failed`/`crashed` states (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<InstanceFailure>,
}

impl Instance {
    /// Create a fresh instance with no process, in the initial
    /// [`InstanceState::Unloaded`] state. `model_id`/`runtime_id`/
    /// `load_config` identify the load; the remaining runtime fields
    /// (`pid`/`port`/`device_ids`/health/timestamps/`active_requests`/
    /// `failure`) default and are set by the supervisor. The lifecycle `state`
    /// is private and moves only via [`Self::with_state`].
    #[must_use]
    pub fn new(
        instance_id: impl Into<String>,
        model_id: impl Into<String>,
        runtime_id: impl Into<String>,
        load_config: LoadConfig,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            model_id: model_id.into(),
            runtime_id: runtime_id.into(),
            load_config,
            state: InstanceState::Unloaded,
            pid: None,
            port: None,
            device_ids: Vec::new(),
            started_at: None,
            last_used_at: None,
            active_requests: 0,
            health: None,
            failure: None,
        }
    }

    /// The current lifecycle state.
    #[must_use]
    pub fn state(&self) -> InstanceState {
        self.state
    }

    /// Whether the instance has in-flight requests (hard eviction protection,
    /// `docs/architecture.md` §6).
    #[must_use]
    pub fn has_active_requests(&self) -> bool {
        self.active_requests > 0
    }

    /// The instance's id.
    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Apply a validated load-state transition and return the updated clone.
    ///
    /// This is the only way to move an instance between states: the target
    /// runs through [`crate::state_machine::transition_instance`], so an
    /// illegal move is rejected rather than stored (the `state` field is
    /// private and cannot be assigned directly).
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InvalidStateTransition`] when `to` is not reachable from
    /// the current state.
    pub fn with_state(self, to: InstanceState) -> Result<Self> {
        let state = transition_instance(self.state, to)?;
        let mut this = self;
        this.state = state;
        Ok(this)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Parse an RFC 3339 literal into a `DateTime<Utc>` for test fixtures.
    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("valid RFC 3339 in test fixture")
            .with_timezone(&Utc)
    }

    /// Assert `value` survives a JSON serialize→deserialize round-trip.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "helper consumes the value to serialize it"
    )]
    fn round_trips<
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + PartialEq + core::fmt::Debug,
    >(
        value: T,
    ) {
        let wire = serde_json::to_string(&value).expect("serialize");
        let back: T = serde_json::from_str(&wire).expect("deserialize");
        assert_eq!(back, value);
    }

    fn sample_model() -> Model {
        Model {
            id: "0f8fad5b-1c2e-4a3d-9b7a-2f6c4e8d1a00".into(),
            key: "qwen-local".into(),
            path: "/mnt/d/models/qwen.gguf".into(),
            artifact_kind: ArtifactKind::Gguf,
            size_bytes: 17_000_000_000,
            mtime: ts("2026-08-01T12:00:00Z"),
            display_name: Some("Qwen Local".into()),
            default_runtime_id: Some("llama-cpp-0".into()),
            default_load_config: None,
            metadata: None,
            deleted: false,
        }
    }

    #[test]
    fn model_round_trips_and_preserves_fields() {
        let model = sample_model();
        let wire = serde_json::to_string(&model).expect("serialize model");
        let back: Model = serde_json::from_str(&wire).expect("deserialize model");
        assert_eq!(back, model);
        assert_eq!(back.key(), "qwen-local");
        assert!(!back.is_deleted());
    }

    #[test]
    fn model_optional_fields_are_skipped_when_none() {
        let mut model = sample_model();
        model.display_name = None;
        model.default_runtime_id = None;
        let v = serde_json::to_value(&model).expect("serialize");
        assert!(v.get("display_name").is_none());
        assert!(v.get("default_runtime_id").is_none());
        assert!(v.get("default_load_config").is_none());
        assert!(v.get("metadata").is_none());
        // `deleted` defaults to false and is still present (bool, not Option).
        assert_eq!(v["deleted"], false);
    }

    #[test]
    fn artifact_kind_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&ArtifactKind::Gguf).expect("ser"),
            "\"gguf\""
        );
        assert_eq!(
            serde_json::to_string(&ArtifactKind::Ninfer).expect("ser"),
            "\"ninfer\""
        );
    }

    #[test]
    fn runtime_kind_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&RuntimeKind::LlamaCpp).expect("ser"),
            "\"llama_cpp\""
        );
        assert_eq!(
            serde_json::to_string(&RuntimeKind::Ninfer).expect("ser"),
            "\"ninfer\""
        );
    }

    #[test]
    fn runtime_defaults_enabled_false_and_empty_capabilities() {
        let rt = Runtime {
            id: "llama-cpp-0".into(),
            kind: RuntimeKind::LlamaCpp,
            executable_path: "/opt/llama/llama-server".into(),
            enabled: true,
            version_text: Some("b4200".into()),
            capabilities: Capabilities::default(),
            fixed_args: vec![],
        };
        assert!(rt.is_enabled());
        let v = serde_json::to_value(&rt).expect("serialize runtime");
        assert_eq!(v["enabled"], true);
        assert_eq!(v["capabilities"]["supports_chat_completions"], false);
    }

    #[test]
    fn load_config_round_trips_the_native_example() {
        let json = r#"{
            "context_length": 262144,
            "max_concurrency": 1,
            "engine_config": {
                "ninfer": {
                    "kv_capacity": 262144,
                    "prefill_chunk": 2048,
                    "kv_dtype": "int8",
                    "spec": "mtp",
                    "draft_tokens": 3,
                    "lm_head_draft": true,
                    "thinking": "disabled"
                }
            }
        }"#;
        let cfg: LoadConfig = serde_json::from_str(json).expect("parse load config");
        assert_eq!(cfg.context_length(), 262_144);
        assert_eq!(
            cfg.ninfer_config().expect("ninfer").kv_capacity,
            Some(KvCapacity::Value(262_144))
        );
        assert_eq!(
            cfg.ninfer_config().expect("ninfer").kv_dtype.as_deref(),
            Some("int8")
        );

        let back = serde_json::to_string(&cfg).expect("serialize");
        let reparsed: LoadConfig = serde_json::from_str(&back).expect("reparse");
        assert_eq!(reparsed, cfg);
    }

    #[test]
    fn load_config_lm_studio_common_fields_parse() {
        let json = r#"{
            "context_length": 32768,
            "eval_batch_size": 512,
            "flash_attention": true,
            "offload_kv_cache_to_gpu": true
        }"#;
        let cfg: LoadConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(cfg.eval_batch_size, Some(512));
        assert_eq!(cfg.flash_attention, Some(true));
        assert_eq!(cfg.offload_kv_cache_to_gpu, Some(true));
        assert_eq!(cfg.max_concurrency, None);
    }

    #[test]
    fn kv_capacity_auto_round_trips_as_string() {
        let v = serde_json::to_value(KvCapacity::Auto).expect("serialize auto");
        assert_eq!(v, "auto");
        let back: KvCapacity = serde_json::from_str("\"auto\"").expect("parse auto");
        assert_eq!(back, KvCapacity::Auto);
    }

    #[test]
    fn kv_capacity_value_round_trips_as_number() {
        let v = serde_json::to_value(KvCapacity::Value(262_144)).expect("serialize");
        assert_eq!(v, 262_144);
        let back: KvCapacity = serde_json::from_str("262144").expect("parse");
        assert_eq!(back, KvCapacity::Value(262_144));
    }

    #[test]
    fn kv_capacity_rejects_unknown_scalar() {
        let err = serde_json::from_str::<KvCapacity>("\"bogus\"");
        assert!(err.is_err(), "a non-auto, non-number scalar must not parse");
    }

    #[test]
    fn instance_state_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&InstanceState::Ready).expect("ser"),
            "\"ready\""
        );
        assert_eq!(
            serde_json::to_string(&InstanceState::Unloading).expect("ser"),
            "\"unloading\""
        );
        assert_eq!(
            serde_json::to_string(&OperationState::Succeeded).expect("ser"),
            "\"succeeded\""
        );
    }

    #[test]
    fn failure_class_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&FailureClass::GpuOomLikely).expect("ser"),
            "\"gpu_oom_likely\""
        );
    }

    #[test]
    fn instance_tracks_active_requests_and_confirmed_pid() {
        let inst = Instance {
            instance_id: "inst-1".into(),
            model_id: "model-1".into(),
            runtime_id: "rt-1".into(),
            load_config: LoadConfig {
                context_length: 4096,
                max_concurrency: None,
                eval_batch_size: None,
                flash_attention: None,
                offload_kv_cache_to_gpu: None,
                n_gpu_layers: None,
                engine_config: None,
            },
            state: InstanceState::Ready,
            pid: Some(4242),
            port: Some(8080),
            device_ids: vec![0],
            started_at: Some(ts("2026-08-01T12:00:05Z")),
            last_used_at: None,
            active_requests: 2,
            health: Some(InstanceHealth {
                ok: true,
                latency_ms: Some(7),
                checked_at: None,
            }),
            failure: None,
        };
        assert!(inst.has_active_requests());
        assert_eq!(inst.instance_id(), "inst-1");
        let v = serde_json::to_value(&inst).expect("serialize");
        assert_eq!(v["pid"], 4242);
        assert_eq!(v["port"], 8080);
        assert_eq!(v["active_requests"], 2);
        assert_eq!(v["device_ids"], json!([0]));
        assert!(v.get("last_used_at").is_none());
    }

    #[test]
    fn operation_error_carries_code_and_message() {
        let op = Operation {
            operation_id: "op-1".into(),
            kind: OperationKind::Load,
            state: OperationState::Failed,
            instance_id: Some("inst-1".into()),
            model_id: Some("model-1".into()),
            created_at: Some(ts("2026-08-01T12:00:00Z")),
            finished_at: Some(ts("2026-08-01T12:00:09Z")),
            error: Some(OperationError {
                code: ErrorCode::StartupTimeout,
                message: "no healthy upstream after 30s".into(),
            }),
            result: None,
        };
        assert!(op.is_terminal());
        assert_eq!(op.operation_id(), "op-1");
        let v = serde_json::to_value(&op).expect("serialize");
        assert_eq!(v["state"], "failed");
        assert_eq!(v["error"]["code"], "startup_timeout");
        // a failed op carries an error and no result.
        assert!(v.get("result").is_none());
    }

    /// A succeeded operation carries its structured result payload
    /// (`docs/api.md` §5), e.g. a rescan's per-model counts.
    #[test]
    fn succeeded_operation_carries_structured_result() {
        let op = Operation {
            operation_id: "op-scan-1".into(),
            kind: OperationKind::Rescan,
            state: OperationState::Succeeded,
            instance_id: None,
            model_id: None,
            created_at: Some(ts("2026-08-01T12:00:00Z")),
            finished_at: Some(ts("2026-08-01T12:00:02Z")),
            error: None,
            result: Some(json!({ "added": 3, "removed": 1, "unchanged": 42 })),
        };
        assert!(op.is_terminal());
        let v = serde_json::to_value(&op).expect("serialize");
        assert_eq!(v["state"], "succeeded");
        assert_eq!(v["result"]["added"], 3);
        assert_eq!(v["result"]["removed"], 1);
    }

    /// A malformed `mtime` is rejected at the domain boundary (the RFC 3339
    /// UTC invariant, `docs/api.md` §1) instead of being stored and re-served.
    #[test]
    fn model_rejects_invalid_rfc3339_mtime() {
        let bad = r#"{
            "id": "0f8fad5b-1c2e-4a3d-9b7a-2f6c4e8d1a00",
            "key": "qwen-local",
            "path": "/mnt/d/models/qwen.gguf",
            "artifact_kind": "gguf",
            "size_bytes": 17000000000,
            "mtime": "not-a-timestamp",
            "deleted": false
        }"#;
        assert!(
            serde_json::from_str::<Model>(bad).is_err(),
            "a non-RFC3339 mtime must be rejected"
        );
    }

    /// `DateTime<Utc>` serializes to an RFC 3339 string (`docs/api.md` §1).
    #[test]
    fn model_mtime_serializes_as_rfc3339_string() {
        let v = serde_json::to_value(sample_model()).expect("serialize");
        assert_eq!(v["mtime"], "2026-08-01T12:00:00Z");
    }

    #[test]
    fn instance_round_trips() {
        let inst = Instance {
            instance_id: "inst-rt".into(),
            model_id: "model-rt".into(),
            runtime_id: "rt-rt".into(),
            load_config: LoadConfig {
                context_length: 4096,
                max_concurrency: Some(1),
                eval_batch_size: None,
                flash_attention: None,
                offload_kv_cache_to_gpu: None,
                n_gpu_layers: None,
                engine_config: None,
            },
            state: InstanceState::Loading,
            pid: Some(1234),
            port: Some(8091),
            device_ids: vec![0, 1],
            started_at: Some(ts("2026-08-01T12:00:05Z")),
            last_used_at: Some(ts("2026-08-01T12:30:00Z")),
            active_requests: 3,
            health: Some(InstanceHealth {
                ok: true,
                latency_ms: Some(7),
                checked_at: Some(ts("2026-08-01T12:30:01Z")),
            }),
            failure: None,
        };
        round_trips(inst);
    }

    #[test]
    fn instance_with_failure_round_trips() {
        let base = Instance::new(
            "inst-f",
            "model-f",
            "rt-f",
            LoadConfig {
                context_length: 4096,
                max_concurrency: None,
                eval_batch_size: None,
                flash_attention: None,
                offload_kv_cache_to_gpu: None,
                n_gpu_layers: None,
                engine_config: None,
            },
        );
        let inst = Instance {
            state: InstanceState::Crashed,
            failure: Some(InstanceFailure {
                class: FailureClass::ProcessCrash,
                exit_code: Some(137),
                stderr_tail: Some("out of memory".into()),
                message: Some("child exited".into()),
            }),
            ..base
        };
        round_trips(inst);
    }

    #[test]
    fn operation_round_trips() {
        let op = Operation {
            operation_id: "op-rt".into(),
            kind: OperationKind::Rescan,
            state: OperationState::Running,
            instance_id: None,
            model_id: Some("model-rt".into()),
            created_at: Some(ts("2026-08-01T12:00:00Z")),
            finished_at: None,
            error: None,
            result: None,
        };
        round_trips(op);
    }

    #[test]
    fn runtime_round_trips() {
        let rt = Runtime {
            id: "rt-rt".into(),
            kind: RuntimeKind::Ninfer,
            executable_path: "/opt/ninfer/bin/ninfer-serve".into(),
            enabled: true,
            version_text: Some("v0.1.0".into()),
            capabilities: Capabilities {
                supports_chat_completions: true,
                supports_completions: true,
                supports_embeddings: false,
                version: Some("0.1.0".into()),
                health_endpoint: Some("/health".into()),
            },
            fixed_args: vec!["--threads".into(), "8".into()],
        };
        round_trips(rt);
    }

    #[test]
    fn small_value_structs_round_trip() {
        round_trips(Capabilities {
            supports_chat_completions: true,
            supports_completions: false,
            supports_embeddings: true,
            version: Some("b4200".into()),
            health_endpoint: None,
        });
        round_trips(InstanceHealth {
            ok: false,
            latency_ms: None,
            checked_at: Some(ts("2026-08-01T12:30:01Z")),
        });
        round_trips(InstanceFailure {
            class: FailureClass::GpuOomLikely,
            exit_code: None,
            stderr_tail: None,
            message: Some("oom".into()),
        });
        round_trips(OperationError {
            code: ErrorCode::GpuOomLikely,
            message: "no memory".into(),
        });
        round_trips(EngineConfig {
            ninfer: Some(NinferEngineConfig {
                kv_capacity: Some(KvCapacity::Auto),
                prefill_chunk: Some(2048),
                kv_dtype: Some("int8".into()),
                spec: None,
                draft_tokens: None,
                lm_head_draft: None,
                thinking: None,
                vision: Some(true),
            }),
        });
    }

    /// Every enum variant serializes to its exact `docs/api.md` wire token
    /// (lowercase / `snake_case`) and round-trips, locking the wire casing for
    /// the whole vocabulary.
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "One exhaustive assertion per enum variant locking the docs/api.md wire tokens"
    )]
    fn all_enums_serialize_to_wire_token_and_round_trip() {
        for (value, wire) in [
            (ArtifactKind::Gguf, "gguf"),
            (ArtifactKind::Ninfer, "ninfer"),
        ] {
            assert_eq!(
                serde_json::to_string(&value).expect("ser"),
                format!("\"{wire}\"")
            );
            assert_eq!(
                serde_json::from_str::<ArtifactKind>(&format!("\"{wire}\"")).expect("de"),
                value
            );
        }
        for (value, wire) in [
            (RuntimeKind::LlamaCpp, "llama_cpp"),
            (RuntimeKind::Ninfer, "ninfer"),
        ] {
            assert_eq!(
                serde_json::to_string(&value).expect("ser"),
                format!("\"{wire}\"")
            );
            assert_eq!(
                serde_json::from_str::<RuntimeKind>(&format!("\"{wire}\"")).expect("de"),
                value
            );
        }
        for (value, wire) in [
            (InstanceState::Unloaded, "unloaded"),
            (InstanceState::Queued, "queued"),
            (InstanceState::Loading, "loading"),
            (InstanceState::Ready, "ready"),
            (InstanceState::Draining, "draining"),
            (InstanceState::Unloading, "unloading"),
            (InstanceState::Failed, "failed"),
            (InstanceState::Crashed, "crashed"),
        ] {
            assert_eq!(
                serde_json::to_string(&value).expect("ser"),
                format!("\"{wire}\"")
            );
            assert_eq!(
                serde_json::from_str::<InstanceState>(&format!("\"{wire}\"")).expect("de"),
                value
            );
        }
        for (value, wire) in [
            (OperationKind::Load, "load"),
            (OperationKind::Unload, "unload"),
            (OperationKind::Rescan, "rescan"),
        ] {
            assert_eq!(
                serde_json::to_string(&value).expect("ser"),
                format!("\"{wire}\"")
            );
            assert_eq!(
                serde_json::from_str::<OperationKind>(&format!("\"{wire}\"")).expect("de"),
                value
            );
        }
        for (value, wire) in [
            (OperationState::Queued, "queued"),
            (OperationState::Running, "running"),
            (OperationState::Succeeded, "succeeded"),
            (OperationState::Failed, "failed"),
            (OperationState::Cancelled, "cancelled"),
        ] {
            assert_eq!(
                serde_json::to_string(&value).expect("ser"),
                format!("\"{wire}\"")
            );
            assert_eq!(
                serde_json::from_str::<OperationState>(&format!("\"{wire}\"")).expect("de"),
                value
            );
        }
        for (value, wire) in [
            (FailureClass::GpuOomLikely, "gpu_oom_likely"),
            (FailureClass::StartupTimeout, "startup_timeout"),
            (FailureClass::ProcessCrash, "process_crash"),
            (FailureClass::PortConflict, "port_conflict"),
            (FailureClass::InvalidModel, "invalid_model"),
            (FailureClass::ConfigError, "config_error"),
            (FailureClass::UpstreamProtocol, "upstream_protocol"),
            (FailureClass::Unknown, "unknown"),
        ] {
            assert_eq!(
                serde_json::to_string(&value).expect("ser"),
                format!("\"{wire}\"")
            );
            assert_eq!(
                serde_json::from_str::<FailureClass>(&format!("\"{wire}\"")).expect("de"),
                value
            );
        }
        // KvCapacity is an untagged scalar: `auto` string vs a bare number.
        assert_eq!(
            serde_json::to_string(&KvCapacity::Auto).expect("ser"),
            "\"auto\""
        );
        assert_eq!(
            serde_json::to_string(&KvCapacity::Value(262_144)).expect("ser"),
            "262144"
        );
        assert_eq!(
            serde_json::from_str::<KvCapacity>("\"auto\"").expect("de"),
            KvCapacity::Auto
        );
        assert_eq!(
            serde_json::from_str::<KvCapacity>("262144").expect("de"),
            KvCapacity::Value(262_144)
        );
    }
}
