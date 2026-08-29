use model_launcher::{Service, ServiceOptions};
use model_launcher_api::{Authentication, GatewayConfig, GatewayLimits};
use model_launcher_core::{
    EngineCapabilities, EngineFuture, EngineProcess, EngineSpec, InferenceEngine, LaunchSettings,
    LifecycleState, ModelRecord,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Engine {
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}
impl InferenceEngine for Engine {
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
        Box::pin(async {
            Ok(EngineCapabilities {
                context_length: true,
                ..Default::default()
            })
        })
    }
    fn validate_launch<'a>(
        &'a self,
        _: &'a ModelRecord,
        _: &'a LaunchSettings,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn spawn<'a>(
        &'a self,
        _: &'a ModelRecord,
        _: &'a LaunchSettings,
    ) -> EngineFuture<'a, Box<dyn EngineProcess>> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        let stops = self.stops.clone();
        Box::pin(async move { Ok(Box::new(Process(stops)) as Box<dyn EngineProcess>) })
    }
}
struct Process(Arc<AtomicUsize>);
impl EngineProcess for Process {
    fn wait_ready(&mut self, _: Duration) -> EngineFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn check_health(&mut self) -> EngineFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn graceful_shutdown(&mut self) -> EngineFuture<'_, ()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn force_shutdown(&mut self) -> EngineFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn wait_for_exit(&mut self) -> EngineFuture<'_, i32> {
        Box::pin(std::future::pending())
    }
}

fn options(root: &std::path::Path, upstream: String) -> ServiceOptions {
    ServiceOptions {
        config_dir: root.join("config"),
        catalog_dir: root.join("models"),
        gateway: GatewayConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            authentication: Authentication::Disabled,
            limits: GatewayLimits {
                shutdown_grace: Duration::from_secs(1),
                ..Default::default()
            },
        },
        upstream,
        watch_catalog: false,
        shutdown_timeout: Duration::from_secs(2),
    }
}

#[tokio::test]
async fn scan_http_stream_eject_restart_and_idempotent_shutdown() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    std::fs::write(temp.path().join("models/tiny.gguf"), b"GGUFtiny").unwrap();
    let upstream = fake_llama_server::FakeServer::spawn().await.unwrap();
    let starts = Arc::new(AtomicUsize::new(0));
    let stops = Arc::new(AtomicUsize::new(0));
    let service = Service::start(
        options(temp.path(), upstream.base_url()),
        Arc::new(Engine {
            starts: starts.clone(),
            stops: stops.clone(),
        }),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.local_addr());
    let models: serde_json::Value = client
        .get(format!("{base}/api/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let key = models["models"][0]["key"].as_str().unwrap();
    let loaded: serde_json::Value = client
        .post(format!("{base}/api/v1/models/load"))
        .json(&serde_json::json!({"model":key,"context_length":4096,"echo_load_config":true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&serde_json::json!({"model":key,"prompt":"hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.bytes().await.unwrap(),
        format!(r#"{{"model":"{key}","prompt":"hello"}}"#)
    );
    let instance = &loaded["model_instance_id"];
    assert!(
        client
            .post(format!("{base}/api/v1/models/unload"))
            .json(&serde_json::json!({"instance_id":instance}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert_eq!(handle.snapshot().lifecycle.state, LifecycleState::Stopped);
    assert!(
        client
            .post(format!("{base}/api/v1/models/load"))
            .json(&serde_json::json!({"model":key,"context_length":4096,"echo_load_config":false}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let a = handle.clone();
    let b = handle.clone();
    let (left, right) = tokio::join!(a.shutdown(), b.shutdown());
    left.unwrap();
    right.unwrap();
    assert!(
        client
            .get(format!("{base}/api/v1/models"))
            .send()
            .await
            .is_err()
    );
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(stops.load(Ordering::SeqCst), 2);
    let restarted = Service::start(
        options(temp.path(), upstream.base_url()),
        Arc::new(Engine {
            starts: starts.clone(),
            stops: stops.clone(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(restarted.handle().snapshot().models.len(), 1);
    assert_eq!(
        restarted.handle().snapshot().lifecycle.state,
        LifecycleState::Stopped
    );
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(
        restarted.handle().snapshot().models[0]
            .launch_profile
            .settings
            .context_length
            .unwrap()
            .get(),
        4096
    );
    restarted.handle().shutdown().await.unwrap();
    upstream.stop().await.unwrap();
}
