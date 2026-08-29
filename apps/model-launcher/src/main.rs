use model_launcher::{Service, ServiceOptions};
use model_launcher_api::{Authentication, GatewayConfig, GatewayLimits};
use model_launcher_ui::{AppSnapshot, UiActions, run_desktop};
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
    let service = runtime.block_on(Service::start(options.clone(), engine))?;
    let handle = service.handle();
    let snapshot = handle.snapshot();
    let runtime_handle = runtime.handle().clone();
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
