//! Integration tests for the llama.cpp lifecycle orchestrator.
//!
//! These tests use the `fake_llama_server` binary as a controllable child
//! process, exercising the full lifecycle: launch → readiness → identity →
//! inference → unload.
//!
//! Each test verifies:
//! - Correct error classification (terminal state / `FailureClass`)
//! - No orphan processes after completion
//! - Clean shutdown on all failure paths

use std::net::TcpListener;
use std::time::Duration;

use model_serving_domain::model::FailureClass;
use model_serving_runtime::{CommandSpec, SupervisorConfig, SupervisorError};
use model_serving_runtime_llamacpp::lifecycle::{
    LifecycleConfig, LifecycleError, LlamaCppLifecycle,
};

/// Absolute path to the fake `llama-server` binary built by Cargo.
const FAKE_BINARY: &str = env!("CARGO_BIN_EXE_fake_llama_server");

/// Find a free loopback port by binding to `:0` and releasing it.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind to a free port");
    listener.local_addr().expect("local address").port()
}

/// Cross-platform liveness check for a direct child PID.
#[cfg(windows)]
fn pid_present(pid: u32) -> Option<bool> {
    let output = std::process::Command::new("tasklist")
        .arg("/NH")
        .arg("/FO")
        .arg("CSV")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let wanted = pid.to_string();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields: Vec<&str> = line
            .split(',')
            .map(|field| field.trim_matches('"').trim())
            .collect();
        if fields.len() >= 2 && fields[1] == wanted {
            return Some(true);
        }
    }
    Some(false)
}

/// Verify that a child process is completely reaped and no longer exists in the OS.
fn assert_process_dead(pid: u32) {
    #[cfg(unix)]
    {
        #[allow(unsafe_code)]
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        assert_ne!(
            result, 0,
            "process {pid} should no longer exist (orphan check)"
        );
    }
    #[cfg(windows)]
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            match pid_present(pid) {
                Some(false) | None => break,
                Some(true) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "process {pid} should no longer exist (orphan check)"
                    );
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
    }
}

/// Build a [`CommandSpec`] for the fake llama-server with the given mode.
fn fake_spec(mode: &str, port: u16, alias: &str) -> CommandSpec {
    CommandSpec::new(FAKE_BINARY)
        .with_arg("--host")
        .with_arg("127.0.0.1")
        .with_arg("--port")
        .with_arg(port.to_string())
        .with_arg("--alias")
        .with_arg(alias)
        .with_arg("--model")
        .with_arg("/fake/model.gguf")
        .with_env("FAKE_LLAMA_MODE", mode)
}

/// Build a fast lifecycle config for testing.
fn fast_config() -> LifecycleConfig {
    LifecycleConfig {
        supervisor: SupervisorConfig {
            stdout_capacity: 8 * 1024,
            stderr_capacity: 8 * 1024,
            startup_timeout: Duration::from_secs(5),
            probe_timeout: Duration::from_millis(500),
            probe_interval: Duration::from_millis(50),
            shutdown_grace: Duration::from_millis(500),
            post_kill_timeout: Duration::from_secs(2),
            pipe_eof_timeout: Duration::from_secs(2),
        },
        http_timeout: Duration::from_secs(2),
        http_connect_timeout: Duration::from_millis(500),
    }
}

/// Build a very-short-deadline config for timeout tests.
fn timeout_config() -> LifecycleConfig {
    LifecycleConfig {
        supervisor: SupervisorConfig {
            stdout_capacity: 4 * 1024,
            stderr_capacity: 4 * 1024,
            startup_timeout: Duration::from_millis(800),
            probe_timeout: Duration::from_millis(200),
            probe_interval: Duration::from_millis(50),
            shutdown_grace: Duration::from_millis(300),
            post_kill_timeout: Duration::from_secs(2),
            pipe_eof_timeout: Duration::from_secs(2),
        },
        http_timeout: Duration::from_millis(500),
        http_connect_timeout: Duration::from_millis(200),
    }
}

// ── Successful lifecycle ─────────────────────────────────────────────

#[tokio::test]
async fn successful_lifecycle_launches_verifies_and_unloads() {
    let port = free_port();
    let spec = fake_spec("healthy", port, "test-model");
    let mut lifecycle =
        LlamaCppLifecycle::launch_from_spec(&spec, port, "test-model".into(), fast_config())
            .expect("spawn must succeed");

    assert!(lifecycle.pid() > 0);
    assert_eq!(lifecycle.port(), port);
    assert_eq!(lifecycle.model_key(), "test-model");

    lifecycle
        .wait_ready()
        .await
        .expect("readiness must succeed");
    lifecycle
        .verify_identity()
        .await
        .expect("identity must match");
    lifecycle
        .verify_inference()
        .await
        .expect("inference check must succeed");
    lifecycle.unload().await.expect("unload must succeed");
}

#[tokio::test]
async fn wait_ready_and_verify_combines_readiness_and_identity() {
    let port = free_port();
    let spec = fake_spec("healthy", port, "combined-model");
    let mut lifecycle =
        LlamaCppLifecycle::launch_from_spec(&spec, port, "combined-model".into(), fast_config())
            .expect("spawn must succeed");

    let result = lifecycle
        .wait_ready_and_verify()
        .await
        .expect("combined check must succeed");
    assert_eq!(result.port, port);
    assert_eq!(result.model_key, "combined-model");
    assert!(result.pid > 0);

    lifecycle.unload().await.expect("unload must succeed");
}

// ── Slow start (child needs time to become ready) ────────────────────

#[tokio::test]
async fn slow_start_becomes_ready_within_deadline() {
    let port = free_port();
    let mut spec = fake_spec("slow_start", port, "slow-model");
    spec = spec.with_env("FAKE_LLAMA_DELAY_MS", "200");
    let mut lifecycle =
        LlamaCppLifecycle::launch_from_spec(&spec, port, "slow-model".into(), fast_config())
            .expect("spawn must succeed");

    lifecycle
        .wait_ready()
        .await
        .expect("readiness must succeed even with a slow start");
    lifecycle.unload().await.expect("unload must succeed");
}

// ── Child early exit ─────────────────────────────────────────────────

#[tokio::test]
async fn child_early_exit_reports_exited_before_ready() {
    let port = free_port();
    let spec = fake_spec("crash_early", port, "crash-model");
    let mut lifecycle =
        LlamaCppLifecycle::launch_from_spec(&spec, port, "crash-model".into(), fast_config())
            .expect("spawn must succeed");

    let error = lifecycle
        .wait_ready()
        .await
        .expect_err("readiness must fail on crash");

    match &error {
        LifecycleError::Supervisor(SupervisorError::ExitedBeforeReady) => {}
        other => panic!("expected ExitedBeforeReady, got {other:?}"),
    }
    assert_eq!(error.failure_class(), FailureClass::ProcessCrash);
}

// ── Identity mismatch ────────────────────────────────────────────────

#[tokio::test]
async fn identity_mismatch_is_detected_and_shut_down() {
    let port = free_port();
    let spec = fake_spec("wrong_model", port, "expected-model");
    let mut lifecycle =
        LlamaCppLifecycle::launch_from_spec(&spec, port, "expected-model".into(), fast_config())
            .expect("spawn must succeed");

    lifecycle
        .wait_ready()
        .await
        .expect("readiness must succeed");

    let error = lifecycle
        .verify_identity()
        .await
        .expect_err("identity must fail for wrong model");

    match &error {
        LifecycleError::IdentityMismatch { expected, got } => {
            assert_eq!(expected, "expected-model");
            assert_eq!(got, &["wrong-model-id"]);
        }
        other => panic!("expected IdentityMismatch, got {other:?}"),
    }
    assert_eq!(error.failure_class(), FailureClass::ConfigError);
}

// ── Invalid model (validation before spawn) ──────────────────────────

#[tokio::test]
async fn invalid_model_artifact_is_rejected_before_spawn() {
    use chrono::{DateTime, Utc};
    use model_serving_domain::model::{
        ArtifactKind, Capabilities, LoadConfig, Model, Runtime, RuntimeKind,
    };
    use model_serving_runtime_llamacpp::LaunchContext;

    let model = Model {
        id: "model-1".to_owned(),
        key: "test".to_owned(),
        path: "/fake/model.ninfer".to_owned(),
        artifact_kind: ArtifactKind::Ninfer, // Wrong artifact kind!
        size_bytes: 100,
        mtime: DateTime::<Utc>::from_timestamp(0, 0).expect("epoch"),
        display_name: None,
        default_runtime_id: None,
        default_load_config: None,
        metadata: None,
        deleted: false,
    };
    let runtime = Runtime {
        id: "rt-1".to_owned(),
        kind: RuntimeKind::LlamaCpp,
        executable_path: FAKE_BINARY.to_owned(),
        enabled: true,
        version_text: None,
        capabilities: Capabilities::default(),
        fixed_args: vec![],
    };
    let config = LoadConfig {
        context_length: 4096,
        max_concurrency: None,
        eval_batch_size: None,
        flash_attention: None,
        offload_kv_cache_to_gpu: None,
        n_gpu_layers: None,
        engine_config: None,
    };
    let ctx = LaunchContext {
        model: &model,
        config: &config,
        runtime: &runtime,
        port: 8080,
    };
    let error = LlamaCppLifecycle::launch(&ctx, fast_config())
        .expect_err("non-GGUF model must be rejected");

    match &error {
        LifecycleError::Command(domain_error) => {
            assert_eq!(
                domain_error.code,
                model_serving_domain::error::ErrorCode::InvalidModel
            );
        }
        other => panic!("expected Command error, got {other:?}"),
    }
    assert_eq!(error.failure_class(), FailureClass::InvalidModel);
}

#[tokio::test]
async fn invalid_model_runtime_load_failure_is_deterministic_terminal() {
    let port = free_port();
    let spec = fake_spec("invalid_model", port, "bad-model");
    let mut lifecycle =
        LlamaCppLifecycle::launch_from_spec(&spec, port, "bad-model".into(), fast_config())
            .expect("spawn succeeds before child attempts model load");

    let pid = lifecycle.pid();
    let error = lifecycle
        .wait_ready()
        .await
        .expect_err("child must fail to load invalid model");

    match &error {
        LifecycleError::Supervisor(SupervisorError::ExitedBeforeReady) => {}
        other => panic!("expected ExitedBeforeReady, got {other:?}"),
    }

    // Stderr inspection classifies InvalidModel
    let classified = lifecycle.classify_lifecycle_error(&error);
    assert_eq!(classified, FailureClass::InvalidModel);

    // Verify child is cleaned up and not orphaned
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_process_dead(pid);
}

// ── Occupied port ────────────────────────────────────────────────────

#[tokio::test]
async fn occupied_port_causes_child_exit() {
    // Pre-bind a port so the fake server can't bind it
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    // Keep `listener` alive to hold the port

    let spec = fake_spec("healthy", port, "port-conflict");
    let mut lifecycle =
        LlamaCppLifecycle::launch_from_spec(&spec, port, "port-conflict".into(), fast_config())
            .expect("spawn must succeed (the child will fail to bind)");

    let pid = lifecycle.pid();
    let error = lifecycle
        .wait_ready()
        .await
        .expect_err("readiness must fail when port is occupied");

    // The child exits because it can't bind the port
    match &error {
        // Timeout is also acceptable if the child hung instead of exiting
        LifecycleError::Supervisor(
            SupervisorError::ExitedBeforeReady | SupervisorError::ReadinessTimeout,
        ) => {}
        other => panic!("expected ExitedBeforeReady or ReadinessTimeout, got {other:?}"),
    }

    // Must be a terminal error classification
    let class = error.failure_class();
    assert!(
        class == FailureClass::ProcessCrash || class == FailureClass::StartupTimeout,
        "expected ProcessCrash or StartupTimeout, got {class:?}"
    );

    // Stderr inspection classifies port conflict accurately
    let classified = lifecycle.classify_lifecycle_error(&error);
    assert_eq!(classified, FailureClass::PortConflict);

    drop(listener);

    // Verify child is cleaned up and not orphaned
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_process_dead(pid);
}

// ── Startup timeout ──────────────────────────────────────────────────

#[tokio::test]
async fn startup_timeout_with_never_ready_child() {
    let port = free_port();
    let spec = fake_spec("never_ready", port, "stuck-model");
    let mut lifecycle =
        LlamaCppLifecycle::launch_from_spec(&spec, port, "stuck-model".into(), timeout_config())
            .expect("spawn must succeed");

    let pid = lifecycle.pid();
    let error = lifecycle
        .wait_ready()
        .await
        .expect_err("readiness must timeout");

    match &error {
        LifecycleError::Supervisor(SupervisorError::ReadinessTimeout) => {}
        other => panic!("expected ReadinessTimeout, got {other:?}"),
    }
    assert_eq!(error.failure_class(), FailureClass::StartupTimeout);
    assert_eq!(
        lifecycle.classify_lifecycle_error(&error),
        FailureClass::StartupTimeout
    );

    // Verify child is cleaned up and not orphaned
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_process_dead(pid);
}

// ── No orphan processes ──────────────────────────────────────────────

#[tokio::test]
async fn no_orphan_after_successful_lifecycle() {
    let port = free_port();
    let spec = fake_spec("healthy", port, "orphan-test");
    let mut lifecycle =
        LlamaCppLifecycle::launch_from_spec(&spec, port, "orphan-test".into(), fast_config())
            .expect("spawn");

    let pid = lifecycle.pid();
    lifecycle.wait_ready().await.expect("ready");
    lifecycle.unload().await.expect("unload");

    // Give the OS a moment to clean up
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify the process no longer exists
    assert_process_dead(pid);
}

#[tokio::test]
async fn no_orphan_after_failed_lifecycle() {
    let port = free_port();
    let spec = fake_spec("never_ready", port, "orphan-fail");
    let mut lifecycle =
        LlamaCppLifecycle::launch_from_spec(&spec, port, "orphan-fail".into(), timeout_config())
            .expect("spawn");

    let pid = lifecycle.pid();
    let _ = lifecycle.wait_ready().await; // will timeout

    // Give the OS a moment to clean up
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_process_dead(pid);
}

// ── Fixture integration tests ────────────────────────────────────────

const HEALTH_AND_MODELS_FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/runtimes/health-and-models.json"
));

#[tokio::test]
async fn fixture_integration_matches_pinned_health_and_models_contract() {
    let fixture: serde_json::Value =
        serde_json::from_str(HEALTH_AND_MODELS_FIXTURE).expect("valid fixture JSON");

    assert_eq!(fixture["schema_version"], 1);
    assert_eq!(fixture["engine_versions"]["llamacpp"], "b5555");

    let cases = fixture["cases"].as_array().expect("cases array");
    let health_case = cases
        .iter()
        .find(|c| c["runtime"] == "llamacpp" && c["kind"] == "health")
        .expect("llamacpp health case");
    let models_case = cases
        .iter()
        .find(|c| c["runtime"] == "llamacpp" && c["kind"] == "models")
        .expect("llamacpp models case");

    // Parse bodies
    let health_body: serde_json::Value = serde_json::from_str(
        health_case["body"]
            .as_str()
            .expect("health body must be string"),
    )
    .expect("health body valid JSON");
    let models_body: serde_json::Value = serde_json::from_str(
        models_case["body"]
            .as_str()
            .expect("models body must be string"),
    )
    .expect("models body valid JSON");

    assert_eq!(health_body["status"], "ok");

    let fixture_model_key = models_body["data"][0]["id"]
        .as_str()
        .expect("pinned model key");
    assert_eq!(fixture_model_key, "qwen2.5-7b-instruct-q4_k_m");

    // Run full lifecycle matching fixture key
    let port = free_port();
    let spec = fake_spec("healthy", port, fixture_model_key);
    let mut lifecycle = LlamaCppLifecycle::launch_from_spec(
        &spec,
        port,
        fixture_model_key.to_owned(),
        fast_config(),
    )
    .expect("launch succeeds");

    let pid = lifecycle.pid();
    let load_res = lifecycle
        .wait_ready_and_verify()
        .await
        .expect("wait_ready_and_verify succeeds against pinned fixture shape");
    assert_eq!(load_res.model_key, fixture_model_key);
    assert_eq!(load_res.port, port);
    assert_eq!(load_res.pid, pid);

    lifecycle
        .verify_inference()
        .await
        .expect("inference check succeeds");
    lifecycle.unload().await.expect("unload succeeds");

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_process_dead(pid);
}

#[tokio::test]
async fn fixture_mismatch_detected_and_terminates_without_orphans() {
    let port = free_port();
    // Server advertises wrong-model-id, but client expects fixture model key
    let spec = fake_spec("wrong_model", port, "qwen2.5-7b-instruct-q4_k_m");
    let mut lifecycle = LlamaCppLifecycle::launch_from_spec(
        &spec,
        port,
        "qwen2.5-7b-instruct-q4_k_m".into(),
        fast_config(),
    )
    .expect("spawn succeeds");

    let pid = lifecycle.pid();
    lifecycle.wait_ready().await.expect("readiness succeeds");

    let error = lifecycle
        .verify_identity()
        .await
        .expect_err("identity verification must fail when server id mismatches");

    match error {
        LifecycleError::IdentityMismatch { expected, got } => {
            assert_eq!(expected, "qwen2.5-7b-instruct-q4_k_m");
            assert_eq!(got, vec!["wrong-model-id".to_string()]);
        }
        other => panic!("expected IdentityMismatch, got {other:?}"),
    }

    // Verify child was automatically shut down on error and is dead
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_process_dead(pid);
}

#[tokio::test]
async fn adapter_launch_convenience_executes_lifecycle() {
    use chrono::{DateTime, Utc};
    use model_serving_domain::model::{
        ArtifactKind, Capabilities, LoadConfig, Model, Runtime, RuntimeKind,
    };
    use model_serving_runtime_llamacpp::{LaunchContext, LlamaCppAdapter};

    let port = free_port();
    let model = Model {
        id: "m-1".to_owned(),
        key: "qwen2.5-7b-instruct-q4_k_m".to_owned(),
        path: "/fake/model.gguf".to_owned(),
        artifact_kind: ArtifactKind::Gguf,
        size_bytes: 1024,
        mtime: DateTime::<Utc>::from_timestamp(0, 0).expect("timestamp"),
        display_name: None,
        default_runtime_id: None,
        default_load_config: None,
        metadata: None,
        deleted: false,
    };
    let runtime = Runtime {
        id: "rt-1".to_owned(),
        kind: RuntimeKind::LlamaCpp,
        executable_path: FAKE_BINARY.to_owned(),
        enabled: true,
        version_text: None,
        capabilities: Capabilities::default(),
        fixed_args: vec![],
    };
    let config = LoadConfig {
        context_length: 2048,
        max_concurrency: None,
        eval_batch_size: None,
        flash_attention: None,
        offload_kv_cache_to_gpu: None,
        n_gpu_layers: None,
        engine_config: None,
    };
    let ctx = LaunchContext {
        model: &model,
        config: &config,
        runtime: &runtime,
        port,
    };

    let mut lifecycle = LlamaCppAdapter::launch(&ctx, fast_config()).expect("launch succeeds");
    let pid = lifecycle.pid();
    lifecycle.wait_ready().await.expect("ready");
    lifecycle.verify_identity().await.expect("identity");
    lifecycle.verify_inference().await.expect("inference");
    lifecycle.unload().await.expect("unload");

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_process_dead(pid);
}

// ── Inference check validation (malformed JSON, empty choices, empty content) ──

#[tokio::test]
async fn inference_rejects_malformed_json_and_cleans_up() {
    let port = free_port();
    let spec = fake_spec("inference_malformed_json", port, "inf-malformed-model");
    let mut lifecycle = LlamaCppLifecycle::launch_from_spec(
        &spec,
        port,
        "inf-malformed-model".into(),
        fast_config(),
    )
    .expect("spawn must succeed");

    let pid = lifecycle.pid();
    lifecycle
        .wait_ready()
        .await
        .expect("readiness must succeed");
    lifecycle
        .verify_identity()
        .await
        .expect("identity must succeed");

    let error = lifecycle
        .verify_inference()
        .await
        .expect_err("verify_inference must reject malformed JSON");

    match &error {
        LifecycleError::InferenceCheck(msg) => {
            assert!(
                msg.contains("malformed JSON"),
                "expected malformed JSON message, got: {msg}"
            );
        }
        other => panic!("expected InferenceCheck error, got {other:?}"),
    }

    // Verify child is cleaned up on error and not orphaned
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_process_dead(pid);
}

#[tokio::test]
async fn inference_rejects_empty_choices_and_cleans_up() {
    let port = free_port();
    let spec = fake_spec("inference_empty_choices", port, "inf-empty-choices-model");
    let mut lifecycle = LlamaCppLifecycle::launch_from_spec(
        &spec,
        port,
        "inf-empty-choices-model".into(),
        fast_config(),
    )
    .expect("spawn must succeed");

    let pid = lifecycle.pid();
    lifecycle
        .wait_ready()
        .await
        .expect("readiness must succeed");
    lifecycle
        .verify_identity()
        .await
        .expect("identity must succeed");

    let error = lifecycle
        .verify_inference()
        .await
        .expect_err("verify_inference must reject empty choices");

    match &error {
        LifecycleError::InferenceCheck(msg) => {
            assert!(
                msg.contains("empty 'choices'"),
                "expected empty choices message, got: {msg}"
            );
        }
        other => panic!("expected InferenceCheck error, got {other:?}"),
    }

    // Verify child is cleaned up on error and not orphaned
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_process_dead(pid);
}

#[tokio::test]
async fn inference_rejects_empty_completion_content_and_cleans_up() {
    let port = free_port();
    let spec = fake_spec("inference_empty_content", port, "inf-empty-content-model");
    let mut lifecycle = LlamaCppLifecycle::launch_from_spec(
        &spec,
        port,
        "inf-empty-content-model".into(),
        fast_config(),
    )
    .expect("spawn must succeed");

    let pid = lifecycle.pid();
    lifecycle
        .wait_ready()
        .await
        .expect("readiness must succeed");
    lifecycle
        .verify_identity()
        .await
        .expect("identity must succeed");

    let error = lifecycle
        .verify_inference()
        .await
        .expect_err("verify_inference must reject empty completion content");

    match &error {
        LifecycleError::InferenceCheck(msg) => {
            assert!(
                msg.contains("empty completion content"),
                "expected empty completion content message, got: {msg}"
            );
        }
        other => panic!("expected InferenceCheck error, got {other:?}"),
    }

    // Verify child is cleaned up on error and not orphaned
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_process_dead(pid);
}

#[tokio::test]
async fn inference_transport_failure_reports_http_error_and_cleans_up() {
    let port = free_port();
    let spec = fake_spec("healthy", port, "inf-http-fail-model");
    let mut lifecycle = LlamaCppLifecycle::launch_from_spec(
        &spec,
        port,
        "inf-http-fail-model".into(),
        fast_config(),
    )
    .expect("spawn must succeed");

    let pid = lifecycle.pid();
    lifecycle
        .wait_ready()
        .await
        .expect("readiness must succeed");
    lifecycle
        .verify_identity()
        .await
        .expect("identity must succeed");

    // Unload the child process so the loopback port is no longer listening.
    lifecycle.unload().await.expect("unload must succeed");

    let error = lifecycle
        .verify_inference()
        .await
        .expect_err("verify_inference must report Http error when child is unreachable");

    match &error {
        LifecycleError::Http(_) => {}
        other => panic!("expected Http error, got {other:?}"),
    }
    assert_eq!(error.failure_class(), FailureClass::Unknown);
    assert_eq!(
        lifecycle.classify_lifecycle_error(&error),
        FailureClass::Unknown
    );

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_process_dead(pid);
}

#[tokio::test]
async fn identity_transport_failure_reports_http_error_and_cleans_up() {
    let port = free_port();
    let spec = fake_spec("healthy", port, "id-http-fail-model");
    let mut lifecycle = LlamaCppLifecycle::launch_from_spec(
        &spec,
        port,
        "id-http-fail-model".into(),
        fast_config(),
    )
    .expect("spawn must succeed");

    let pid = lifecycle.pid();
    lifecycle
        .wait_ready()
        .await
        .expect("readiness must succeed");

    // Unload the child process so the loopback port is no longer listening.
    lifecycle.unload().await.expect("unload must succeed");

    let error = lifecycle
        .verify_identity()
        .await
        .expect_err("verify_identity must report Http error when child is unreachable");

    match &error {
        LifecycleError::Http(_) => {}
        other => panic!("expected Http error, got {other:?}"),
    }
    assert_eq!(error.failure_class(), FailureClass::Unknown);
    assert_eq!(
        lifecycle.classify_lifecycle_error(&error),
        FailureClass::Unknown
    );

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_process_dead(pid);
}

// ── Real-runtime smoke test (opt-in) ─────────────────────────────────

/// This test is opt-in: set `LLAMACPP_SMOKE_EXECUTABLE` and
/// `LLAMACPP_SMOKE_MODEL` environment variables to run it.
///
/// When both are set, runs 20 consecutive load/readiness/identity/inference/
/// unload cycles against a real `llama-server` and a real GGUF model.
///
/// # Environment variables
///
/// - `LLAMACPP_SMOKE_EXECUTABLE`: Absolute path to `llama-server`.
/// - `LLAMACPP_SMOKE_MODEL`: Absolute path to a `.gguf` file.
///
/// # Example
///
/// ```bash
/// LLAMACPP_SMOKE_EXECUTABLE=/usr/local/bin/llama-server \
/// LLAMACPP_SMOKE_MODEL=/mnt/d/models/tinyllama-1.1b-chat-v1.0.Q4_K_M.gguf \
/// cargo test --package model-serving-runtime-llamacpp \
///   --test lifecycle smoke_test_real_runtime -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "requires LLAMACPP_SMOKE_EXECUTABLE and LLAMACPP_SMOKE_MODEL environment variables"]
#[allow(clippy::too_many_lines)]
async fn smoke_test_real_runtime_20_cycles() {
    const CYCLES: usize = 20;

    use model_serving_domain::model::{
        ArtifactKind, Capabilities, LoadConfig, Model, Runtime, RuntimeKind,
    };
    use model_serving_runtime_llamacpp::LaunchContext;

    let executable =
        std::env::var("LLAMACPP_SMOKE_EXECUTABLE").expect("LLAMACPP_SMOKE_EXECUTABLE must be set");
    let model_path =
        std::env::var("LLAMACPP_SMOKE_MODEL").expect("LLAMACPP_SMOKE_MODEL must be set");

    // Verify the executable and model exist
    assert!(
        std::path::Path::new(&executable).exists(),
        "llama-server not found at {executable}"
    );
    assert!(
        std::path::Path::new(&model_path).exists(),
        "GGUF model not found at {model_path}"
    );

    let model = Model {
        id: "smoke-model".to_owned(),
        key: "smoke-test".to_owned(),
        path: model_path,
        artifact_kind: ArtifactKind::Gguf,
        size_bytes: 0,
        mtime: chrono::Utc::now(),
        display_name: None,
        default_runtime_id: None,
        default_load_config: None,
        metadata: None,
        deleted: false,
    };
    let runtime = Runtime {
        id: "smoke-runtime".to_owned(),
        kind: RuntimeKind::LlamaCpp,
        executable_path: executable,
        enabled: true,
        version_text: None,
        capabilities: Capabilities::default(),
        fixed_args: vec![],
    };
    let config = LoadConfig {
        context_length: 2048,
        max_concurrency: Some(1),
        eval_batch_size: None,
        flash_attention: None,
        offload_kv_cache_to_gpu: None,
        n_gpu_layers: None,
        engine_config: None,
    };

    let lifecycle_config = LifecycleConfig {
        supervisor: SupervisorConfig {
            startup_timeout: Duration::from_secs(120),
            probe_timeout: Duration::from_secs(3),
            probe_interval: Duration::from_millis(500),
            ..SupervisorConfig::default()
        },
        http_timeout: Duration::from_secs(30),
        http_connect_timeout: Duration::from_secs(5),
    };

    for cycle in 1..=CYCLES {
        eprintln!("=== Smoke cycle {cycle}/{CYCLES} ===");
        let port = free_port();
        let ctx = LaunchContext {
            model: &model,
            config: &config,
            runtime: &runtime,
            port,
        };

        let mut lifecycle = LlamaCppLifecycle::launch(&ctx, lifecycle_config.clone())
            .unwrap_or_else(|error| panic!("cycle {cycle}: launch failed: {error}"));

        let pid = lifecycle.pid();
        eprintln!("  pid={pid}, port={port}");

        lifecycle
            .wait_ready()
            .await
            .unwrap_or_else(|error| panic!("cycle {cycle}: readiness failed: {error}"));
        eprintln!("  ready");

        lifecycle
            .verify_identity()
            .await
            .unwrap_or_else(|error| panic!("cycle {cycle}: identity failed: {error}"));
        eprintln!("  identity verified");

        lifecycle
            .verify_inference()
            .await
            .unwrap_or_else(|error| panic!("cycle {cycle}: inference failed: {error}"));
        eprintln!("  inference ok");

        lifecycle
            .unload()
            .await
            .unwrap_or_else(|error| panic!("cycle {cycle}: unload failed: {error}"));
        eprintln!("  unloaded");

        // Verify child cleanup (no-orphan check)
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_process_dead(pid);
    }
    eprintln!("=== All {CYCLES} smoke cycles passed ===");
}
