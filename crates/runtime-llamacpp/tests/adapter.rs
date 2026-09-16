//! Fixture-driven tests for the llama.cpp adapter: golden argv and the
//! typed [`ProbeError::Incompatible`] taxonomy.

use std::ffi::OsString;

use chrono::{DateTime, Utc};
use model_serving_domain::error::ErrorCode;
use model_serving_domain::model::{
    ArtifactKind, Capabilities, EngineConfig, KvCapacity, LoadConfig, Model, NinferEngineConfig,
    Runtime, RuntimeKind,
};
use model_serving_runtime::{DoctorStatus, ProbeConfig, ProbeError, ProbeSnapshot};
use model_serving_runtime_llamacpp::{
    HEALTH_ENDPOINT, LaunchContext, LlamaCppAdapter, MIN_SUPPORTED_BUILD,
};

const VERSION: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/runtimes/llama-server-version.txt"
));
const HELP: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/runtimes/llama-server-help.txt"
));

// The adapter now rejects non-absolute executable paths, and a Unix `/opt/...`
// path is not absolute on Windows. Use a genuinely platform-absolute path so
// the fixture exercises the real validation on every platform.
#[cfg(windows)]
const EXECUTABLE: &str = r"C:\opt\llama\llama-server.exe";
#[cfg(not(windows))]
const EXECUTABLE: &str = "/opt/llama/llama-server";

#[cfg(windows)]
const MISSING_EXECUTABLE: &str = r"C:\definitely\not\a\real\llama-server.exe";
#[cfg(not(windows))]
const MISSING_EXECUTABLE: &str = "/definitely/not/a/real/llama-server";

const MODEL_PATH: &str = "/models/qwen2.5-7b-instruct-q4_k_m.gguf";
const MODEL_KEY: &str = "qwen2.5-7b-instruct-q4_k_m";

fn model(kind: ArtifactKind) -> Model {
    Model {
        id: "model-1".to_owned(),
        key: MODEL_KEY.to_owned(),
        path: MODEL_PATH.to_owned(),
        artifact_kind: kind,
        size_bytes: 4_400_000_000,
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
        kind: RuntimeKind::LlamaCpp,
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

fn argv(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(|part| OsString::from(*part)).collect()
}

fn expect_command(model: &Model, config: &LoadConfig, rt: &Runtime, port: u16) -> Vec<OsString> {
    let ctx = LaunchContext {
        model,
        config,
        runtime: rt,
        port,
    };
    match LlamaCppAdapter::command(&ctx) {
        Ok(spec) => spec.args().to_vec(),
        Err(error) => panic!("expected a command, got {error:?}"),
    }
}

#[test]
fn capabilities_from_the_pinned_fixture() {
    let capabilities = match LlamaCppAdapter::capabilities_from_text(
        std::path::Path::new(EXECUTABLE),
        VERSION,
        HELP,
    ) {
        Ok(capabilities) => capabilities,
        Err(error) => panic!("expected a compatible build, got {error:?}"),
    };

    assert!(capabilities.supports_chat_completions);
    assert!(capabilities.supports_completions);
    assert!(capabilities.supports_embeddings);
    assert_eq!(
        capabilities.health_endpoint.as_deref(),
        Some(HEALTH_ENDPOINT)
    );
    assert_eq!(capabilities.version.as_deref(), Some(VERSION.trim()));
    assert_eq!(MIN_SUPPORTED_BUILD, 5555);
}

#[test]
fn capabilities_reject_a_build_below_the_minimum() {
    let old = VERSION.replace("5555", "5000");
    match LlamaCppAdapter::capabilities_from_text(std::path::Path::new(EXECUTABLE), &old, HELP) {
        Err(ProbeError::Incompatible { message, .. }) => {
            assert!(message.contains("5000"));
            assert!(message.contains("b5555"));
        }
        other => panic!("expected Incompatible, got {other:?}"),
    }
}

#[test]
fn capabilities_reject_a_malformed_version() {
    match LlamaCppAdapter::capabilities_from_text(
        std::path::Path::new(EXECUTABLE),
        "this binary has no version line",
        HELP,
    ) {
        Err(ProbeError::Incompatible { message, .. }) => {
            assert!(message.contains("unrecognized llama.cpp version text"));
        }
        other => panic!("expected Incompatible, got {other:?}"),
    }
}

#[test]
fn capabilities_reject_a_missing_required_flag() {
    let help = HELP.replace("--flash-attn", "--not-a-real-flag");
    match LlamaCppAdapter::capabilities_from_text(std::path::Path::new(EXECUTABLE), VERSION, &help)
    {
        Err(ProbeError::Incompatible { message, .. }) => {
            assert!(message.contains("--flash-attn"));
        }
        other => panic!("expected Incompatible, got {other:?}"),
    }
}

#[test]
fn capabilities_reject_a_wrong_binary() {
    match LlamaCppAdapter::capabilities_from_text(
        std::path::Path::new(EXECUTABLE),
        VERSION,
        "usage: some-other-tool [options]",
    ) {
        Err(ProbeError::Incompatible { message, .. }) => {
            assert!(message.contains("--model"));
        }
        other => panic!("expected Incompatible, got {other:?}"),
    }
}

#[test]
fn full_config_produces_the_exact_golden_argv() {
    let config = LoadConfig {
        context_length: 8192,
        max_concurrency: Some(4),
        eval_batch_size: Some(512),
        flash_attention: Some(true),
        offload_kv_cache_to_gpu: Some(false),
        n_gpu_layers: Some(33),
        engine_config: None,
    };
    let model = model(ArtifactKind::Gguf);
    let rt = runtime(&["--admin-extra", "1"]);

    let args = expect_command(&model, &config, &rt, 8123);

    assert_eq!(
        args,
        argv(&[
            "--model",
            MODEL_PATH,
            "--host",
            "127.0.0.1",
            "--port",
            "8123",
            "--alias",
            MODEL_KEY,
            "--ctx-size",
            "8192",
            "--parallel",
            "4",
            "--batch-size",
            "512",
            "--flash-attn",
            "--no-kv-offload",
            "--n-gpu-layers",
            "33",
            "--admin-extra",
            "1",
        ])
    );
}

#[test]
fn minimal_config_produces_the_exact_golden_argv() {
    let model = model(ArtifactKind::Gguf);
    let rt = runtime(&[]);

    let args = expect_command(&model, &base_config(), &rt, 8080);

    assert_eq!(
        args,
        argv(&[
            "--model",
            MODEL_PATH,
            "--host",
            "127.0.0.1",
            "--port",
            "8080",
            "--alias",
            MODEL_KEY,
            "--ctx-size",
            "4096",
        ])
    );
}

#[test]
fn flash_false_and_kv_offload_true_omit_their_flags() {
    let config = LoadConfig {
        flash_attention: Some(false),
        offload_kv_cache_to_gpu: Some(true),
        ..base_config()
    };
    let model = model(ArtifactKind::Gguf);
    let rt = runtime(&[]);

    let args = expect_command(&model, &config, &rt, 8080);

    assert!(!args.contains(&OsString::from("--flash-attn")));
    assert!(!args.contains(&OsString::from("--no-kv-offload")));
}

#[test]
fn validate_rejects_a_non_gguf_artifact() {
    let error = LlamaCppAdapter::validate(&model(ArtifactKind::Ninfer), &base_config())
        .expect_err("a .ninfer artifact must be rejected");
    assert_eq!(
        error.code,
        model_serving_domain::error::ErrorCode::InvalidModel
    );
}

#[test]
fn validate_rejects_ninfer_engine_config() {
    let config = LoadConfig {
        engine_config: Some(EngineConfig {
            ninfer: Some(NinferEngineConfig {
                kv_capacity: Some(KvCapacity::Auto),
                ..NinferEngineConfig::default()
            }),
        }),
        ..base_config()
    };
    let error = LlamaCppAdapter::validate(&model(ArtifactKind::Gguf), &config)
        .expect_err("NInfer config must be rejected");
    assert_eq!(
        error.code,
        model_serving_domain::error::ErrorCode::UnsupportedField
    );
}

#[test]
fn command_propagates_a_validation_error() {
    let model = model(ArtifactKind::Ninfer);
    let rt = runtime(&[]);
    let ctx = LaunchContext {
        model: &model,
        config: &base_config(),
        runtime: &rt,
        port: 8080,
    };
    assert!(LlamaCppAdapter::command(&ctx).is_err());
}

#[test]
fn command_rejects_a_runtime_of_the_wrong_kind() {
    let rt = Runtime {
        kind: RuntimeKind::Ninfer,
        ..runtime(&[])
    };
    let ctx = LaunchContext {
        model: &model(ArtifactKind::Gguf),
        config: &base_config(),
        runtime: &rt,
        port: 8080,
    };
    let error = LlamaCppAdapter::command(&ctx).expect_err("wrong runtime kind must be rejected");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("Ninfer"));
}

#[test]
fn command_rejects_fixed_args_that_override_owned_flags() {
    let cases: &[&[&str]] = &[
        &["--model", "/tmp/evil.gguf"],
        &["--model=/tmp/evil.gguf"],
        // `--model` / `--gpu-layers` aliases must not bypass protection.
        &["-m", "/tmp/evil.gguf"],
        &["-m=/tmp/evil.gguf"],
        &["--gpu-layers", "0"],
        &["--gpu-layers=0"],
        &["-ngl", "0"],
        &["-c", "1"],
        &["-np", "1"],
        &["-b", "1"],
        &["-fa"],
        &["-nkvo"],
        &["-a", "evil"],
        &["--host", "0.0.0.0"],
        &["--host=0.0.0.0"],
        &["--port", "9"],
        &["--port=9"],
        &["--alias", "evil"],
        &["--alias=evil"],
        &["--ctx-size", "1"],
        &["--flash-attn"],
        &["--no-kv-offload"],
    ];
    for fixed in cases {
        let rt = runtime(fixed);
        let ctx = LaunchContext {
            model: &model(ArtifactKind::Gguf),
            config: &base_config(),
            runtime: &rt,
            port: 8080,
        };
        let error =
            LlamaCppAdapter::command(&ctx).expect_err("owned flag override must be rejected");
        assert_eq!(error.code, ErrorCode::UnsupportedField, "case {fixed:?}");
        assert!(error.message.contains("collides"), "case {fixed:?}");
    }
}

#[test]
fn command_rejects_a_relative_executable_path() {
    for relative in ["llama-server", "", "./llama-server"] {
        let rt = Runtime {
            executable_path: relative.to_owned(),
            ..runtime(&[])
        };
        let ctx = LaunchContext {
            model: &model(ArtifactKind::Gguf),
            config: &base_config(),
            runtime: &rt,
            port: 8080,
        };
        let error =
            LlamaCppAdapter::command(&ctx).expect_err("a relative executable must be rejected");
        assert_eq!(error.code, ErrorCode::InvalidRequest, "path {relative:?}");
        assert!(error.message.contains("absolute"), "path {relative:?}");
    }
}

#[test]
fn command_keeps_harmless_fixed_args_in_order() {
    let rt = runtime(&["--threads", "8", "--admin-extra=1"]);
    let args = expect_command(&model(ArtifactKind::Gguf), &base_config(), &rt, 8080);

    assert!(
        args.ends_with(&argv(&["--threads", "8", "--admin-extra=1"])),
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
    let report = LlamaCppAdapter::diagnose_snapshot(&snapshot(VERSION, HELP));
    assert_eq!(report.status, DoctorStatus::Ready);
    assert_eq!(report.code, "runtime_ready");
    assert!(report.capabilities.supports_chat_completions);
    assert!(report.capabilities.supports_completions);
}

#[test]
fn diagnose_snapshot_reports_an_old_engine_as_incompatible() {
    let old = VERSION.replace("5555", "5000");
    let report = LlamaCppAdapter::diagnose_snapshot(&snapshot(&old, HELP));
    assert_eq!(report.status, DoctorStatus::Incompatible);
    assert_eq!(report.code, "runtime_incompatible");
    assert_eq!(report.stage, None);
    assert!(report.message.contains("5000"));
}

#[test]
fn diagnose_snapshot_reports_a_wrong_engine_as_incompatible() {
    let report = LlamaCppAdapter::diagnose_snapshot(&snapshot(VERSION, "usage: some-other-tool"));
    assert_eq!(report.status, DoctorStatus::Incompatible);
    assert_eq!(report.code, "runtime_incompatible");
}

#[tokio::test]
async fn diagnose_reports_a_missing_executable_as_missing() {
    let report = LlamaCppAdapter::diagnose(
        std::path::Path::new(MISSING_EXECUTABLE),
        &ProbeConfig::new(),
    )
    .await;
    assert_eq!(report.status, DoctorStatus::Missing);
    assert_eq!(report.code, "runtime_missing");
}
