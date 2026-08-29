use axum::{
    Router,
    body::{Body, Bytes},
    extract::Request,
    http::{HeaderValue, StatusCode},
    response::Response,
    routing::post,
};
use http_body_util::BodyExt as _;
use model_launcher_api::{
    ApiModel, Authentication, Gateway, GatewayConfig, GatewayConfigError, GatewayLimits,
    LoadRequest, TokenStore,
};
use model_launcher_core::{
    CatalogIdentity, EngineCapabilities, EngineFuture, EngineProcess, EngineSpec, InferenceEngine,
    LaunchProfile, Lifecycle, ModelId, ModelKey, ModelRecord, ModelState,
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
        architecture: "llama".into(),
        quantization: "Q4_K_M".into(),
        params_string: "1B".into(),
    }
}
fn config(authentication: Authentication) -> GatewayConfig {
    GatewayConfig {
        bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        authentication,
        allow_insecure_lan: false,
        limits: GatewayLimits {
            max_body_bytes: 256,
            max_headers: 8,
            max_header_bytes: 256,
            max_connections: 2,
            startup_timeout: Duration::from_secs(1),
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
fn lan_without_auth_requires_override_and_warns() {
    let mut value = config(Authentication::Disabled);
    value.bind = "0.0.0.0:1234".parse().unwrap();
    assert!(matches!(
        value.validate(),
        Err(GatewayConfigError::UnauthenticatedLan)
    ));
    value.allow_insecure_lan = true;
    assert_eq!(
        value.validate().unwrap(),
        ["API is exposed to the LAN without authentication"]
    );
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
    assert!(loaded.get("config").is_none());
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
    assert_eq!(extra.status(), StatusCode::UNPROCESSABLE_ENTITY);

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
    gateway_config.limits.max_connections = 1;
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
