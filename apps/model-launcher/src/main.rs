use model_launcher::{Service, ServiceOptions};
use model_launcher_api::{Authentication, GatewayConfig, GatewayLimits};
use model_launcher_wsl::{LlamaCppWslEngine, TokioCommandRunner};
use std::{path::PathBuf, sync::Arc, time::Duration};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    let service = Service::start(options, engine).await?;
    tokio::signal::ctrl_c().await?;
    service.handle().shutdown().await?;
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
