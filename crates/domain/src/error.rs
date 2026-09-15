//! Stable error catalog and the shared [`DomainError`] type.
//!
//! Every error that crosses a boundary in `model-serving` is represented by a
//! stable [`ErrorCode`]. The code fixes four things, all derivable from a
//! single table (see [`ErrorCode::meta`]) so the wire mappers below cannot
//! drift apart:
//!
//! - `code_str` — the stable machine token (`"model_not_found"`), matching the
//!   `code` field in both the `OpenAI` error body and the Problem Details body
//!   (`docs/api.md` §2.2).
//! - `title` — a human-readable default title / message.
//! - `default_status` — the HTTP status the code maps to.
//! - `openai_type` — the `OpenAI` `error.type` for the `/v1/*` gateway.
//!
//! [`ErrorCode::to_openai_error`] and [`ErrorCode::to_problem_details`] (via
//! [`DomainError`]) are pure functions: they produce the two wire shapes
//! without touching I/O, so both the `OpenAI` gateway and the management API can
//! build responses from the same domain error.
//!
//! See `docs/api.md` §2.2 for the two wire shapes and the status-code table.

use serde::{Deserialize, Serialize};

/// Convenience alias for `Result<T, DomainError>` used across the domain.
pub type Result<T> = std::result::Result<T, DomainError>;

/// Stable, serializable error code catalog.
///
/// The `snake_case` serde representation of each variant intentionally equals
/// [`ErrorCode::code_str`], so a code can be stored or sent as a bare string
/// and round-tripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Management/inference bearer token missing or invalid (401).
    Unauthorized,
    /// Authenticated but not permitted (403).
    Forbidden,

    /// Malformed request body or missing required field (400).
    InvalidRequest,
    /// A request field is not supported by the selected runtime (422).
    UnsupportedField,
    /// A requested capability (e.g. embeddings) is not provided by the
    /// instance (422).
    UnsupportedCapability,
    /// Request body exceeded the configured limit (413).
    BodyTooLarge,
    /// Request headers exceeded the configured limit (431).
    HeaderTooLarge,
    /// Client is sending too many requests (429).
    RateLimited,

    /// Model key does not exist in the index (404).
    ModelNotFound,
    /// Model exists but has no loaded instance (404).
    ModelNotLoaded,
    /// Model is loaded but not `ready` (loading/draining) (503).
    ModelNotReady,
    /// The upstream runtime process is not serving (crashed) (503).
    UpstreamUnavailable,

    /// Resource precheck says GPU memory is insufficient (409).
    GpuMemoryInsufficient,
    /// The load failed with a signature consistent with a GPU OOM (409).
    GpuOomLikely,
    /// A generic resource was exhausted (409).
    ResourceExhausted,
    /// The requested eviction conflicts with an active-lease protection (409).
    EvictionConflict,
    /// The chosen loopback port could not be bound (409).
    PortConflict,

    /// The model artifact is invalid or unsupported by the runtime (400).
    InvalidModel,
    /// The instance did not become healthy before its start deadline (504).
    StartupTimeout,
    /// The inference process exited unexpectedly (503).
    ProcessCrash,
    /// The upstream returned a malformed or protocol-invalid response (502).
    UpstreamProtocolError,
    /// The upstream request timed out (504).
    UpstreamTimeout,
    /// The upstream returned an error status that is not a protocol error (502).
    UpstreamError,

    /// No such instance (including already-unloaded historical ones) (404).
    InstanceNotFound,
    /// A requested state change is not legal for the current state (409).
    InvalidStateTransition,
    /// The LM Studio-compatible endpoint is not implemented; returned as 404
    /// so clients never mistake it for a success (404).
    EndpointNotFound,
    /// An operation is not implemented for this build (501).
    NotImplemented,

    /// An unexpected internal failure (500).
    Internal,
}

impl ErrorCode {
    /// The single source of truth for a code's wire attributes:
    /// `(code_str, title, default_status, openai_type, problem_type)`.
    ///
    /// `openai_type` follows `OpenAI`'s convention by status class
    /// (`docs/api.md` §2.2): `401` → `authentication_error`,
    /// `403` → `permission_error`, `500` → `server_error` (our fault),
    /// request-shape problems (`400`/`404`/`413`/`422`/`431`) →
    /// `invalid_request_error`, and every other client-visible failure
    /// (`409`/`429`/`502`/`503`/`504`, `501`) → `api_error`.
    ///
    /// `problem_type` is the kebab-case of `code_str`, the only documented
    /// exception being the GPU/resource group (`gpu_memory_insufficient` /
    /// `gpu_oom_likely` / `resource_exhausted`), which share the
    /// `resource-exhausted` category: `docs/api.md` §2.2 shows
    /// `gpu_memory_insufficient` reporting `type: "...:resource-exhausted"`
    /// while `code: "gpu_memory_insufficient"`.
    #[allow(
        clippy::too_many_lines,
        reason = "One exhaustive match over every ErrorCode (docs/api.md §2.2); decomposing it would hide the wire source of truth."
    )]
    const fn meta(self) -> (&'static str, &'static str, u16, &'static str, &'static str) {
        match self {
            Self::Unauthorized => (
                "unauthorized",
                "Authentication required",
                401,
                "authentication_error",
                "unauthorized",
            ),
            Self::Forbidden => (
                "forbidden",
                "Insufficient permissions",
                403,
                "permission_error",
                "forbidden",
            ),
            Self::InvalidRequest => (
                "invalid_request",
                "Request is invalid",
                400,
                "invalid_request_error",
                "invalid-request",
            ),
            Self::UnsupportedField => (
                "unsupported_field",
                "Field not supported by runtime",
                422,
                "invalid_request_error",
                "unsupported-field",
            ),
            Self::UnsupportedCapability => (
                "unsupported_capability",
                "Capability not supported",
                422,
                "invalid_request_error",
                "unsupported-capability",
            ),
            Self::BodyTooLarge => (
                "body_too_large",
                "Request body too large",
                413,
                "invalid_request_error",
                "body-too-large",
            ),
            Self::HeaderTooLarge => (
                "header_too_large",
                "Request headers too large",
                431,
                "invalid_request_error",
                "header-too-large",
            ),
            Self::RateLimited => (
                "rate_limited",
                "Too many requests",
                429,
                "api_error",
                "rate-limited",
            ),
            Self::ModelNotFound => (
                "model_not_found",
                "Model not found",
                404,
                "invalid_request_error",
                "model-not-found",
            ),
            Self::ModelNotLoaded => (
                "model_not_loaded",
                "Model is not loaded",
                404,
                "invalid_request_error",
                "model-not-loaded",
            ),
            Self::ModelNotReady => (
                "model_not_ready",
                "Model is not ready",
                503,
                "api_error",
                "model-not-ready",
            ),
            Self::UpstreamUnavailable => (
                "upstream_unavailable",
                "Upstream is unavailable",
                503,
                "api_error",
                "upstream-unavailable",
            ),
            Self::GpuMemoryInsufficient => (
                "gpu_memory_insufficient",
                "Insufficient GPU memory",
                409,
                "api_error",
                "resource-exhausted",
            ),
            Self::GpuOomLikely => (
                "gpu_oom_likely",
                "Out of GPU memory (likely)",
                409,
                "api_error",
                "resource-exhausted",
            ),
            Self::ResourceExhausted => (
                "resource_exhausted",
                "Resource exhausted",
                409,
                "api_error",
                "resource-exhausted",
            ),
            Self::EvictionConflict => (
                "eviction_conflict",
                "Eviction conflict",
                409,
                "api_error",
                "eviction-conflict",
            ),
            Self::PortConflict => (
                "port_conflict",
                "Port conflict",
                409,
                "api_error",
                "port-conflict",
            ),
            Self::InvalidModel => (
                "invalid_model",
                "Model artifact is invalid",
                400,
                "invalid_request_error",
                "invalid-model",
            ),
            Self::StartupTimeout => (
                "startup_timeout",
                "Instance failed to start in time",
                504,
                "api_error",
                "startup-timeout",
            ),
            Self::ProcessCrash => (
                "process_crash",
                "Inference process crashed",
                503,
                "api_error",
                "process-crash",
            ),
            Self::UpstreamProtocolError => (
                "upstream_protocol_error",
                "Upstream protocol error",
                502,
                "api_error",
                "upstream-protocol-error",
            ),
            Self::UpstreamTimeout => (
                "upstream_timeout",
                "Upstream timed out",
                504,
                "api_error",
                "upstream-timeout",
            ),
            Self::UpstreamError => (
                "upstream_error",
                "Upstream returned an error",
                502,
                "api_error",
                "upstream-error",
            ),
            Self::InstanceNotFound => (
                "instance_not_found",
                "Instance not found",
                404,
                "invalid_request_error",
                "instance-not-found",
            ),
            Self::InvalidStateTransition => (
                "invalid_state_transition",
                "Invalid state transition",
                409,
                "api_error",
                "invalid-state-transition",
            ),
            Self::EndpointNotFound => (
                "endpoint_not_found",
                "Endpoint not found",
                404,
                "invalid_request_error",
                "endpoint-not-found",
            ),
            Self::NotImplemented => (
                "not_implemented",
                "Not implemented",
                501,
                "api_error",
                "not-implemented",
            ),
            Self::Internal => (
                "internal",
                "Internal error",
                500,
                "server_error",
                "internal",
            ),
        }
    }

    /// Stable machine token, e.g. `"model_not_loaded"`.
    #[must_use]
    pub const fn code_str(self) -> &'static str {
        self.meta().0
    }

    /// Human-readable default title / message.
    #[must_use]
    pub const fn title(self) -> &'static str {
        self.meta().1
    }

    /// Default HTTP status this code maps to.
    #[must_use]
    pub const fn default_status(self) -> u16 {
        self.meta().2
    }

    /// `OpenAI` `error.type` for the `/v1/*` gateway.
    #[must_use]
    pub fn openai_type(self) -> &'static str {
        self.meta().3
    }

    /// Problem Details category for the native-management `type` URN.
    ///
    /// Deliberately separate from [`Self::code_str`]: the management API
    /// reports a coarse category (`docs/api.md` §2.2) while `code` carries
    /// the precise machine token.
    #[must_use]
    pub fn problem_type(self) -> &'static str {
        self.meta().4
    }

    /// Problem Details `type` URN for the native management API, e.g.
    /// `"urn:model-serving:error:resource-exhausted"`. Built from the
    /// category [`Self::problem_type`], not the machine `code_str`.
    #[must_use]
    pub fn urn(self) -> String {
        format!("urn:model-serving:error:{}", self.problem_type())
    }

    /// Build the OpenAI-style gateway error body (`docs/api.md` §2.2).
    ///
    /// `param` is the optional offending field name (e.g. `"model"`); pass
    /// `None` when there is no single field to blame.
    pub fn to_openai_error(self, message: &str, param: Option<&str>) -> OpenAiErrorBody {
        OpenAiErrorBody {
            error: OpenAiError {
                message: message.to_string(),
                r#type: self.openai_type().to_string(),
                param: param.map(str::to_string),
                code: self.code_str().to_string(),
            },
        }
    }

    /// Build the Problem Details body for the native management API
    /// (`docs/api.md` §2.2). `request_id` is the correlation id; pass `None`
    /// when none was established.
    pub fn to_problem_details(self, message: &str, request_id: Option<&str>) -> ProblemDetails {
        ProblemDetails {
            r#type: self.urn(),
            title: self.title().to_string(),
            status: self.default_status(),
            detail: Some(message.to_string()),
            r#request_id: request_id.map(str::to_string),
            code: Some(self.code_str().to_string()),
        }
    }
}

/// A domain error: a stable [`ErrorCode`] plus a concrete message.
///
/// Construct with `code.into()` for the default message, or
/// [`DomainError::with_message`] to attach a specific detail.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DomainError {
    /// The stable error code.
    pub code: ErrorCode,
    /// Concrete, human-readable message (defaults to [`ErrorCode::title`]).
    pub message: String,
}

impl DomainError {
    /// Build an error with a specific message for `code`.
    pub fn with_message(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The OpenAI-style gateway error body for this error
    /// (`docs/api.md` §2.2).
    #[must_use]
    pub fn to_openai_error(&self, param: Option<&str>) -> OpenAiErrorBody {
        self.code.to_openai_error(&self.message, param)
    }

    /// The Problem Details body for this error on the native management API
    /// (`docs/api.md` §2.2).
    #[must_use]
    pub fn to_problem_details(&self, request_id: Option<&str>) -> ProblemDetails {
        self.code.to_problem_details(&self.message, request_id)
    }

    /// The default HTTP status for this error's code.
    ///
    /// The API layer sets the response status from this; the per-request
    /// correlation `request_id` is not part of the error (it is assigned by
    /// the HTTP layer and passed to [`Self::to_problem_details`]).
    #[must_use]
    pub fn status(&self) -> u16 {
        self.code.default_status()
    }
}

impl From<ErrorCode> for DomainError {
    fn from(code: ErrorCode) -> Self {
        Self {
            code,
            message: code.title().to_string(),
        }
    }
}

/// OpenAI-style error body: `{ "error": { message, type, param?, code? } }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiErrorBody {
    error: OpenAiError,
}

/// The inner `error` object of an OpenAI-style response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiError {
    message: String,
    #[serde(rename = "type")]
    r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    param: Option<String>,
    code: String,
}

/// RFC 9457 Problem Details body for the native management API:
/// `{ type, title, status, detail?, request_id?, code? }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProblemDetails {
    #[serde(rename = "type")]
    r#type: String,
    title: String,
    status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(rename = "request_id", skip_serializing_if = "Option::is_none")]
    r#request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn all_codes() -> Vec<ErrorCode> {
        vec![
            ErrorCode::Unauthorized,
            ErrorCode::Forbidden,
            ErrorCode::InvalidRequest,
            ErrorCode::UnsupportedField,
            ErrorCode::UnsupportedCapability,
            ErrorCode::BodyTooLarge,
            ErrorCode::HeaderTooLarge,
            ErrorCode::RateLimited,
            ErrorCode::ModelNotFound,
            ErrorCode::ModelNotLoaded,
            ErrorCode::ModelNotReady,
            ErrorCode::UpstreamUnavailable,
            ErrorCode::GpuMemoryInsufficient,
            ErrorCode::GpuOomLikely,
            ErrorCode::ResourceExhausted,
            ErrorCode::EvictionConflict,
            ErrorCode::PortConflict,
            ErrorCode::InvalidModel,
            ErrorCode::StartupTimeout,
            ErrorCode::ProcessCrash,
            ErrorCode::UpstreamProtocolError,
            ErrorCode::UpstreamTimeout,
            ErrorCode::UpstreamError,
            ErrorCode::InstanceNotFound,
            ErrorCode::InvalidStateTransition,
            ErrorCode::EndpointNotFound,
            ErrorCode::NotImplemented,
            ErrorCode::Internal,
        ]
    }

    #[test]
    fn code_str_is_unique_per_code() {
        let codes = all_codes();
        let mut strs: Vec<&str> = codes.iter().map(|c| c.code_str()).collect();
        let total = strs.len();
        strs.sort_unstable();
        strs.dedup();
        assert_eq!(strs.len(), total, "two codes share a code_str");
    }

    #[test]
    fn serde_form_matches_code_str() {
        for code in all_codes() {
            let wire = serde_json::to_string(&code).expect("serialize code");
            assert_eq!(wire, format!("\"{}\"", code.code_str()));
        }
    }

    #[test]
    fn default_statuses_are_http_valid_and_expected() {
        // Spot-check the statuses the plan pins (docs/api.md §2.2).
        let expected: &[(ErrorCode, u16)] = &[
            (ErrorCode::Unauthorized, 401),
            (ErrorCode::Forbidden, 403),
            (ErrorCode::InvalidRequest, 400),
            (ErrorCode::UnsupportedField, 422),
            (ErrorCode::BodyTooLarge, 413),
            (ErrorCode::RateLimited, 429),
            (ErrorCode::ModelNotFound, 404),
            (ErrorCode::ModelNotLoaded, 404),
            (ErrorCode::ModelNotReady, 503),
            (ErrorCode::GpuMemoryInsufficient, 409),
            (ErrorCode::PortConflict, 409),
            (ErrorCode::StartupTimeout, 504),
            (ErrorCode::UpstreamProtocolError, 502),
            (ErrorCode::EndpointNotFound, 404),
            (ErrorCode::Internal, 500),
        ];
        for (code, status) in expected {
            assert_eq!(code.default_status(), *status, "{code:?}");
        }
    }

    #[test]
    fn openai_type_stays_within_openai_vocabulary() {
        const VOCAB: &[&str] = &[
            "invalid_request_error",
            "authentication_error",
            "permission_error",
            "api_error",
            "server_error",
        ];
        for code in all_codes() {
            assert!(
                VOCAB.contains(&code.openai_type()),
                "{code:?} -> {} not in OpenAI vocabulary",
                code.openai_type()
            );
        }
    }

    #[test]
    fn urn_is_namespaced_by_problem_type() {
        let code = ErrorCode::GpuMemoryInsufficient;
        // The management `type` URN uses the category, not the machine code
        // (docs/api.md §2.2): code stays `gpu_memory_insufficient` while the
        // type is the `resource-exhausted` category.
        assert_eq!(code.urn(), "urn:model-serving:error:resource-exhausted");
        assert_eq!(code.code_str(), "gpu_memory_insufficient");
        assert_ne!(
            code.urn(),
            format!("urn:model-serving:error:{}", code.code_str())
        );
    }

    #[test]
    fn openai_type_follows_status_convention() {
        // Our-internal-fault.
        assert_eq!(ErrorCode::Internal.openai_type(), "server_error");
        // Request-shape problems (400/404/422/413/431) and auth (401/403).
        assert_eq!(
            ErrorCode::InvalidRequest.openai_type(),
            "invalid_request_error"
        );
        assert_eq!(
            ErrorCode::ModelNotLoaded.openai_type(),
            "invalid_request_error"
        );
        assert_eq!(
            ErrorCode::EndpointNotFound.openai_type(),
            "invalid_request_error"
        );
        assert_eq!(
            ErrorCode::Unauthorized.openai_type(),
            "authentication_error"
        );
        assert_eq!(ErrorCode::Forbidden.openai_type(), "permission_error");
        // Other client-visible failures (409/429/502/503/504, 501) -> api_error.
        assert_eq!(ErrorCode::EvictionConflict.openai_type(), "api_error");
        assert_eq!(ErrorCode::RateLimited.openai_type(), "api_error");
        assert_eq!(ErrorCode::ProcessCrash.openai_type(), "api_error");
        assert_eq!(ErrorCode::UpstreamUnavailable.openai_type(), "api_error");
        assert_eq!(ErrorCode::UpstreamProtocolError.openai_type(), "api_error");
    }

    #[test]
    fn resource_codes_share_one_problem_type_urn() {
        let urn = ErrorCode::GpuMemoryInsufficient.urn();
        assert_eq!(urn, "urn:model-serving:error:resource-exhausted");
        for code in [
            ErrorCode::GpuMemoryInsufficient,
            ErrorCode::GpuOomLikely,
            ErrorCode::ResourceExhausted,
        ] {
            assert_eq!(code.urn(), urn, "{code:?} should share resource-exhausted");
        }
    }

    #[test]
    fn domain_error_status_matches_code_default() {
        let err = DomainError::with_message(ErrorCode::ProcessCrash, "child exited 137");
        assert_eq!(err.status(), 503);
        assert_eq!(err.status(), err.code.default_status());
    }

    #[test]
    fn from_error_code_uses_title_as_message() {
        let err: DomainError = ErrorCode::ModelNotLoaded.into();
        assert_eq!(err.code, ErrorCode::ModelNotLoaded);
        assert_eq!(err.message, ErrorCode::ModelNotLoaded.title());
    }

    #[test]
    fn with_message_overrides_default() {
        let err =
            DomainError::with_message(ErrorCode::ModelNotFound, "model 'qwen-local' is not loaded");
        assert_eq!(err.message, "model 'qwen-local' is not loaded");
        assert_eq!(err.code, ErrorCode::ModelNotFound);
    }

    #[test]
    fn openai_error_body_shape_matches_api_doc() {
        let body = ErrorCode::ModelNotLoaded
            .to_openai_error("Model 'qwen-local' is not loaded", Some("model"));
        let v = serde_json::to_value(&body).expect("serialize body");
        assert_eq!(
            v,
            json!({
                "error": {
                    "message": "Model 'qwen-local' is not loaded",
                    "type": "invalid_request_error",
                    "param": "model",
                    "code": "model_not_loaded",
                }
            })
        );
    }

    #[test]
    fn openai_error_body_omits_param_when_absent() {
        let body = ErrorCode::RateLimited.to_openai_error("slow down", None);
        let v = serde_json::to_value(&body).expect("serialize body");
        assert!(v["error"].get("param").is_none(), "param must be omitted");
        assert_eq!(v["error"]["code"], "rate_limited");
    }

    #[test]
    fn problem_details_shape_matches_api_doc() {
        let body = ErrorCode::GpuMemoryInsufficient
            .to_problem_details("No idle instance can be evicted safely", Some("req-1"));
        let v = serde_json::to_value(&body).expect("serialize body");
        assert_eq!(
            v,
            json!({
                "type": "urn:model-serving:error:resource-exhausted",
                "title": "Insufficient GPU memory",
                "status": 409,
                "detail": "No idle instance can be evicted safely",
                "request_id": "req-1",
                "code": "gpu_memory_insufficient",
            })
        );
    }

    #[test]
    fn problem_details_omits_optional_fields_when_absent() {
        let body = ErrorCode::Internal.to_problem_details("boom", None);
        let v = serde_json::to_value(&body).expect("serialize body");
        assert!(v.get("request_id").is_none());
        assert_eq!(v["status"], 500);
    }

    #[test]
    fn error_implements_std_error_and_displays_message() {
        let err = DomainError::with_message(ErrorCode::ProcessCrash, "child exited 137");
        assert_eq!(err.to_string(), "child exited 137");
        let as_dyn: &dyn std::error::Error = &err;
        assert_eq!(as_dyn.to_string(), "child exited 137");
    }
}
