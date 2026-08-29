use model_launcher::{Service, ServiceOptions};
use model_launcher_api::{Authentication, GatewayConfig, GatewayLimits};
use model_launcher_core::InferenceEngine as _;
use model_launcher_ui::{AppSnapshot, CloseNoticeStore, UiActions, run_desktop};
use model_launcher_wsl::{LlamaCppWslEngine, TokioCommandRunner};
use std::{path::PathBuf, sync::Arc, time::Duration};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let base = platform_data_dir();
    let options = ServiceOptions {
        config_dir: base.join("config"),
        catalog_dir: base.join("models"),
        gateway: GatewayConfig {
            bind: "127.0.0.1:1234".parse()?,
            authentication: Authentication::Disabled,
            limits: GatewayLimits::default(),
        },
        upstream: std::env::var("MODEL_LAUNCHER_UPSTREAM")
            .unwrap_or_else(|_| "http://127.0.0.1:8080".into()),
        watch_catalog: true,
        shutdown_timeout: Duration::from_secs(10),
    };
    let engine = Arc::new(LlamaCppWslEngine::new(
        std::env::var("MODEL_LAUNCHER_WSL_DISTRO").unwrap_or_else(|_| "Ubuntu".into()),
        std::env::var("MODEL_LAUNCHER_LLAMA_SERVER")
            .unwrap_or_else(|_| "/usr/local/bin/llama-server".into()),
        Arc::new(TokioCommandRunner::default()),
    ));
    let reprobe_engine = engine.clone();
    let service = runtime.block_on(Service::start(options.clone(), engine))?;
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
            move |id| {
                let handle = handle.clone();
                runtime_handle.spawn(async move {
                    let _ = handle.load(id).await;
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
            move |_| {
                let engine = reprobe_engine.clone();
                runtime_handle.spawn(async move {
                    let _ = engine.probe_capabilities().await;
                });
            }
        }),
    };
    run_desktop(
        AppSnapshot {
            models: snapshot.models,
            lifecycle: snapshot.lifecycle,
            capabilities: handle.capabilities(),
        },
        handle.local_addr().to_string(),
        actions,
    )?;
    runtime.block_on(handle.shutdown())?;
    Ok(())
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
