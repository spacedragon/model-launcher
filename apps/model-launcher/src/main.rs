use model_launcher::{EngineSettingsManager, Service, ServiceOptions};
use model_launcher_api::{Authentication, GatewayConfig, GatewayLimits, TokenStore};
use model_launcher_core::{LogFilter, LogStore, LogStoreLimits};
use model_launcher_ui::{AppSnapshot, CloseNoticeStore, UiActions, run_desktop};
use model_launcher_wsl::{LlamaCppWslEngine, ProbeCache, TokioCommandRunner, WslProber};
use std::{path::PathBuf, sync::Arc, time::Duration};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let base = platform_data_dir();
    let token_store = Arc::new(TokenStore::default());
    let options = ServiceOptions {
        config_dir: base.join("config"),
        catalog_dir: base.join("models"),
        gateway: GatewayConfig {
            bind: "127.0.0.1:1234".parse()?,
            authentication: Authentication::Tokens(token_store),
            limits: GatewayLimits::default(),
        },
        upstream: std::env::var("MODEL_LAUNCHER_UPSTREAM")
            .unwrap_or_else(|_| "http://127.0.0.1:8080".into()),
        watch_catalog: true,
        shutdown_timeout: Duration::from_secs(10),
    };
    let logs = LogStore::new(LogStoreLimits::new(2_000, 2 * 1024 * 1024, 256))?;
    let runner = Arc::new(TokioCommandRunner::with_log_store(logs.clone(), None, None));
    let engine = Arc::new(LlamaCppWslEngine::new(
        std::env::var("MODEL_LAUNCHER_WSL_DISTRO").unwrap_or_else(|_| "Ubuntu".into()),
        std::env::var("MODEL_LAUNCHER_LLAMA_SERVER")
            .unwrap_or_else(|_| "/usr/local/bin/llama-server".into()),
        runner.clone(),
    ));
    let settings_manager = Arc::new(ProductionEngineSettings {
        engine: engine.clone(),
        probe: ProbeCache::new(
            base.join("config/engine-probe.json"),
            WslProber::new(runner),
        ),
    });
    let service = runtime.block_on(Service::start_with_desktop_dependencies(
        options.clone(),
        engine,
        logs,
        settings_manager,
    ))?;
    let handle = service.handle();
    let snapshot = handle.snapshot();
    let runtime_handle = runtime.handle().clone();
    let close_notice = Arc::new(std::sync::Mutex::new((
        CloseNoticeStore::new(base.join("close-to-tray-notice")),
        None,
    )));
    let actions = UiActions {
        load: Arc::new({
            let handle = handle.clone();
            let runtime_handle = runtime_handle.clone();
            move |request: model_launcher_ui::UiLoadRequest| {
                let handle = handle.clone();
                runtime_handle.spawn(async move {
                    let _ = handle
                        .load_model_with_profile(request.id, request.key, request.settings)
                        .await;
                });
            }
        }),
        eject: Arc::new({
            let handle = handle.clone();
            let runtime_handle = runtime_handle.clone();
            move || {
                let handle = handle.clone();
                runtime_handle.spawn(async move {
                    let _ = handle.eject().await;
                });
            }
        }),
        rescan: Arc::new({
            let handle = handle.clone();
            let runtime_handle = runtime_handle.clone();
            let catalog = options.catalog_dir.clone();
            move || {
                let handle = handle.clone();
                let catalog = catalog.clone();
                runtime_handle.spawn(async move {
                    let _ = handle.rescan(catalog).await;
                });
            }
        }),
        snapshot: Arc::new({
            let handle = handle.clone();
            move || {
                let snapshot = handle.snapshot();
                AppSnapshot {
                    models: snapshot.models,
                    recent_models: handle.recent_models(),
                    lifecycle: snapshot.lifecycle,
                    capabilities: handle.capabilities(),
                }
            }
        }),
        quit: Arc::new({
            let handle = handle.clone();
            let runtime_handle = runtime_handle.clone();
            move || {
                let handle = handle.clone();
                runtime_handle.spawn(async move {
                    let _ = handle.shutdown().await;
                    let _ = slint::quit_event_loop();
                });
            }
        }),
        close_notice: Arc::new({
            let close_notice = close_notice.clone();
            move || {
                let mut state = close_notice.lock().expect("close notice lock poisoned");
                let mut notice = state.1.take().unwrap_or_else(|| state.0.load());
                let message = notice.take().map(str::to_owned);
                let _ = state.0.save(&notice);
                state.1 = Some(notice);
                message
            }
        }),
        save_settings: Arc::new({
            let runtime_handle = runtime_handle.clone();
            let handle = handle.clone();
            move |settings| {
                let handle = handle.clone();
                runtime_handle.spawn(async move {
                    let _ = handle
                        .save_engine_settings(settings.distribution, settings.executable)
                        .await;
                });
            }
        }),
        logs: Arc::new({
            let handle = handle.clone();
            move |filter: LogFilter| handle.logs(filter)
        }),
        export_logs: Arc::new({
            let handle = handle.clone();
            let path = base.join("model-launcher-logs.jsonl");
            move || {
                let _ = handle.export_logs(&path);
            }
        }),
        generate_token: Arc::new({
            let handle = handle.clone();
            move || handle.generate_token().ok().map(|token| token.plaintext)
        }),
        engine_settings: Arc::new({
            let handle = handle.clone();
            let directory = options.catalog_dir.display().to_string();
            move || {
                let (distribution, executable) = handle.engine_settings();
                model_launcher_ui::EngineSettings {
                    model_directory: directory.clone(),
                    distribution: distribution.unwrap_or_else(|| "Ubuntu".into()),
                    executable: executable.unwrap_or_else(|| "/usr/local/bin/llama-server".into()),
                }
            }
        }),
    };
    runtime_handle.spawn({
        let handle = handle.clone();
        async move {
            let mut lifecycle = handle.subscribe_lifecycle();
            let mut changes = handle.subscribe_changes();
            let mut logs = handle.log_store().subscribe();
            let mut log_poll = tokio::time::interval(Duration::from_millis(100));
            loop {
                let refresh = tokio::select! {
                    changed = lifecycle.changed() => changed.is_ok(),
                    changed = changes.changed() => changed.is_ok(),
                    _ = log_poll.tick() => logs.try_recv().is_ok(),
                };
                if !refresh {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(35)).await;
                model_launcher_ui::request_refresh();
            }
        }
    });
    run_desktop(
        AppSnapshot {
            models: snapshot.models,
            recent_models: handle.recent_models(),
            lifecycle: snapshot.lifecycle,
            capabilities: handle.capabilities(),
        },
        handle.local_addr().to_string(),
        actions,
    )?;
    runtime.block_on(handle.shutdown())?;
    Ok(())
}

struct ProductionEngineSettings {
    engine: Arc<LlamaCppWslEngine>,
    probe: ProbeCache,
}

#[async_trait::async_trait]
impl EngineSettingsManager for ProductionEngineSettings {
    async fn validate(
        &self,
        distribution: &str,
        executable: &str,
    ) -> Result<model_launcher_core::EngineCapabilities, String> {
        self.probe
            .refresh(distribution, executable)
            .await
            .map(|snapshot| snapshot.capabilities)
            .map_err(|error| error.to_string())
    }

    fn apply(&self, distribution: String, executable: String) {
        self.engine.apply_settings(distribution, executable);
    }
}

fn platform_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("ModelLauncher")
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("model-launcher"))
    }
}
