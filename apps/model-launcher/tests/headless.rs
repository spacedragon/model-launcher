use model_launcher::{
    EngineSettingsManager, LauncherSettings, Service, ServiceError, ServiceOptions, ShutdownEvent,
    ShutdownPhase, WatcherBarrier, WatcherBarrierPoint,
};
use model_launcher_api::{Authentication, GatewayConfig, GatewayLimits, TokenStore};
use model_launcher_core::{
    ConfigDiagnosticKind, ConfigStore, EngineCapabilities, EngineFuture, EngineProcess, EngineSpec,
    InferenceEngine, LaunchSettings, LifecycleState, LogFilter, LogLevel, LogRecord, LogSource,
    LogStore, LogStoreLimits, ModelRecord,
};
use std::{
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

async fn models_status(address: std::net::SocketAddr, token: Option<&str>) -> reqwest::StatusCode {
    let client = reqwest::Client::new();
    let mut request = client.get(format!("http://{address}/v1/models"));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request.send().await.unwrap().status()
}

#[tokio::test]
async fn server_authentication_can_be_enabled_without_rebinding() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let service = Service::start(
        options(temp.path(), "http://127.0.0.1:1".into()),
        idle_engine(),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let address = handle.local_addr();
    handle
        .save_server_settings(address.ip(), address.port(), true)
        .await
        .unwrap();
    let token = handle.generate_token().await.unwrap();
    assert_eq!(handle.local_addr(), address);
    assert_eq!(
        models_status(address, None).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        models_status(address, Some(&token.plaintext)).await,
        reqwest::StatusCode::OK
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn server_rebind_moves_listener_and_persists_only_after_success() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let service = Service::start(
        options(temp.path(), "http://127.0.0.1:1".into()),
        idle_engine(),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let old = handle.local_addr();
    let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let new_port = reservation.local_addr().unwrap().port();
    drop(reservation);
    handle
        .save_server_settings(IpAddr::from([127, 0, 0, 1]), new_port, false)
        .await
        .unwrap();
    assert_eq!(
        models_status(handle.local_addr(), None).await,
        reqwest::StatusCode::OK
    );
    assert!(
        reqwest::Client::new()
            .get(format!("http://{old}/v1/models"))
            .send()
            .await
            .is_err()
    );
    let saved = ConfigStore::new(temp.path().join("config")).load().unwrap();
    assert_eq!(saved.port, new_port);
    handle.shutdown().await.unwrap();
    assert!(
        reqwest::get(format!("http://127.0.0.1:{new_port}/v1/models"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn occupied_server_rebind_keeps_old_listener_and_disk_settings() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let service = Service::start(
        options(temp.path(), "http://127.0.0.1:1".into()),
        idle_engine(),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let old = handle.local_addr();
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let occupied_port = occupied.local_addr().unwrap().port();
    assert!(
        handle
            .save_server_settings(old.ip(), occupied_port, false)
            .await
            .is_err()
    );
    assert_eq!(handle.local_addr(), old);
    assert_eq!(models_status(old, None).await, reqwest::StatusCode::OK);
    let saved = ConfigStore::new(temp.path().join("config")).load().unwrap();
    assert_ne!(saved.port, occupied_port);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn loopback_to_wildcard_rebind_can_keep_the_same_port() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let service = Service::start(
        options(temp.path(), "http://127.0.0.1:1".into()),
        idle_engine(),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let old = handle.local_addr();

    handle
        .save_server_settings(IpAddr::from([0, 0, 0, 0]), old.port(), false)
        .await
        .unwrap();
    assert_eq!(handle.local_addr(), ([0, 0, 0, 0], old.port()).into());
    assert_eq!(
        models_status(([127, 0, 0, 1], old.port()).into(), None).await,
        reqwest::StatusCode::OK
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_waits_for_in_progress_handoff_and_stops_replacement_listener() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let mut opts = options(temp.path(), String::new());
    opts.gateway.limits.shutdown_grace = Duration::from_millis(100);
    let service = Service::start(opts, idle_engine()).await.unwrap();
    let handle = service.handle();
    let old = handle.local_addr();
    let mut idle = tokio::net::TcpStream::connect(old).await.unwrap();
    use tokio::io::AsyncWriteExt as _;
    idle.write_all(
        b"POST /v1/completions HTTP/1.1\r\nhost: localhost\r\ncontent-length: 10\r\n\r\n",
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let save = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .save_server_settings(IpAddr::from([0, 0, 0, 0]), old.port(), false)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let shutdown = tokio::spawn({
        let handle = handle.clone();
        async move { handle.shutdown().await }
    });

    save.await.unwrap().unwrap();
    shutdown.await.unwrap().unwrap();
    assert!(
        reqwest::get(format!("http://127.0.0.1:{}/v1/models", old.port()))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn server_settings_reject_zero_port_without_changing_listener() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let service = Service::start(options(temp.path(), String::new()), idle_engine())
        .await
        .unwrap();
    let handle = service.handle();
    let old = handle.local_addr();
    assert!(
        handle
            .save_server_settings(old.ip(), 0, false)
            .await
            .is_err()
    );
    assert_eq!(handle.local_addr(), old);
    assert_eq!(models_status(old, None).await, reqwest::StatusCode::OK);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn rebound_server_model_lists_follow_later_catalog_rescans() {
    let temp = tempfile::tempdir().unwrap();
    let models = temp.path().join("models");
    std::fs::create_dir(&models).unwrap();
    std::fs::write(models.join("first.gguf"), b"GGUFfirst").unwrap();
    let service = Service::start(
        options(temp.path(), "http://127.0.0.1:1".into()),
        idle_engine(),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let new_port = reservation.local_addr().unwrap().port();
    drop(reservation);
    handle
        .save_server_settings(handle.local_addr().ip(), new_port, false)
        .await
        .unwrap();

    std::fs::write(models.join("second.gguf"), b"GGUFsecond").unwrap();
    handle.rescan(models).await.unwrap();

    let response: serde_json::Value =
        reqwest::get(format!("http://{}/v1/models", handle.local_addr()))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(response["data"].as_array().unwrap().len(), 2);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn server_rebind_rolls_back_replacement_when_persistence_fails() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let service = Service::start(
        options(temp.path(), "http://127.0.0.1:1".into()),
        idle_engine(),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let old = handle.local_addr();
    let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let new_port = reservation.local_addr().unwrap().port();
    drop(reservation);
    std::fs::remove_dir_all(temp.path().join("config")).unwrap();
    std::fs::write(temp.path().join("config"), b"not a directory").unwrap();

    assert!(
        handle
            .save_server_settings(old.ip(), new_port, false)
            .await
            .is_err()
    );
    assert_eq!(handle.local_addr(), old);
    assert_eq!(models_status(old, None).await, reqwest::StatusCode::OK);
    assert!(
        reqwest::get(format!("http://127.0.0.1:{new_port}/v1/models"))
            .await
            .is_err()
    );
    handle.shutdown().await.unwrap_err();
}

#[tokio::test]
async fn handoff_rollback_keeps_auth_policy_attached_to_restored_listener() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let service = Service::start(options(temp.path(), String::new()), idle_engine())
        .await
        .unwrap();
    let handle = service.handle();
    let old = handle.local_addr();
    std::fs::remove_dir_all(temp.path().join("config")).unwrap();
    std::fs::write(temp.path().join("config"), b"not a directory").unwrap();
    assert!(
        handle
            .save_server_settings(IpAddr::from([0, 0, 0, 0]), old.port(), false)
            .await
            .is_err()
    );
    assert_eq!(models_status(old, None).await, reqwest::StatusCode::OK);

    std::fs::remove_file(temp.path().join("config")).unwrap();
    std::fs::create_dir(temp.path().join("config")).unwrap();
    handle
        .save_server_settings(old.ip(), old.port(), true)
        .await
        .unwrap();
    let token = handle.generate_token().await.unwrap();
    assert_eq!(
        models_status(old, None).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        models_status(old, Some(&token.plaintext)).await,
        reqwest::StatusCode::OK
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn old_listener_stop_failure_does_not_report_applied_rebind_as_failed() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let mut opts = options(temp.path(), "http://127.0.0.1:1".into());
    opts.gateway.limits.shutdown_grace = Duration::ZERO;
    let service = Service::start(opts, idle_engine()).await.unwrap();
    let handle = service.handle();
    let old = handle.local_addr();
    let mut idle = tokio::net::TcpStream::connect(old).await.unwrap();
    use tokio::io::AsyncWriteExt as _;
    idle.write_all(
        b"POST /v1/completions HTTP/1.1\r\nhost: localhost\r\ncontent-length: 10\r\n\r\n",
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let new_port = reservation.local_addr().unwrap().port();
    drop(reservation);

    handle
        .save_server_settings(old.ip(), new_port, false)
        .await
        .unwrap();
    assert_eq!(handle.local_addr().port(), new_port);
    assert_eq!(
        models_status(handle.local_addr(), None).await,
        reqwest::StatusCode::OK
    );
    let _ = handle.shutdown().await;
}

struct Engine {
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

struct ProbeFailEngine {
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

struct RecordingSettings(Arc<std::sync::Mutex<Vec<String>>>);
#[async_trait::async_trait]
impl EngineSettingsManager for RecordingSettings {
    async fn validate(
        &self,
        distribution: &str,
        executable: &str,
    ) -> Result<EngineCapabilities, String> {
        self.0
            .lock()
            .unwrap()
            .push(format!("validate:{distribution}:{executable}"));
        Ok(EngineCapabilities {
            context_length: true,
            ..Default::default()
        })
    }
    fn apply(&self, distribution: String, executable: String) {
        self.0
            .lock()
            .unwrap()
            .push(format!("apply:{distribution}:{executable}"));
    }
}

struct BlockingSettings {
    calls: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    first_entered: tokio::sync::Notify,
    release_first: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl EngineSettingsManager for BlockingSettings {
    async fn validate(&self, _: &str, _: &str) -> Result<EngineCapabilities, String> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        if call == 0 {
            self.first_entered.notify_one();
            self.release_first.notified().await;
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(EngineCapabilities::default())
    }

    fn apply(&self, _: String, _: String) {}
}

#[tokio::test]
async fn launcher_settings_saves_are_serialized_across_async_validation() {
    let temp = tempfile::tempdir().unwrap();
    let initial = temp.path().join("models");
    let first_root = temp.path().join("first");
    let second_root = temp.path().join("second");
    for root in [&initial, &first_root, &second_root] {
        std::fs::create_dir(root).unwrap();
    }
    let settings = Arc::new(BlockingSettings {
        calls: AtomicUsize::new(0),
        active: AtomicUsize::new(0),
        max_active: AtomicUsize::new(0),
        first_entered: tokio::sync::Notify::new(),
        release_first: tokio::sync::Notify::new(),
    });
    let service = Service::start_with_desktop_dependencies(
        options(temp.path(), "http://127.0.0.1:1".into()),
        idle_engine(),
        LogStore::new(LogStoreLimits::new(10, 1024, 4)).unwrap(),
        settings.clone(),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let save = |catalog_directory: std::path::PathBuf, distribution: &str| LauncherSettings {
        catalog_directory,
        engine_distribution: distribution.into(),
        engine_executable: "/opt/llama-server".into(),
        default_launch_settings: LaunchSettings::default(),
    };
    let first = tokio::spawn({
        let handle = handle.clone();
        let settings = save(first_root, "First");
        async move { handle.save_launcher_settings(settings).await }
    });
    settings.first_entered.notified().await;
    let second = tokio::spawn({
        let handle = handle.clone();
        let settings = save(second_root.clone(), "Second");
        async move { handle.save_launcher_settings(settings).await }
    });
    tokio::task::yield_now().await;
    assert_eq!(settings.max_active.load(Ordering::SeqCst), 1);
    settings.release_first.notify_one();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(handle.launcher_settings().catalog_directory, second_root);
    assert_eq!(handle.engine_settings().0.as_deref(), Some("Second"));
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn engine_settings_saves_are_serialized_across_async_validation() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let settings = Arc::new(BlockingSettings {
        calls: AtomicUsize::new(0),
        active: AtomicUsize::new(0),
        max_active: AtomicUsize::new(0),
        first_entered: tokio::sync::Notify::new(),
        release_first: tokio::sync::Notify::new(),
    });
    let service = Service::start_with_desktop_dependencies(
        options(temp.path(), String::new()),
        idle_engine(),
        LogStore::new(LogStoreLimits::new(10, 1024, 4)).unwrap(),
        settings.clone(),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let first = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .save_engine_settings("First".into(), "/first".into())
                .await
        }
    });
    settings.first_entered.notified().await;
    let second = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .save_engine_settings("Second".into(), "/second".into())
                .await
        }
    });
    tokio::task::yield_now().await;
    assert_eq!(settings.max_active.load(Ordering::SeqCst), 1);
    settings.release_first.notify_one();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(handle.engine_settings().0.as_deref(), Some("Second"));
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn engine_settings_save_aborts_after_validation_when_shutdown_starts() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let settings = Arc::new(BlockingSettings {
        calls: AtomicUsize::new(0),
        active: AtomicUsize::new(0),
        max_active: AtomicUsize::new(0),
        first_entered: tokio::sync::Notify::new(),
        release_first: tokio::sync::Notify::new(),
    });
    let service = Service::start_with_desktop_dependencies(
        options(temp.path(), String::new()),
        idle_engine(),
        LogStore::new(LogStoreLimits::new(10, 1024, 4)).unwrap(),
        settings.clone(),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let save = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .save_engine_settings("Late".into(), "/late".into())
                .await
        }
    });
    settings.first_entered.notified().await;
    let shutdown = tokio::spawn({
        let handle = handle.clone();
        async move { handle.shutdown().await }
    });
    tokio::task::yield_now().await;
    settings.release_first.notify_one();

    assert!(matches!(
        save.await.unwrap(),
        Err(ServiceError::ShuttingDown)
    ));
    shutdown.await.unwrap().unwrap();
    assert_eq!(handle.engine_settings(), (None, None));
    let saved = ConfigStore::new(temp.path().join("config")).load().unwrap();
    assert_eq!(saved.engine_distribution, None);
    assert_eq!(saved.engine_executable, None);
}

#[tokio::test]
async fn launcher_settings_save_aborts_after_validation_when_shutdown_starts() {
    let temp = tempfile::tempdir().unwrap();
    let initial = temp.path().join("models");
    let replacement = temp.path().join("replacement");
    std::fs::create_dir(&initial).unwrap();
    std::fs::create_dir(&replacement).unwrap();
    let settings = Arc::new(BlockingSettings {
        calls: AtomicUsize::new(0),
        active: AtomicUsize::new(0),
        max_active: AtomicUsize::new(0),
        first_entered: tokio::sync::Notify::new(),
        release_first: tokio::sync::Notify::new(),
    });
    let service = Service::start_with_desktop_dependencies(
        options(temp.path(), String::new()),
        idle_engine(),
        LogStore::new(LogStoreLimits::new(10, 1024, 4)).unwrap(),
        settings.clone(),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let save = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .save_launcher_settings(LauncherSettings {
                    catalog_directory: replacement,
                    engine_distribution: "Late".into(),
                    engine_executable: "/late".into(),
                    default_launch_settings: LaunchSettings::default(),
                })
                .await
        }
    });
    settings.first_entered.notified().await;
    let shutdown = tokio::spawn({
        let handle = handle.clone();
        async move { handle.shutdown().await }
    });
    tokio::task::yield_now().await;
    settings.release_first.notify_one();

    assert!(matches!(
        save.await.unwrap(),
        Err(ServiceError::ShuttingDown)
    ));
    shutdown.await.unwrap().unwrap();
    assert_eq!(handle.launcher_settings().catalog_directory, initial);
}

#[tokio::test]
async fn engine_settings_validate_persist_then_apply_exact_inputs() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    let service = Service::start_with_desktop_dependencies(
        options(temp.path(), "http://127.0.0.1:1".into()),
        Arc::new(Engine {
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
        }),
        LogStore::new(LogStoreLimits::new(10, 1024, 4)).unwrap(),
        Arc::new(RecordingSettings(order.clone())),
    )
    .await
    .unwrap();
    let caps = service
        .handle()
        .save_engine_settings("Ubuntu 24.04".into(), "/opt/llama server".into())
        .await
        .unwrap();
    assert!(caps.context_length);
    assert_eq!(
        *order.lock().unwrap(),
        [
            "validate:Ubuntu 24.04:/opt/llama server",
            "apply:Ubuntu 24.04:/opt/llama server"
        ]
    );
    let saved = ConfigStore::new(temp.path().join("config")).load().unwrap();
    assert_eq!(saved.engine_distribution.as_deref(), Some("Ubuntu 24.04"));
    assert_eq!(
        saved.engine_executable.as_deref(),
        Some("/opt/llama server")
    );
    service.handle().shutdown().await.unwrap();
}

#[tokio::test]
async fn profile_load_validates_persists_key_and_profile_before_lifecycle_load() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    std::fs::write(temp.path().join("models/tiny.gguf"), b"GGUFtiny").unwrap();
    let service = Service::start(
        options(temp.path(), "http://127.0.0.1:1".into()),
        Arc::new(Engine {
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let model = handle.snapshot().models[0].clone();
    let settings = LaunchSettings::default();
    handle
        .load_model_with_profile(model.id, "edited/key".into(), settings.clone())
        .await
        .unwrap();
    let persisted = ConfigStore::new(temp.path().join("config")).load().unwrap();
    let persisted = persisted
        .models
        .iter()
        .find(|candidate| candidate.id == model.id)
        .unwrap();
    assert_eq!(persisted.key.as_str(), "edited/key");
    assert_eq!(persisted.launch_profile.settings, settings);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn recent_models_are_successful_mru_entries_with_stable_ids() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    for name in ["alpha.gguf", "beta.gguf", "gamma.gguf"] {
        std::fs::write(temp.path().join("models").join(name), b"GGUFtiny").unwrap();
    }
    let service = Service::start(
        options(temp.path(), "http://127.0.0.1:1".into()),
        Arc::new(Engine {
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let models = handle.snapshot().models;
    handle.load(models[0].id).await.unwrap();
    handle.load(models[1].id).await.unwrap();
    handle.load(models[0].id).await.unwrap();

    let recent = handle.recent_models();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].id, models[0].id);
    assert_eq!(recent[1].id, models[1].id);
    std::fs::remove_file(&models[0].path).unwrap();
    handle.rescan(temp.path().join("models")).await.unwrap();
    assert_eq!(handle.recent_models()[0].id, models[1].id);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn service_change_subscription_observes_catalog_and_profile_mutations() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    std::fs::write(temp.path().join("models/tiny.gguf"), b"GGUFtiny").unwrap();
    let service = Service::start(
        options(temp.path(), "http://127.0.0.1:1".into()),
        Arc::new(Engine {
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let mut changes = handle.subscribe_changes();
    let model = handle.snapshot().models[0].clone();
    handle
        .load_model_with_profile(model.id, model.key.to_string(), LaunchSettings::default())
        .await
        .unwrap();
    changes.changed().await.unwrap();
    assert!(*changes.borrow() > 0);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn launcher_settings_persist_root_and_defaults_for_new_models() {
    let temp = tempfile::tempdir().unwrap();
    let initial = temp.path().join("models");
    let replacement = temp.path().join("replacement");
    std::fs::create_dir(&initial).unwrap();
    std::fs::create_dir(&replacement).unwrap();
    std::fs::write(replacement.join("new.gguf"), b"GGUFtiny").unwrap();
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    let service = Service::start_with_desktop_dependencies(
        options(temp.path(), "http://127.0.0.1:1".into()),
        Arc::new(Engine {
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
        }),
        LogStore::new(LogStoreLimits::new(10, 1024, 4)).unwrap(),
        Arc::new(RecordingSettings(order)),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let defaults = LaunchSettings {
        context_length: Some(model_launcher_core::ContextLength::new(6144).unwrap()),
        kv_cache_type: Some(model_launcher_core::KvCacheType::Q4_0),
        ..LaunchSettings::default()
    };
    handle
        .save_launcher_settings(LauncherSettings {
            catalog_directory: replacement.clone(),
            engine_distribution: "Ubuntu".into(),
            engine_executable: "/opt/llama-server".into(),
            default_launch_settings: defaults.clone(),
        })
        .await
        .unwrap();

    assert_eq!(handle.launcher_settings().catalog_directory, replacement);
    assert_eq!(
        handle.snapshot().models[0].launch_profile.settings,
        defaults
    );
    let saved = ConfigStore::new(temp.path().join("config")).load().unwrap();
    assert_eq!(saved.catalog_directory, Some(replacement));
    handle.shutdown().await.unwrap();

    let restarted = Service::start_with_desktop_dependencies(
        options(temp.path(), "http://127.0.0.1:1".into()),
        Arc::new(Engine {
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
        }),
        LogStore::new(LogStoreLimits::new(10, 1024, 4)).unwrap(),
        Arc::new(RecordingSettings(Arc::new(std::sync::Mutex::new(
            Vec::new(),
        )))),
    )
    .await
    .unwrap();
    assert_eq!(
        restarted
            .handle()
            .launcher_settings()
            .default_launch_settings
            .kv_cache_type,
        Some(model_launcher_core::KvCacheType::Q4_0)
    );
    assert_eq!(
        restarted.handle().snapshot().models[0]
            .launch_profile
            .settings
            .kv_cache_type,
        Some(model_launcher_core::KvCacheType::Q4_0)
    );
    restarted.handle().shutdown().await.unwrap();
}

#[tokio::test]
async fn catalog_watcher_follows_a_saved_root_switch() {
    let temp = tempfile::tempdir().unwrap();
    let initial = temp.path().join("models");
    let replacement = temp.path().join("replacement");
    std::fs::create_dir(&initial).unwrap();
    std::fs::create_dir(&replacement).unwrap();
    let mut opts = options(temp.path(), "http://127.0.0.1:1".into());
    opts.watch_catalog = true;
    let service = Service::start_with_desktop_dependencies(
        opts,
        Arc::new(Engine {
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
        }),
        LogStore::new(LogStoreLimits::new(10, 1024, 4)).unwrap(),
        Arc::new(RecordingSettings(Arc::new(std::sync::Mutex::new(
            Vec::new(),
        )))),
    )
    .await
    .unwrap();
    let handle = service.handle();
    handle
        .save_launcher_settings(LauncherSettings {
            catalog_directory: replacement.clone(),
            engine_distribution: "Ubuntu".into(),
            engine_executable: "/opt/llama-server".into(),
            default_launch_settings: LaunchSettings::default(),
        })
        .await
        .unwrap();
    let mut changes = handle.subscribe_changes();
    std::fs::write(replacement.join("watched.gguf"), b"GGUFtiny").unwrap();
    tokio::time::timeout(Duration::from_secs(3), changes.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(handle.snapshot().models.len(), 1);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn service_persists_live_tokens_and_exposes_redacted_logs() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let upstream = fake_llama_server::FakeServer::spawn().await.unwrap();
    let tokens = Arc::new(TokenStore::default());
    let mut first_options = options(temp.path(), upstream.base_url());
    first_options.gateway.authentication = Authentication::Tokens(tokens.clone());
    let engine = || {
        Arc::new(Engine {
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
        })
    };
    let service = Service::start(first_options, engine()).await.unwrap();
    let handle = service.handle();
    let plaintext = handle.generate_token().await.unwrap().plaintext;
    handle.log_store().append(LogRecord {
        timestamp_ms: 1,
        source: LogSource::Application,
        level: LogLevel::Warn,
        generation: None,
        model_id: None,
        message: format!("Authorization: Bearer {plaintext}"),
        truncated: false,
    });
    assert!(
        !handle.logs(LogFilter::default())[0]
            .message
            .contains(&plaintext)
    );
    handle.shutdown().await.unwrap();

    let restarted_tokens = Arc::new(TokenStore::default());
    let mut second_options = options(temp.path(), upstream.base_url());
    second_options.gateway.authentication = Authentication::Tokens(restarted_tokens.clone());
    let restarted = Service::start(second_options, engine()).await.unwrap();
    assert!(restarted_tokens.verify(&plaintext).await);
    restarted.handle().shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_token_persistence_failures_restore_the_live_token_set() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let tokens = Arc::new(TokenStore::default());
    let mut opts = options(temp.path(), String::new());
    opts.gateway.authentication = Authentication::Tokens(tokens.clone());
    let service = Service::start(opts, idle_engine()).await.unwrap();
    let handle = service.handle();
    std::fs::remove_dir_all(temp.path().join("config")).unwrap();
    std::fs::write(temp.path().join("config"), b"not a directory").unwrap();

    let (first, second) = tokio::join!(handle.generate_token(), handle.generate_token());
    assert!(first.is_err());
    assert!(second.is_err());
    assert!(tokens.phc_hashes().is_empty());

    std::fs::remove_file(temp.path().join("config")).unwrap();
    std::fs::create_dir(temp.path().join("config")).unwrap();
    let created = handle.generate_token().await.unwrap();
    assert!(tokens.verify(&created.plaintext).await);
    assert_eq!(tokens.phc_hashes().len(), 1);
    handle.shutdown().await.unwrap();
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
impl InferenceEngine for ProbeFailEngine {
    fn spec(&self) -> EngineFuture<'_, EngineSpec> {
        Box::pin(async { Err(model_launcher_core::AppError::EngineUnavailable) })
    }
    fn probe_capabilities(&self) -> EngineFuture<'_, EngineCapabilities> {
        Box::pin(async { Err(model_launcher_core::AppError::EngineUnavailable) })
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

#[tokio::test]
async fn initial_probe_failure_starts_disabled_and_valid_settings_recover_loading() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    std::fs::write(temp.path().join("models/tiny.gguf"), b"GGUFtiny").unwrap();
    let starts = Arc::new(AtomicUsize::new(0));
    let service = Service::start_with_desktop_dependencies(
        options(temp.path(), "http://127.0.0.1:1".into()),
        Arc::new(ProbeFailEngine {
            starts: starts.clone(),
            stops: Arc::new(AtomicUsize::new(0)),
        }),
        LogStore::new(LogStoreLimits::new(10, 1024, 4)).unwrap(),
        Arc::new(RecordingSettings(Arc::new(std::sync::Mutex::new(
            Vec::new(),
        )))),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let snapshot = handle.snapshot();
    assert!(!snapshot.engine_valid);
    assert_eq!(
        snapshot.engine_diagnostic.as_deref(),
        Some("engine is unavailable")
    );
    assert!(handle.load(snapshot.models[0].id).await.is_err());
    assert_eq!(starts.load(Ordering::SeqCst), 0);

    handle
        .save_engine_settings("Ubuntu".into(), "/opt/llama-server".into())
        .await
        .unwrap();
    assert!(handle.snapshot().engine_valid);
    handle.load(snapshot.models[0].id).await.unwrap();
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    handle.shutdown().await.unwrap();
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

struct CrashEngine(Arc<AtomicUsize>);
impl InferenceEngine for CrashEngine {
    fn spec(&self) -> EngineFuture<'_, EngineSpec> {
        Box::pin(async { unreachable!() })
    }
    fn probe_capabilities(&self) -> EngineFuture<'_, EngineCapabilities> {
        Box::pin(async { Ok(EngineCapabilities::default()) })
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
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(Box::new(CrashProcess) as Box<dyn EngineProcess>) })
    }
}
struct CrashProcess;
impl EngineProcess for CrashProcess {
    fn wait_ready(&mut self, _: Duration) -> EngineFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn check_health(&mut self) -> EngineFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn graceful_shutdown(&mut self) -> EngineFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn force_shutdown(&mut self) -> EngineFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn wait_for_exit(&mut self) -> EngineFuture<'_, i32> {
        Box::pin(async { Ok(1) })
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
        watch_catalog: false,
        shutdown_timeout: Duration::from_secs(2),
        upstream_override: (!upstream.is_empty()).then_some(upstream),
    }
}

#[tokio::test]
async fn fresh_missing_default_catalog_is_created_before_watcher_start() {
    let temp = tempfile::tempdir().unwrap();
    let mut service_options = options(temp.path(), String::new());
    service_options.watch_catalog = true;

    let service = Service::start(
        service_options,
        Arc::new(Engine {
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .await
    .expect("fresh service starts");

    assert!(temp.path().join("models").is_dir());
    service.handle().shutdown().await.unwrap();
}

#[tokio::test]
async fn file_at_default_catalog_root_starts_with_diagnostic_and_can_recover() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    std::fs::write(temp.path().join("models/preserved.gguf"), b"GGUFpreserved").unwrap();
    let seeded = Service::start(options(temp.path(), String::new()), idle_engine())
        .await
        .unwrap();
    let preserved = seeded.handle().snapshot().models[0].clone();
    seeded.handle().shutdown().await.unwrap();
    std::fs::remove_dir_all(temp.path().join("models")).unwrap();
    std::fs::write(temp.path().join("models"), b"not a directory").unwrap();
    let settings = Arc::new(RecordingSettings(Arc::new(std::sync::Mutex::new(
        Vec::new(),
    ))));
    let service = Service::start_with_desktop_dependencies(
        options(temp.path(), String::new()),
        idle_engine(),
        LogStore::new(LogStoreLimits::new(10, 1024, 4)).unwrap(),
        settings,
    )
    .await
    .unwrap();
    let handle = service.handle();
    assert!(handle.snapshot().catalog_diagnostic.is_some());
    let unavailable = handle.snapshot();
    assert_eq!(unavailable.models.len(), 1);
    assert_eq!(unavailable.models[0].id, preserved.id);
    assert_eq!(unavailable.models[0].key, preserved.key);

    let replacement = temp.path().join("recovered-models");
    std::fs::create_dir(&replacement).unwrap();
    std::fs::write(replacement.join("recovered.gguf"), b"GGUFrecovered").unwrap();
    handle
        .save_launcher_settings(LauncherSettings {
            catalog_directory: replacement.clone(),
            engine_distribution: "Ubuntu".into(),
            engine_executable: "/opt/llama-server".into(),
            default_launch_settings: LaunchSettings::default(),
        })
        .await
        .unwrap();
    assert_eq!(handle.launcher_settings().catalog_directory, replacement);
    let recovered = handle.snapshot();
    assert_eq!(recovered.models.len(), 2);
    assert!(
        recovered
            .models
            .iter()
            .any(|model| model.id == preserved.id)
    );
    assert_eq!(handle.snapshot().catalog_diagnostic, None);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn corrupt_config_starts_with_typed_visible_diagnostic() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    std::fs::write(
        temp.path().join("config/config.json"),
        b"private corrupt bytes",
    )
    .unwrap();

    let service = Service::start(
        options(temp.path(), String::new()),
        Arc::new(Engine {
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .await
    .expect("quarantined config must not prevent startup");

    let diagnostic = service
        .handle()
        .snapshot()
        .config_diagnostic
        .expect("visible diagnostic");
    assert_eq!(diagnostic.kind, ConfigDiagnosticKind::Corrupt);
    assert!(!diagnostic.to_string().contains("private corrupt bytes"));
    service.handle().shutdown().await.unwrap();
}

#[tokio::test]
async fn preloaded_invalid_config_keeps_original_diagnostic_visible_to_service_and_ui() {
    for (contents, expected_kind, expected_status) in [
        (
            b"private corrupt bytes".as_slice(),
            ConfigDiagnosticKind::Corrupt,
            "Configuration was corrupt and has been quarantined.",
        ),
        (
            br#"{"version":99,"config":{}}"#.as_slice(),
            ConfigDiagnosticKind::UnsupportedVersion { version: 99 },
            "Configuration version 99 is unsupported and has been quarantined.",
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.json"), contents).unwrap();

        // Mirror production startup: the executable needs persisted server settings before
        // constructing ServiceOptions, then hands that exact load to the service.
        let loaded = Service::load_configuration(&config_dir).expect("preload configuration");
        let persisted = loaded.config().clone();
        let mut service_options = options(temp.path(), String::new());
        service_options.gateway.bind =
            std::net::SocketAddr::new(persisted.bind_address.parse().unwrap(), 0);
        let service = Service::start_with_configuration(service_options, idle_engine(), loaded)
            .await
            .expect("quarantined config must not prevent startup");

        let diagnostic = service
            .handle()
            .snapshot()
            .config_diagnostic
            .expect("original diagnostic must survive startup");
        assert_eq!(diagnostic.kind, expected_kind);
        assert_eq!(
            model_launcher_ui::configuration_diagnostic_status(Some(&diagnostic)).as_deref(),
            Some(expected_status)
        );
        assert_eq!(
            std::fs::read_dir(&config_dir)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains("quarantine"))
                .count(),
            1,
            "the service must not quarantine the same configuration a second time"
        );
        service.handle().shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn preloaded_configuration_cannot_be_started_with_an_unrelated_store() {
    let loaded_root = tempfile::tempdir().unwrap();
    let service_root = tempfile::tempdir().unwrap();
    let loaded = Service::load_configuration(loaded_root.path().join("config")).unwrap();

    let result = Service::start_with_configuration(
        options(service_root.path(), String::new()),
        idle_engine(),
        loaded,
    )
    .await;

    assert!(matches!(
        result,
        Err(ServiceError::ConfigurationSourceMismatch)
    ));
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
    let mut shutdown_events = handle.subscribe_shutdown();
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
        .header("x-fake-mode", "sse-multi")
        .json(&serde_json::json!({"model":key,"prompt":"hello","stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    use futures_util::StreamExt as _;
    let mut frames = response.bytes_stream();
    let mut chunks = Vec::new();
    while let Some(frame) = frames.next().await {
        chunks.push(frame.unwrap());
    }
    let joined = chunks.concat();
    assert_eq!(joined, b"data: a\n\ndata: \xff\0\n\n");
    assert_eq!(
        joined.windows(2).filter(|value| *value == b"\n\n").count(),
        2
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
    let mut observed = Vec::new();
    while let Ok(event) = shutdown_events.try_recv() {
        observed.push(event);
    }
    assert_eq!(
        observed,
        vec![
            ShutdownEvent::Started(ShutdownPhase::StopGateway),
            ShutdownEvent::Completed(ShutdownPhase::StopGateway),
            ShutdownEvent::Started(ShutdownPhase::StopLifecycle),
            ShutdownEvent::Completed(ShutdownPhase::StopLifecycle),
            ShutdownEvent::Started(ShutdownPhase::PersistConfig),
            ShutdownEvent::Completed(ShutdownPhase::PersistConfig),
            ShutdownEvent::Started(ShutdownPhase::StopWatcher),
            ShutdownEvent::Completed(ShutdownPhase::StopWatcher),
            ShutdownEvent::Started(ShutdownPhase::JoinLifecycle),
            ShutdownEvent::Completed(ShutdownPhase::JoinLifecycle),
        ]
    );
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

#[tokio::test]
async fn shutdown_cancels_backoff_and_prevents_restart() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    std::fs::write(temp.path().join("models/crash.gguf"), b"GGUFtiny").unwrap();
    let upstream = fake_llama_server::FakeServer::spawn().await.unwrap();
    let starts = Arc::new(AtomicUsize::new(0));
    let service = Service::start(
        options(temp.path(), upstream.base_url()),
        Arc::new(CrashEngine(starts.clone())),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let base = format!("http://{}", handle.local_addr());
    reqwest::Client::new()
        .post(format!("{base}/api/v1/models/load"))
        .json(&serde_json::json!({"model":"crash"}))
        .send()
        .await
        .unwrap();
    let mut lifecycle = handle.subscribe_lifecycle();
    tokio::time::timeout(Duration::from_secs(1), async {
        while lifecycle.borrow().state != LifecycleState::Backoff {
            lifecycle.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(handle.snapshot().lifecycle.state, LifecycleState::Backoff);
    handle.shutdown().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert_eq!(handle.snapshot().lifecycle.desired_model, None);
    upstream.stop().await.unwrap();
}

#[tokio::test]
async fn cancelling_first_shutdown_caller_does_not_cancel_owned_shutdown() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let upstream = fake_llama_server::FakeServer::spawn().await.unwrap();
    let mut opts = options(temp.path(), upstream.base_url());
    opts.shutdown_timeout = Duration::from_millis(20);
    let pending = tokio::spawn(std::future::pending());
    let service = Service::start_with_background_task(
        opts,
        Arc::new(Engine {
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
        }),
        Some(pending),
    )
    .await
    .unwrap();
    let handle = service.handle();
    let mut events = handle.subscribe_shutdown();
    let first = tokio::spawn({
        let handle = handle.clone();
        async move { handle.shutdown().await }
    });
    loop {
        if events.recv().await.unwrap() == ShutdownEvent::Started(ShutdownPhase::StopWatcher) {
            break;
        }
    }
    first.abort();
    let _ = first.await;
    assert!(matches!(
        handle.rescan(temp.path().join("models")).await,
        Err(ServiceError::ShuttingDown)
    ));
    let one = handle.shutdown().await.unwrap_err();
    let two = handle.shutdown().await.unwrap_err();
    assert_eq!(one.to_string(), two.to_string());
    assert!(
        !one.to_string()
            .contains("service shutdown: service shutdown:")
    );
    upstream.stop().await.unwrap();
}

#[tokio::test]
async fn dropped_and_slow_shutdown_subscribers_never_block_shutdown() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let upstream = fake_llama_server::FakeServer::spawn().await.unwrap();
    let service = Service::start(
        options(temp.path(), upstream.base_url()),
        Arc::new(Engine {
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .await
    .unwrap();
    let dropped = service.handle().subscribe_shutdown();
    drop(dropped);
    let _slow = service.handle().subscribe_shutdown();
    tokio::time::timeout(Duration::from_secs(1), service.handle().shutdown())
        .await
        .unwrap()
        .unwrap();
    upstream.stop().await.unwrap();
}

#[tokio::test]
async fn watcher_join_timeout_is_typed_and_later_phases_are_attempted() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let upstream = fake_llama_server::FakeServer::spawn().await.unwrap();
    let mut opts = options(temp.path(), upstream.base_url());
    opts.shutdown_timeout = Duration::from_millis(10);
    let pending = tokio::spawn(std::future::pending());
    let service = Service::start_with_background_task(
        opts,
        Arc::new(Engine {
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
        }),
        Some(pending),
    )
    .await
    .unwrap();
    let mut shutdown_events = service.handle().subscribe_shutdown();
    let error = service.handle().shutdown().await.unwrap_err().to_string();
    assert!(error.contains("catalog watcher join timed out"));
    let mut observed = Vec::new();
    while let Ok(event) = shutdown_events.try_recv() {
        observed.push(event);
    }
    assert!(observed.contains(&ShutdownEvent::Completed(ShutdownPhase::JoinLifecycle)));
    upstream.stop().await.unwrap();
}

#[tokio::test]
async fn listener_bind_failure_cleans_partially_started_lifecycle() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = occupied.local_addr().unwrap();
    let mut opts = options(temp.path(), "http://127.0.0.1:1".into());
    opts.gateway.bind = address;
    let starts = Arc::new(AtomicUsize::new(0));
    let result = Service::start(
        opts,
        Arc::new(Engine {
            starts: starts.clone(),
            stops: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    drop(occupied);
    assert!(tokio::net::TcpListener::bind(address).await.is_ok());
}

fn idle_engine() -> Arc<dyn InferenceEngine> {
    Arc::new(Engine {
        starts: Arc::new(AtomicUsize::new(0)),
        stops: Arc::new(AtomicUsize::new(0)),
    })
}

#[tokio::test]
async fn watcher_batch_waiting_before_gate_is_discarded_after_shutdown_starts() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let mut opts = options(temp.path(), "http://127.0.0.1:1".into());
    opts.watch_catalog = true;
    opts.shutdown_timeout = Duration::from_secs(2);
    let barrier = WatcherBarrier::new(WatcherBarrierPoint::BeforeGate);
    let service = Service::start_with_watcher_barrier(opts, idle_engine(), barrier.clone())
        .await
        .unwrap();
    std::fs::write(temp.path().join("models/late.gguf"), b"GGUFtiny").unwrap();
    tokio::time::timeout(Duration::from_secs(3), barrier.entered())
        .await
        .unwrap();
    let handle = service.handle();
    let mut events = handle.subscribe_shutdown();
    let shutdown = tokio::spawn({
        let handle = handle.clone();
        async move { handle.shutdown().await }
    });
    while events.recv().await.unwrap() != ShutdownEvent::Started(ShutdownPhase::StopGateway) {}
    barrier.release();
    shutdown.await.unwrap().unwrap();
    assert!(handle.snapshot().models.is_empty());
    assert!(
        ConfigStore::new(temp.path().join("config"))
            .load()
            .unwrap()
            .models
            .is_empty()
    );
}

#[tokio::test]
async fn shutdown_waits_for_watcher_already_inside_mutation_gate_and_persists_latest() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("models")).unwrap();
    let mut opts = options(temp.path(), "http://127.0.0.1:1".into());
    opts.watch_catalog = true;
    opts.shutdown_timeout = Duration::from_secs(2);
    let barrier = WatcherBarrier::new(WatcherBarrierPoint::InsideGate);
    let service = Service::start_with_watcher_barrier(opts, idle_engine(), barrier.clone())
        .await
        .unwrap();
    std::fs::write(temp.path().join("models/latest.gguf"), b"GGUFtiny").unwrap();
    tokio::time::timeout(Duration::from_secs(3), barrier.entered())
        .await
        .unwrap();
    let handle = service.handle();
    let shutdown = tokio::spawn({
        let handle = handle.clone();
        async move { handle.shutdown().await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!shutdown.is_finished());
    barrier.release();
    shutdown.await.unwrap().unwrap();
    let memory = handle.snapshot().models;
    let disk = ConfigStore::new(temp.path().join("config"))
        .load()
        .unwrap()
        .models;
    assert_eq!(memory, disk);
    assert_eq!(disk.len(), 1);
}
