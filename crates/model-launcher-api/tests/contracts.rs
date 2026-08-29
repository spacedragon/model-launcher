use axum::{
    Router,
    body::{Body, Bytes},
    extract::Request,
    http::{HeaderValue, StatusCode},
    response::Response,
    routing::post,
};
use fake_llama_server::FakeServer;
use http_body_util::BodyExt as _;
use model_launcher_api::{
    ApiModel, Authentication, Gateway, GatewayConfig, GatewayLimits, LoadRequest, TokenStore,
};
use model_launcher_core::{
    AppError, CatalogIdentity, EngineCapabilities, EngineFuture, EngineProcess, EngineSpec,
    InferenceEngine, LaunchProfile, Lifecycle, ModelId, ModelKey, ModelRecord, ModelState,
};
use serde_json::{Value, json};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

struct ReadyEngine;
impl InferenceEngine for ReadyEngine {
    fn spec(&self) -> EngineFuture<'_, EngineSpec> {
        Box::pin(async {
            Ok(EngineSpec {
                id: "fake".into(),
                display_name: "Fake".into(),
                version: "1".into(),
            })
        })
    }
    fn probe_capabilities(&self) -> EngineFuture<'_, EngineCapabilities> {
        Box::pin(async { Ok(EngineCapabilities::default()) })
    }
    fn validate_launch<'a>(
        &'a self,
        _: &'a ModelRecord,
        _: &'a model_launcher_core::LaunchSettings,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn spawn<'a>(
        &'a self,
        _: &'a ModelRecord,
        _: &'a model_launcher_core::LaunchSettings,
    ) -> EngineFuture<'a, Box<dyn EngineProcess>> {
        Box::pin(async { Ok(Box::new(ReadyProcess) as Box<dyn EngineProcess>) })
    }
}
struct ReadyProcess;
impl EngineProcess for ReadyProcess {
    fn wait_ready(&mut self, _: Duration) -> EngineFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn check_health(&mut self) -> EngineFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
    fn graceful_shutdown(&mut self) -> EngineFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn force_shutdown(&mut self) -> EngineFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn wait_for_exit(&mut self) -> EngineFuture<'_, i32> {
        Box::pin(std::future::pending())
    }
}
struct PendingEngine;
impl InferenceEngine for PendingEngine {
    fn spec(&self) -> EngineFuture<'_, EngineSpec> {
        ReadyEngine.spec()
    }
    fn probe_capabilities(&self) -> EngineFuture<'_, EngineCapabilities> {
        ReadyEngine.probe_capabilities()
    }
    fn validate_launch<'a>(
        &'a self,
        model: &'a ModelRecord,
        settings: &'a model_launcher_core::LaunchSettings,
    ) -> EngineFuture<'a, ()> {
        ReadyEngine.validate_launch(model, settings)
    }
    fn spawn<'a>(
        &'a self,
        _: &'a ModelRecord,
        _: &'a model_launcher_core::LaunchSettings,
    ) -> EngineFuture<'a, Box<dyn EngineProcess>> {
        Box::pin(std::future::pending())
    }
}
struct GateEngine {
    spawns: Arc<std::sync::atomic::AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
impl InferenceEngine for GateEngine {
    fn spec(&self) -> EngineFuture<'_, EngineSpec> {
        ReadyEngine.spec()
    }
    fn probe_capabilities(&self) -> EngineFuture<'_, EngineCapabilities> {
        ReadyEngine.probe_capabilities()
    }
    fn validate_launch<'a>(
        &'a self,
        m: &'a ModelRecord,
        s: &'a model_launcher_core::LaunchSettings,
    ) -> EngineFuture<'a, ()> {
        ReadyEngine.validate_launch(m, s)
    }
    fn spawn<'a>(
        &'a self,
        _: &'a ModelRecord,
        _: &'a model_launcher_core::LaunchSettings,
    ) -> EngineFuture<'a, Box<dyn EngineProcess>> {
        Box::pin(async move {
            self.spawns
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.entered.notify_one();
            self.release.notified().await;
            Ok(Box::new(ReadyProcess) as Box<dyn EngineProcess>)
        })
    }
}
struct FailureEngine;
impl InferenceEngine for FailureEngine {
    fn spec(&self) -> EngineFuture<'_, EngineSpec> {
        ReadyEngine.spec()
    }
    fn probe_capabilities(&self) -> EngineFuture<'_, EngineCapabilities> {
        ReadyEngine.probe_capabilities()
    }
    fn validate_launch<'a>(
        &'a self,
        _: &'a ModelRecord,
        _: &'a model_launcher_core::LaunchSettings,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async {
            Err(AppError::ModelLoadFailed(Box::new(std::io::Error::other(
                "fake failure",
            ))))
        })
    }
    fn spawn<'a>(
        &'a self,
        _: &'a ModelRecord,
        _: &'a model_launcher_core::LaunchSettings,
    ) -> EngineFuture<'a, Box<dyn EngineProcess>> {
        unreachable!()
    }
}
struct CaptureEngine(Arc<Mutex<Option<ModelRecord>>>);
impl InferenceEngine for CaptureEngine {
    fn spec(&self) -> EngineFuture<'_, EngineSpec> {
        ReadyEngine.spec()
    }
    fn probe_capabilities(&self) -> EngineFuture<'_, EngineCapabilities> {
        ReadyEngine.probe_capabilities()
    }
    fn validate_launch<'a>(
        &'a self,
        _: &'a ModelRecord,
        _: &'a model_launcher_core::LaunchSettings,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn spawn<'a>(
        &'a self,
        model: &'a ModelRecord,
        _: &'a model_launcher_core::LaunchSettings,
    ) -> EngineFuture<'a, Box<dyn EngineProcess>> {
        *self.0.lock().unwrap() = Some(model.clone());
        Box::pin(async { Ok(Box::new(ReadyProcess) as Box<dyn EngineProcess>) })
    }
}
fn api_model() -> ApiModel {
    ApiModel {
        record: ModelRecord {
            id: ModelId::from_uuid(Uuid::from_u128(1)),
            key: ModelKey::parse("acme/tiny").unwrap(),
            display_name: "Tiny".into(),
            path: PathBuf::from("tiny.gguf"),
            file_identity: CatalogIdentity::Unavailable,
            size_bytes: 42,
            state: ModelState::Available,
            launch_profile: LaunchProfile::default(),
        },
        publisher: "Acme".into(),
        architecture: Some("llama".into()),
        quantization: Some("Q4_K_M".into()),
        params_string: Some("1B".into()),
        capabilities: EngineCapabilities {
            context_length: true,
            batch_size: true,
            flash_attention: true,
            ..EngineCapabilities::default()
        },
    }
}
fn other_model() -> ApiModel {
    let mut model = api_model();
    model.record.id = ModelId::from_uuid(Uuid::from_u128(2));
    model.record.key = ModelKey::parse("acme/other").unwrap();
    model.record.display_name = "Other".into();
    model
}
fn config(authentication: Authentication) -> GatewayConfig {
    GatewayConfig {
        bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        authentication,
        limits: GatewayLimits {
            max_body_bytes: 256,
            max_headers: 8,
            max_header_bytes: 256,
            max_connections: 2,
            max_in_flight_requests: 2,
            startup_timeout: Duration::from_secs(1),
            shutdown_grace: Duration::from_secs(1),
        },
    }
}
async fn start(
    authentication: Authentication,
    upstream: String,
) -> (Lifecycle, model_launcher_api::GatewayServer) {
    let lifecycle = Lifecycle::spawn(Arc::new(ReadyEngine));
    let endpoint = Arc::new(upstream);
    let gateway = Gateway::new(
        config(authentication),
        vec![api_model()],
        lifecycle.handle(),
        Arc::new(move |_| Some((*endpoint).clone())),
    )
    .unwrap();
    let server = gateway.start().await.unwrap();
    (lifecycle, server)
}
async fn stop(lifecycle: Lifecycle, server: model_launcher_api::GatewayServer) {
    server.stop().await.unwrap();
    lifecycle.handle().shutdown().await.unwrap();
    lifecycle.wait_for_termination().await;
}

#[test]
fn load_contract_rejects_unknown_fields_and_distinguishes_omitted_and_null() {
    let omitted: LoadRequest = serde_json::from_value(json!({"model":"acme/tiny"})).unwrap();
    let null: LoadRequest =
        serde_json::from_value(json!({"model":"acme/tiny","context_length":null})).unwrap();
    assert_eq!(omitted.context_length, null.context_length);
    assert!(serde_json::from_value::<LoadRequest>(json!({"model":"x","gpu":99})).is_err());
}
#[test]
fn generated_tokens_persist_only_argon2_phc_hashes() {
    let mut store = TokenStore::default();
    let created = store.create().unwrap();
    assert!(store.verify(&created.plaintext));
    assert!(!store.verify("wrong"));
    assert!(store.phc_hashes()[0].starts_with("$argon2"));
    assert!(!store.phc_hashes()[0].contains(&created.plaintext));
    assert!(
        TokenStore::from_phc_hashes(store.phc_hashes().to_vec())
            .unwrap()
            .verify(&created.plaintext)
    );
    assert!(!format!("{store:?}").contains(&created.plaintext));
}
#[test]
fn lan_without_auth_is_allowed_with_typed_warning() {
    let mut value = config(Authentication::Disabled);
    value.bind = "0.0.0.0:1234".parse().unwrap();
    assert_eq!(
        value.validate(),
        [model_launcher_api::GatewayWarning::UnauthenticatedNonLoopback]
    );
    value.bind = "127.0.0.1:1234".parse().unwrap();
    assert!(value.validate().is_empty());
    value.bind = "0.0.0.0:1234".parse().unwrap();
    value.authentication = Authentication::Tokens(Arc::new(TokenStore::default()));
    assert!(value.validate().is_empty());
}

#[tokio::test]
async fn lists_match_pinned_semantics() {
    let (lifecycle, server) = start(Authentication::Disabled, "http://127.0.0.1:1".into()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", server.local_addr());
    let listed: Value = client
        .get(format!("{base}/api/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/models/list-success.json")).unwrap();
    assert_eq!(listed, expected);
    let openai: Value = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        openai,
        json!({"object":"list","data":[{"id":"acme/tiny","object":"model","owned_by":"Acme"}]})
    );
    stop(lifecycle, server).await;
}

#[tokio::test]
async fn server_handle_keeps_stable_address_and_graceful_stop_releases_listener() {
    let (lifecycle, server) = start(Authentication::Disabled, "http://127.0.0.1:1".into()).await;
    let address = server.local_addr();
    assert_eq!(address, server.local_addr());
    server.stop().await.unwrap();
    let rebound = tokio::net::TcpListener::bind(address).await.unwrap();
    drop(rebound);
    lifecycle.handle().shutdown().await.unwrap();
    lifecycle.wait_for_termination().await;
}

#[tokio::test]
async fn listener_connection_cap_covers_idle_prebody_connections() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let lifecycle = Lifecycle::spawn(Arc::new(ReadyEngine));
    let mut gateway_config = config(Authentication::Disabled);
    gateway_config.limits.max_connections = 1;
    let gateway = Gateway::new(
        gateway_config,
        vec![api_model()],
        lifecycle.handle(),
        Arc::new(|_| None),
    )
    .unwrap();
    let server = gateway.start().await.unwrap();
    let first = tokio::net::TcpStream::connect(server.local_addr())
        .await
        .unwrap();
    first.writable().await.unwrap();
    let mut second = tokio::net::TcpStream::connect(server.local_addr())
        .await
        .unwrap();
    second
        .write_all(b"GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut byte = [0_u8; 1];
    assert_eq!(second.read(&mut byte).await.unwrap(), 0);
    drop(first);
    let mut recovered = tokio::net::TcpStream::connect(server.local_addr())
        .await
        .unwrap();
    recovered
        .write_all(b"GET /v1/models HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    assert!(recovered.read(&mut byte).await.unwrap() > 0);
    drop(recovered);
    stop(lifecycle, server).await;
}

#[tokio::test]
async fn bounded_stop_aborts_hung_idle_connection_and_releases_port() {
    let upstream = FakeServer::spawn().await.unwrap();
    let lifecycle = Lifecycle::spawn(Arc::new(ReadyEngine));
    let mut gateway_config = config(Authentication::Disabled);
    gateway_config.limits.shutdown_grace = Duration::ZERO;
    let endpoint = upstream.base_url();
    let gateway = Gateway::new(
        gateway_config,
        vec![api_model()],
        lifecycle.handle(),
        Arc::new(move |_| Some(endpoint.clone())),
    )
    .unwrap();
    let server = gateway.start().await.unwrap();
    let address = server.local_addr();
    let request = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("http://{address}/v1/completions"))
            .header("x-fake-mode", "gate")
            .body(r#"{"model":"acme/tiny"}"#)
            .send()
            .await
    });
    upstream.control.gate_entered().await;
    let error = server.stop().await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    request.abort();
    let _ = request.await;
    let rebound = tokio::net::TcpListener::bind(address).await.unwrap();
    drop(rebound);
    lifecycle.handle().shutdown().await.unwrap();
    lifecycle.wait_for_termination().await;
    upstream.control.release_gate();
    upstream.stop().await.unwrap();
}

#[tokio::test]
async fn management_load_echo_unload_and_errors_match_contracts() {
    let (lifecycle, server) = start(Authentication::Disabled, "http://127.0.0.1:1".into()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", server.local_addr());
    let loaded = client
        .post(format!("{base}/api/v1/models/load"))
        .json(&json!({"model":"acme/tiny"}))
        .send()
        .await
        .unwrap();
    assert_eq!(loaded.status(), StatusCode::OK);
    let mut loaded: Value = loaded.json().await.unwrap();
    assert!(loaded.get("load_config").is_none());
    loaded["load_time_seconds"] = json!(0.0);
    let expected: Value = serde_json::from_str(include_str!("fixtures/load/success.json")).unwrap();
    assert_eq!(loaded, expected);

    let echoed = client
        .post(format!("{base}/api/v1/models/load"))
        .json(&json!({"model":"acme/tiny","context_length":4096,"flash_attention":true,"echo_load_config":true}))
        .send().await.unwrap();
    assert_eq!(echoed.status(), StatusCode::OK);
    let mut echoed: Value = echoed.json().await.unwrap();
    echoed["load_time_seconds"] = json!(0.0);
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/load/success-echo.json")).unwrap();
    assert_eq!(echoed, expected);

    let unknown = client
        .post(format!("{base}/api/v1/models/load"))
        .json(&json!({"model":"missing"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    let invalid = client
        .post(format!("{base}/api/v1/models/load"))
        .json(&json!({"model":"acme/tiny","context_length":0}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        invalid.json::<Value>().await.unwrap()["error"]["code"],
        "invalid_setting"
    );
    let extra = client
        .post(format!("{base}/api/v1/models/load"))
        .json(&json!({"model":"acme/tiny","gpu":1}))
        .send()
        .await
        .unwrap();
    assert_eq!(extra.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        extra.json::<Value>().await.unwrap()["error"]["code"],
        "invalid_request"
    );
    for unsupported in [
        json!({"model":"acme/tiny","num_experts":2}),
        json!({"model":"acme/tiny","offload_kv_cache_to_gpu":true}),
    ] {
        let response = client
            .post(format!("{base}/api/v1/models/load"))
            .json(&unsupported)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "invalid_setting"
        );
    }

    let mismatch = client
        .post(format!("{base}/api/v1/models/unload"))
        .json(&json!({"instance_id":"wrong"}))
        .send()
        .await
        .unwrap();
    assert_eq!(mismatch.status(), StatusCode::NOT_FOUND);
    let unloaded = client
        .post(format!("{base}/api/v1/models/unload"))
        .json(&json!({"instance_id":"00000000-0000-0000-0000-000000000001"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unloaded.status(), StatusCode::OK);
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/unload/success.json")).unwrap();
    assert_eq!(unloaded.json::<Value>().await.unwrap(), expected);
    stop(lifecycle, server).await;
}

#[tokio::test]
async fn supported_management_overrides_are_typed_and_applied_before_load() {
    let captured = Arc::new(Mutex::new(None));
    let lifecycle = Lifecycle::spawn(Arc::new(CaptureEngine(captured.clone())));
    let gateway = Gateway::new(
        config(Authentication::Disabled),
        vec![api_model()],
        lifecycle.handle(),
        Arc::new(|_| None),
    )
    .unwrap();
    let server = gateway.start().await.unwrap();
    let response=reqwest::Client::new().post(format!("http://{}/api/v1/models/load",server.local_addr())).json(&json!({"model":"acme/tiny","context_length":8192,"eval_batch_size":512,"flash_attention":true})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let model = captured.lock().unwrap().clone().unwrap();
    assert_eq!(
        model.launch_profile.settings.context_length.unwrap().get(),
        8192
    );
    assert_eq!(model.launch_profile.settings.batch_size.unwrap().get(), 512);
    assert_eq!(model.launch_profile.settings.flash_attention, Some(true));
    stop(lifecycle, server).await;
}

#[tokio::test]
async fn authentication_failure_is_uniform() {
    let mut tokens = TokenStore::default();
    let plaintext = tokens.create().unwrap().plaintext;
    let (lifecycle, server) = start(
        Authentication::Tokens(Arc::new(tokens)),
        "http://127.0.0.1:1".into(),
    )
    .await;
    let client = reqwest::Client::new();
    let url = format!("http://{}/v1/models", server.local_addr());
    for auth in [None, Some("Basic x"), Some("Bearer wrong")] {
        let mut request = client.get(&url);
        if let Some(value) = auth {
            request = request.header("authorization", value);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({"error":{"code":"unauthorized","message":"authentication failed"}})
        );
    }
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(plaintext)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    stop(lifecycle, server).await;
}

#[tokio::test]
async fn proxy_preserves_raw_bytes_and_safe_headers() {
    let captured = Arc::new(Mutex::new(None));
    let capture = captured.clone();
    let upstream = Router::new().route(
        "/v1/chat/completions",
        post(move |request: Request| {
            let capture = capture.clone();
            async move {
                let auth = request.headers().get("authorization").cloned();
                let safe = request.headers().get("x-safe").cloned();
                let nominated = request.headers().get("x-remove").cloned();
                let bytes = request.into_body().collect().await.unwrap().to_bytes();
                *capture.lock().unwrap() = Some((bytes, auth, safe, nominated));
                let mut response = Response::new(Body::from(Bytes::from_static(b"data:\x00x\n\n")));
                response.headers_mut().insert(
                    "content-type",
                    HeaderValue::from_static("text/event-stream"),
                );
                response
                    .headers_mut()
                    .insert("x-safe-response", HeaderValue::from_static("yes"));
                response
                    .headers_mut()
                    .insert("connection", HeaderValue::from_static("x-remove-response"));
                response
                    .headers_mut()
                    .insert("x-remove-response", HeaderValue::from_static("secret"));
                response
                    .headers_mut()
                    .insert("content-length", HeaderValue::from_static("9"));
                response
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, upstream).await });
    let mut tokens = TokenStore::default();
    let token = tokens.create().unwrap().plaintext;
    let (lifecycle, server) = start(
        Authentication::Tokens(Arc::new(tokens)),
        format!("http://{addr}"),
    )
    .await;
    let body = br#"{ "model" : "acme/tiny", "messages" : [ ] }"#;
    let response = reqwest::Client::new()
        .post(format!(
            "http://{}/v1/chat/completions",
            server.local_addr()
        ))
        .bearer_auth(token)
        .header("x-safe", "yes")
        .header("connection", "x-remove")
        .header("x-remove", "secret")
        .body(body.as_slice())
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.headers()["x-safe-response"], "yes");
    assert!(!response.headers().contains_key("x-remove-response"));
    assert_eq!(
        response.bytes().await.unwrap(),
        Bytes::from_static(b"data:\x00x\n\n")
    );
    let got = captured.lock().unwrap().take().unwrap();
    assert_eq!(got.0, body.as_slice());
    assert!(got.1.is_none());
    assert_eq!(got.2.unwrap(), "yes");
    assert!(got.3.is_none());
    stop(lifecycle, server).await;
    task.abort();
}

#[tokio::test]
async fn controllable_fake_upstream_preserves_split_non_utf8_sse_bytes() {
    let upstream = FakeServer::spawn().await.unwrap();
    let (lifecycle, server) = start(Authentication::Disabled, upstream.base_url()).await;
    let response = reqwest::Client::new()
        .post(format!(
            "http://{}/v1/chat/completions",
            server.local_addr()
        ))
        .header("x-fake-mode", "sse")
        .body(r#"{"model":"acme/tiny"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(
        response.bytes().await.unwrap(),
        Bytes::from_static(b"data: a\xff\x00\n\n")
    );
    assert_eq!(upstream.control.requests(), 1);
    assert_eq!(upstream.control.stream_drops(), 1);
    stop(lifecycle, server).await;
    upstream.stop().await.unwrap();
}

#[tokio::test]
async fn lifecycle_http_matrix_jit_same_running_busy_and_unknown() {
    let upstream = FakeServer::spawn().await.unwrap();
    let lifecycle = Lifecycle::spawn(Arc::new(ReadyEngine));
    let endpoint = upstream.base_url();
    let gateway = Gateway::new(
        config(Authentication::Disabled),
        vec![api_model(), other_model()],
        lifecycle.handle(),
        Arc::new(move |_| Some(endpoint.clone())),
    )
    .unwrap();
    let server = gateway.start().await.unwrap();
    let client = reqwest::Client::new();
    let url = format!("http://{}/v1/completions", server.local_addr());
    let first = tokio::spawn({
        let client = client.clone();
        let url = url.clone();
        async move {
            client
                .post(url)
                .header("x-fake-mode", "gate")
                .body(r#"{"model":"acme/tiny"}"#)
                .send()
                .await
        }
    });
    upstream.control.gate_entered().await;
    assert_eq!(lifecycle.handle().snapshot().in_flight, 1);
    let busy = client
        .post(&url)
        .body(r#"{"model":"acme/other"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(busy.status(), StatusCode::CONFLICT);
    assert_eq!(
        busy.json::<Value>().await.unwrap()["error"]["code"],
        "model_busy"
    );
    let unknown = client
        .post(&url)
        .body(r#"{"model":"missing"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    upstream.control.release_gate();
    assert_eq!(first.await.unwrap().unwrap().status(), StatusCode::OK);
    let same_running = client
        .post(&url)
        .body(r#"{"model":"acme/tiny"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(same_running.status(), StatusCode::OK);
    stop(lifecycle, server).await;
    upstream.stop().await.unwrap();
}

#[tokio::test]
async fn zero_startup_budget_returns_model_starting_with_retry_after() {
    let lifecycle = Lifecycle::spawn(Arc::new(PendingEngine));
    let mut gateway_config = config(Authentication::Disabled);
    gateway_config.limits.startup_timeout = Duration::ZERO;
    let gateway = Gateway::new(
        gateway_config,
        vec![api_model()],
        lifecycle.handle(),
        Arc::new(|_| Some("http://127.0.0.1:1".into())),
    )
    .unwrap();
    let server = gateway.start().await.unwrap();
    let response = reqwest::Client::new()
        .post(format!("http://{}/v1/completions", server.local_addr()))
        .body(r#"{"model":"acme/tiny"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["retry-after"], "1");
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "model_starting"
    );
    stop(lifecycle, server).await;
}

#[tokio::test]
async fn concurrent_same_model_http_jit_shares_one_spawn() {
    let upstream = FakeServer::spawn().await.unwrap();
    let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let lifecycle = Lifecycle::spawn(Arc::new(GateEngine {
        spawns: spawns.clone(),
        entered: entered.clone(),
        release: release.clone(),
    }));
    let endpoint = upstream.base_url();
    let gateway = Gateway::new(
        config(Authentication::Disabled),
        vec![api_model()],
        lifecycle.handle(),
        Arc::new(move |_| Some(endpoint.clone())),
    )
    .unwrap();
    let server = gateway.start().await.unwrap();
    let url = format!("http://{}/v1/completions", server.local_addr());
    let request = |url: String| {
        tokio::spawn(async move {
            reqwest::Client::new()
                .post(url)
                .body(r#"{"model":"acme/tiny"}"#)
                .send()
                .await
        })
    };
    let first = request(url.clone());
    let second = request(url);
    entered.notified().await;
    assert_eq!(spawns.load(std::sync::atomic::Ordering::SeqCst), 1);
    release.notify_waiters();
    assert_eq!(first.await.unwrap().unwrap().status(), StatusCode::OK);
    assert_eq!(second.await.unwrap().unwrap().status(), StatusCode::OK);
    assert_eq!(spawns.load(std::sync::atomic::Ordering::SeqCst), 1);
    stop(lifecycle, server).await;
    upstream.stop().await.unwrap();
}

#[tokio::test]
async fn lifecycle_load_failure_is_stable_503() {
    let lifecycle = Lifecycle::spawn(Arc::new(FailureEngine));
    let gateway = Gateway::new(
        config(Authentication::Disabled),
        vec![api_model()],
        lifecycle.handle(),
        Arc::new(|_| Some("http://127.0.0.1:1".into())),
    )
    .unwrap();
    let server = gateway.start().await.unwrap();
    let response = reqwest::Client::new()
        .post(format!("http://{}/v1/completions", server.local_addr()))
        .body(r#"{"model":"acme/tiny"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["retry-after"], "1");
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "model_load_failed"
    );
    stop(lifecycle, server).await;
}

#[tokio::test]
async fn unknown_and_limits_have_stable_statuses() {
    let (lifecycle, server) = start(Authentication::Disabled, "http://127.0.0.1:1".into()).await;
    let client = reqwest::Client::new();
    let url = format!("http://{}/v1/completions", server.local_addr());
    let unknown = client
        .post(&url)
        .json(&json!({"model":"missing"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        unknown.json::<Value>().await.unwrap(),
        json!({"error":{"code":"model_not_found","message":"model was not found"}})
    );
    assert_eq!(
        client
            .post(&url)
            .body(vec![b'x'; 257])
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let mut request = client.post(&url).body("{}");
    for index in 0..9 {
        request = request.header(format!("x-{index}"), "x");
    }
    assert_eq!(
        request.send().await.unwrap().status(),
        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
    );
    let bytes = client
        .post(&url)
        .header("x-large", "x".repeat(300))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(bytes.status(), StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
    stop(lifecycle, server).await;
}

#[tokio::test]
async fn active_response_holds_connection_permit_and_overload_is_stable() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let upstream = Router::new().route(
        "/v1/completions",
        post({
            let entered = entered.clone();
            let release = release.clone();
            move || {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    entered.notify_one();
                    release.notified().await;
                    "{}"
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let upstream_task = tokio::spawn(async move { axum::serve(listener, upstream).await });
    let lifecycle = Lifecycle::spawn(Arc::new(ReadyEngine));
    let mut gateway_config = config(Authentication::Disabled);
    gateway_config.limits.max_in_flight_requests = 1;
    let endpoint = format!("http://{upstream_addr}");
    let gateway = Gateway::new(
        gateway_config,
        vec![api_model()],
        lifecycle.handle(),
        Arc::new(move |_| Some(endpoint.clone())),
    )
    .unwrap();
    let server = gateway.start().await.unwrap();
    let url = format!("http://{}/v1/completions", server.local_addr());
    let first = tokio::spawn({
        let url = url.clone();
        async move {
            reqwest::Client::new()
                .post(url)
                .body(r#"{"model":"acme/tiny"}"#)
                .send()
                .await
        }
    });
    entered.notified().await;
    let overloaded = reqwest::Client::new()
        .post(url)
        .body(r#"{"model":"acme/tiny"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(overloaded.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(overloaded.headers()["retry-after"], "1");
    assert_eq!(
        overloaded.json::<Value>().await.unwrap(),
        json!({"error":{"code":"connection_limit","message":"too many active connections"}})
    );
    release.notify_one();
    assert_eq!(first.await.unwrap().unwrap().status(), StatusCode::OK);
    stop(lifecycle, server).await;
    upstream_task.abort();
}

struct DropSignal(Arc<tokio::sync::Notify>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

#[tokio::test]
async fn eject_active_stream_cancels_upstream_and_releases_lease_once() {
    let stream_started = Arc::new(tokio::sync::Notify::new());
    let upstream_dropped = Arc::new(tokio::sync::Notify::new());
    let upstream = Router::new().route(
        "/v1/chat/completions",
        post({
            let stream_started = stream_started.clone();
            let upstream_dropped = upstream_dropped.clone();
            move || {
                let stream_started = stream_started.clone();
                let guard = DropSignal(upstream_dropped.clone());
                async move {
                    let output =
                        futures_util::stream::unfold((Some(guard), false), move |(guard, sent)| {
                            let stream_started = stream_started.clone();
                            async move {
                                if !sent {
                                    stream_started.notify_one();
                                    Some((
                                        Ok::<_, std::io::Error>(Bytes::from_static(
                                            b"data: first\n\n",
                                        )),
                                        (guard, true),
                                    ))
                                } else {
                                    std::future::pending().await
                                }
                            }
                        });
                    let mut response = Response::new(Body::from_stream(output));
                    response.headers_mut().insert(
                        "content-type",
                        HeaderValue::from_static("text/event-stream"),
                    );
                    response
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let upstream_task = tokio::spawn(async move { axum::serve(listener, upstream).await });
    let lifecycle = Lifecycle::spawn(Arc::new(ReadyEngine));
    let endpoint = format!("http://{addr}");
    let gateway = Gateway::new(
        config(Authentication::Disabled),
        vec![api_model()],
        lifecycle.handle(),
        Arc::new(move |_| Some(endpoint.clone())),
    )
    .unwrap();
    let server = gateway.start().await.unwrap();
    let mut response = reqwest::Client::new()
        .post(format!(
            "http://{}/v1/chat/completions",
            server.local_addr()
        ))
        .body(r#"{"model":"acme/tiny"}"#)
        .send()
        .await
        .unwrap();
    stream_started.notified().await;
    assert_eq!(
        response.chunk().await.unwrap().unwrap(),
        Bytes::from_static(b"data: first\n\n")
    );
    assert_eq!(lifecycle.handle().snapshot().in_flight, 1);
    lifecycle.handle().eject().await.unwrap();
    let termination = response.chunk().await;
    assert!(termination.is_err() || termination.unwrap().is_none());
    upstream_dropped.notified().await;
    assert_eq!(lifecycle.handle().snapshot().in_flight, 0);
    drop(response);
    tokio::task::yield_now().await;
    assert_eq!(lifecycle.handle().snapshot().in_flight, 0);
    stop(lifecycle, server).await;
    upstream_task.abort();
}

#[tokio::test]
async fn client_disconnect_cancels_upstream_and_decrements_in_flight() {
    let dropped = Arc::new(tokio::sync::Notify::new());
    let entered = Arc::new(tokio::sync::Notify::new());
    let upstream = Router::new().route(
        "/v1/completions",
        post({
            let dropped = dropped.clone();
            let entered = entered.clone();
            move || {
                let guard = DropSignal(dropped.clone());
                let entered = entered.clone();
                async move {
                    entered.notify_one();
                    let stream = futures_util::stream::unfold(Some(guard), |guard| async move {
                        let _keep_alive = &guard;
                        std::future::pending::<
                            Option<(Result<Bytes, std::io::Error>, Option<DropSignal>)>,
                        >()
                        .await
                    });
                    Response::new(Body::from_stream(stream))
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream_task = tokio::spawn(async move { axum::serve(listener, upstream).await });
    let lifecycle = Lifecycle::spawn(Arc::new(ReadyEngine));
    let endpoint = format!("http://{address}");
    let gateway = Gateway::new(
        config(Authentication::Disabled),
        vec![api_model()],
        lifecycle.handle(),
        Arc::new(move |_| Some(endpoint.clone())),
    )
    .unwrap();
    let server = gateway.start().await.unwrap();
    let request = tokio::spawn({
        let url = format!("http://{}/v1/completions", server.local_addr());
        async move {
            reqwest::Client::new()
                .post(url)
                .body(r#"{"model":"acme/tiny"}"#)
                .send()
                .await
        }
    });
    entered.notified().await;
    assert_eq!(lifecycle.handle().snapshot().in_flight, 1);
    request.abort();
    dropped.notified().await;
    let mut snapshots = lifecycle.subscribe();
    while snapshots.borrow().in_flight != 0 {
        snapshots.changed().await.unwrap();
    }
    assert_eq!(snapshots.borrow().in_flight, 0);
    stop(lifecycle, server).await;
    upstream_task.abort();
}
