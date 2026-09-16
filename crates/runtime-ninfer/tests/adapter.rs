//! Fixture-driven tests for the `NInfer` adapter: golden argv, the typed
//! [`ProbeError::Incompatible`] taxonomy, and field validation.

use std::ffi::OsString;

use chrono::{DateTime, Utc};
use model_serving_domain::error::ErrorCode;
use model_serving_domain::model::{
    ArtifactKind, Capabilities, EngineConfig, KvCapacity, LoadConfig, Model, NinferEngineConfig,
    Runtime, RuntimeKind,
};
use model_serving_runtime::{DoctorStatus, ProbeConfig, ProbeError, ProbeSnapshot};
use model_serving_runtime_ninfer::{
    HEALTH_ENDPOINT, LaunchContext, MIN_SUPPORTED_VERSION, NinferAdapter,
};

const VERSION: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/runtimes/ninfer-serve-version.txt"
));
const HELP: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/runtimes/ninfer-serve-help.txt"
));

// The adapter rejects non-absolute executable paths, and a Unix `/opt/...`
// path is not absolute on Windows. Use a genuinely platform-absolute path so
// the fixture exercises the real validation on every platform.
#[cfg(windows)]
const EXECUTABLE: &str = r"C:\opt\ninfer\ninfer-serve.exe";
#[cfg(not(windows))]
const EXECUTABLE: &str = "/opt/ninfer/ninfer-serve";

#[cfg(windows)]
const MISSING_EXECUTABLE: &str = r"C:\definitely\not\a\real\ninfer-serve.exe";
#[cfg(not(windows))]
const MISSING_EXECUTABLE: &str = "/definitely/not/a/real/ninfer-serve";

const MODEL_PATH: &str = "/models/qwen-2.5-7b-instruct.ninfer";
const MODEL_KEY: &str = "qwen-2.5-7b-instruct";

fn model(kind: ArtifactKind) -> Model {
    Model {
        id: "model-1".to_owned(),
        key: MODEL_KEY.to_owned(),
        path: MODEL_PATH.to_owned(),
        artifact_kind: kind,
        size_bytes: 5_100_000_000,
        mtime: DateTime::<Utc>::from_timestamp(0, 0).expect("epoch is a valid timestamp"),
        display_name: None,
        default_runtime_id: None,
        default_load_config: None,
        metadata: None,
        deleted: false,
    }
}

fn runtime(fixed_args: &[&str]) -> Runtime {
    Runtime {
        id: "runtime-1".to_owned(),
        kind: RuntimeKind::Ninfer,
        executable_path: EXECUTABLE.to_owned(),
        enabled: true,
        version_text: None,
        capabilities: Capabilities::default(),
        fixed_args: fixed_args.iter().map(|arg| (*arg).to_owned()).collect(),
    }
}

fn base_config() -> LoadConfig {
    LoadConfig {
        context_length: 4096,
        max_concurrency: None,
        eval_batch_size: None,
        flash_attention: None,
        offload_kv_cache_to_gpu: None,
        n_gpu_layers: None,
        engine_config: None,
    }
}

fn with_ninfer(config: &LoadConfig, ninfer: NinferEngineConfig) -> LoadConfig {
    LoadConfig {
        engine_config: Some(EngineConfig {
            ninfer: Some(ninfer),
        }),
        ..config.clone()
    }
}

fn argv(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(|part| OsString::from(*part)).collect()
}

fn command_args(model: &Model, config: &LoadConfig, rt: &Runtime, port: u16) -> Vec<OsString> {
    let ctx = LaunchContext {
        model,
        config,
        runtime: rt,
        port,
    };
    match NinferAdapter::command(&ctx) {
        Ok(spec) => spec.args().to_vec(),
        Err(error) => panic!("expected a command, got {error:?}"),
    }
}

fn capability_error(version: &str, help: &str) -> ProbeError {
    match NinferAdapter::capabilities_from_text(std::path::Path::new(EXECUTABLE), version, help) {
        Err(error) => error,
        Ok(capabilities) => panic!("expected an incompatible build, got {capabilities:?}"),
    }
}

#[test]
fn capabilities_from_the_pinned_fixture() {
    let capabilities = match NinferAdapter::capabilities_from_text(
        std::path::Path::new(EXECUTABLE),
        VERSION,
        HELP,
    ) {
        Ok(capabilities) => capabilities,
        Err(error) => panic!("expected a compatible build, got {error:?}"),
    };

    assert!(capabilities.supports_chat_completions);
    // NInfer serves Responses and Chat Completions, but not legacy
    // `/v1/completions`.
    assert!(!capabilities.supports_completions);
    assert!(!capabilities.supports_embeddings);
    assert_eq!(
        capabilities.health_endpoint.as_deref(),
        Some(HEALTH_ENDPOINT)
    );
    assert_eq!(capabilities.version.as_deref(), Some(VERSION.trim()));
    assert_eq!(MIN_SUPPORTED_VERSION, (0, 9, 0));
}

#[test]
fn capabilities_reject_a_version_below_the_minimum() {
    let old = VERSION.replace("0.9.2", "0.8.0");
    match capability_error(&old, HELP) {
        ProbeError::Incompatible { message, .. } => {
            assert!(message.contains("0.8.0"));
            assert!(message.contains("0.9.0"));
        }
        other => panic!("expected Incompatible, got {other:?}"),
    }
}

#[test]
fn capabilities_reject_a_malformed_version() {
    match capability_error("no version here", HELP) {
        ProbeError::Incompatible { message, .. } => {
            assert!(message.contains("unrecognized ninfer-serve version text"));
        }
        other => panic!("expected Incompatible, got {other:?}"),
    }
}

#[test]
fn capabilities_reject_a_wrong_binary() {
    match capability_error(VERSION, "usage: some-other-tool [options]") {
        ProbeError::Incompatible { message, .. } => {
            assert!(message.contains("ninfer-serve"));
        }
        other => panic!("expected Incompatible, got {other:?}"),
    }
}

#[test]
fn capabilities_reject_a_missing_required_flag() {
    let help = HELP.replace("--lm-head-draft", "--not-a-real-flag");
    match capability_error(VERSION, &help) {
        ProbeError::Incompatible { message, .. } => {
            assert!(message.contains("--lm-head-draft"));
        }
        other => panic!("expected Incompatible, got {other:?}"),
    }
}

#[test]
fn full_config_produces_the_exact_golden_argv() {
    let config = LoadConfig {
        context_length: 32768,
        max_concurrency: Some(8),
        eval_batch_size: None,
        flash_attention: None,
        offload_kv_cache_to_gpu: None,
        n_gpu_layers: None,
        engine_config: Some(EngineConfig {
            ninfer: Some(NinferEngineConfig {
                kv_capacity: Some(KvCapacity::Value(262_144)),
                prefill_chunk: Some(1024),
                kv_dtype: Some("int8".to_owned()),
                spec: Some("mtp".to_owned()),
                draft_tokens: Some(8),
                lm_head_draft: Some(true),
                thinking: Some("preserve".to_owned()),
                vision: Some(true),
            }),
        }),
    };
    let rt = runtime(&["--admin-extra", "1"]);

    let args = command_args(&model(ArtifactKind::Ninfer), &config, &rt, 9000);

    assert_eq!(
        args,
        argv(&[
            MODEL_PATH,
            "--host",
            "127.0.0.1",
            "--port",
            "9000",
            "--model-id",
            MODEL_KEY,
            "--max-context",
            "32768",
            "--max-concurrency",
            "8",
            "--kv-capacity",
            "262144",
            "--prefill-chunk",
            "1024",
            "--kv-dtype",
            "int8",
            "--spec",
            "mtp",
            "--draft-tokens",
            "8",
            "--lm-head-draft",
            "--preserve-thinking",
            "--vision",
            "--admin-extra",
            "1",
        ])
    );
}

#[test]
fn minimal_config_produces_the_exact_golden_argv() {
    let args = command_args(
        &model(ArtifactKind::Ninfer),
        &base_config(),
        &runtime(&[]),
        8080,
    );

    assert_eq!(
        args,
        argv(&[
            MODEL_PATH,
            "--host",
            "127.0.0.1",
            "--port",
            "8080",
            "--model-id",
            MODEL_KEY,
            "--max-context",
            "4096",
        ])
    );
}

#[test]
fn auto_kv_capacity_and_disabled_thinking_map_to_their_tokens() {
    let config = with_ninfer(
        &base_config(),
        NinferEngineConfig {
            kv_capacity: Some(KvCapacity::Auto),
            thinking: Some("disabled".to_owned()),
            ..NinferEngineConfig::default()
        },
    );

    let args = command_args(&model(ArtifactKind::Ninfer), &config, &runtime(&[]), 8080);

    assert!(args.contains(&OsString::from("auto")));
    assert!(args.contains(&OsString::from("--no-thinking")));
    assert!(!args.contains(&OsString::from("--preserve-thinking")));
}

#[test]
fn validate_rejects_a_non_ninfer_artifact() {
    let error = NinferAdapter::validate(&model(ArtifactKind::Gguf), &base_config())
        .expect_err("a .gguf artifact must be rejected");
    assert_eq!(error.code, ErrorCode::InvalidModel);
}

#[test]
fn validate_rejects_llama_only_fields() {
    for config in [
        LoadConfig {
            eval_batch_size: Some(512),
            ..base_config()
        },
        LoadConfig {
            flash_attention: Some(true),
            ..base_config()
        },
        LoadConfig {
            offload_kv_cache_to_gpu: Some(true),
            ..base_config()
        },
        LoadConfig {
            n_gpu_layers: Some(33),
            ..base_config()
        },
    ] {
        let error = NinferAdapter::validate(&model(ArtifactKind::Ninfer), &config)
            .expect_err("llama-only fields must be rejected, not ignored");
        assert_eq!(error.code, ErrorCode::UnsupportedField);
    }
}

#[test]
fn validate_rejects_an_unknown_kv_dtype() {
    let config = with_ninfer(
        &base_config(),
        NinferEngineConfig {
            kv_dtype: Some("float128".to_owned()),
            ..NinferEngineConfig::default()
        },
    );
    let error = NinferAdapter::validate(&model(ArtifactKind::Ninfer), &config)
        .expect_err("unknown kv_dtype must be rejected");
    assert_eq!(error.code, ErrorCode::UnsupportedField);
}

#[test]
fn validate_rejects_an_unknown_spec_mode() {
    let config = with_ninfer(
        &base_config(),
        NinferEngineConfig {
            spec: Some("quantum".to_owned()),
            ..NinferEngineConfig::default()
        },
    );
    let error = NinferAdapter::validate(&model(ArtifactKind::Ninfer), &config)
        .expect_err("unknown spec mode must be rejected");
    assert_eq!(error.code, ErrorCode::UnsupportedField);
}

#[test]
fn validate_rejects_draft_tokens_without_spec() {
    let config = with_ninfer(
        &base_config(),
        NinferEngineConfig {
            draft_tokens: Some(8),
            ..NinferEngineConfig::default()
        },
    );
    let error = NinferAdapter::validate(&model(ArtifactKind::Ninfer), &config)
        .expect_err("draft_tokens without spec must be rejected");
    assert!(error.message.contains("requires"));
}

#[test]
fn validate_rejects_an_unknown_thinking_mode() {
    let config = with_ninfer(
        &base_config(),
        NinferEngineConfig {
            thinking: Some("loud".to_owned()),
            ..NinferEngineConfig::default()
        },
    );
    let error = NinferAdapter::validate(&model(ArtifactKind::Ninfer), &config)
        .expect_err("unknown thinking mode must be rejected");
    assert_eq!(error.code, ErrorCode::UnsupportedField);
}

#[test]
fn command_rejects_a_runtime_of_the_wrong_kind() {
    let rt = Runtime {
        kind: RuntimeKind::LlamaCpp,
        ..runtime(&[])
    };
    let ctx = LaunchContext {
        model: &model(ArtifactKind::Ninfer),
        config: &base_config(),
        runtime: &rt,
        port: 8080,
    };
    let error = NinferAdapter::command(&ctx).expect_err("wrong runtime kind must be rejected");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("LlamaCpp"));
}

#[test]
fn command_rejects_fixed_args_that_override_owned_flags() {
    let cases: &[&[&str]] = &[
        &["--host", "0.0.0.0"],
        &["--host=0.0.0.0"],
        &["--port", "9"],
        &["--port=9"],
        &["--model-id", "evil"],
        &["--model-id=evil"],
        &["--max-context", "1"],
        &["--max-context=1"],
        &["--no-thinking"],
        &["--vision"],
    ];
    for fixed in cases {
        let rt = runtime(fixed);
        let ctx = LaunchContext {
            model: &model(ArtifactKind::Ninfer),
            config: &base_config(),
            runtime: &rt,
            port: 8080,
        };
        let error = NinferAdapter::command(&ctx).expect_err("owned flag override must be rejected");
        assert_eq!(error.code, ErrorCode::UnsupportedField, "case {fixed:?}");
        assert!(error.message.contains("collides"), "case {fixed:?}");
    }
}

#[test]
fn command_rejects_a_relative_executable_path() {
    for relative in ["ninfer-serve", "", "./ninfer-serve"] {
        let rt = Runtime {
            executable_path: relative.to_owned(),
            ..runtime(&[])
        };
        let ctx = LaunchContext {
            model: &model(ArtifactKind::Ninfer),
            config: &base_config(),
            runtime: &rt,
            port: 8080,
        };
        let error =
            NinferAdapter::command(&ctx).expect_err("a relative executable must be rejected");
        assert_eq!(error.code, ErrorCode::InvalidRequest, "path {relative:?}");
        assert!(error.message.contains("absolute"), "path {relative:?}");
    }
}

#[test]
fn command_keeps_harmless_fixed_args_in_order() {
    let rt = runtime(&["--admin-extra", "1", "--tuning=fast"]);
    let args = command_args(&model(ArtifactKind::Ninfer), &base_config(), &rt, 8080);

    assert!(
        args.ends_with(&argv(&["--admin-extra", "1", "--tuning=fast"])),
        "harmless fixed args must be preserved verbatim and last: {args:?}"
    );
}

fn snapshot(version: &str, help: &str) -> ProbeSnapshot {
    ProbeSnapshot {
        executable: std::path::PathBuf::from(EXECUTABLE),
        version_text: version.trim().to_owned(),
        help_text: help.trim().to_owned(),
        capabilities: Capabilities::default(),
    }
}

#[test]
fn diagnose_snapshot_reports_a_compatible_engine_as_ready() {
    let report = NinferAdapter::diagnose_snapshot(&snapshot(VERSION, HELP));
    assert_eq!(report.status, DoctorStatus::Ready);
    assert_eq!(report.code, "runtime_ready");
    assert!(report.capabilities.supports_chat_completions);
    assert!(!report.capabilities.supports_completions);
}

#[test]
fn diagnose_snapshot_reports_an_old_engine_as_incompatible() {
    let old = VERSION.replace("0.9.2", "0.8.0");
    let report = NinferAdapter::diagnose_snapshot(&snapshot(&old, HELP));
    assert_eq!(report.status, DoctorStatus::Incompatible);
    assert_eq!(report.code, "runtime_incompatible");
    assert_eq!(report.stage, None);
    assert!(report.message.contains("0.8.0"));
}

#[test]
fn diagnose_snapshot_reports_a_wrong_engine_as_incompatible() {
    let report = NinferAdapter::diagnose_snapshot(&snapshot(VERSION, "usage: some-other-tool"));
    assert_eq!(report.status, DoctorStatus::Incompatible);
    assert_eq!(report.code, "runtime_incompatible");
}

#[tokio::test]
async fn diagnose_reports_a_missing_executable_as_missing() {
    let report = NinferAdapter::diagnose(
        std::path::Path::new(MISSING_EXECUTABLE),
        &ProbeConfig::new(),
    )
    .await;
    assert_eq!(report.status, DoctorStatus::Missing);
    assert_eq!(report.code, "runtime_missing");
}
