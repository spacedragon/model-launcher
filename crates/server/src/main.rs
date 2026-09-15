//! `model-serving` daemon entry point.
//!
//! Tooling-skeleton stage (Job 1): the daemon boots a minimal axum app
//! with a `/healthz` liveness route. Route assembly, configuration and
//! static UI hosting arrive in later jobs (see `docs/development-plan.md`).

use std::net::SocketAddr;

use axum::{Router, routing::get};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

/// Default management/gateway listener port. Must match the Vite dev
/// proxy target in `frontend/vite.config.ts`; override with
/// `MODELSERVING_HTTP_PORT`. Full bind configuration (explicit
/// non-loopback opt-in, per `docs/product-requirements.md` §3.6) lands
/// with configuration support in a later job.
const DEFAULT_HTTP_PORT: u16 = 8137;

fn router() -> Router {
    Router::new().route("/healthz", get(healthz))
}

async fn healthz() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "status": "ok" }))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    tracing::info!("model-serving daemon starting (tooling skeleton)");

    let port: u16 = match std::env::var("MODELSERVING_HTTP_PORT") {
        Err(_) => DEFAULT_HTTP_PORT,
        Ok(raw) => match raw.parse() {
            Ok(parsed) => parsed,
            Err(err) => {
                eprintln!("invalid MODELSERVING_HTTP_PORT {raw:?}: {err}");
                std::process::exit(1);
            }
        },
    };
    let listener = match TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("failed to bind 127.0.0.1:{port}: {err}");
            std::process::exit(1);
        }
    };
    let addr = listener
        .local_addr()
        .expect("listener has no local address");
    tracing::info!(%addr, "listening");

    if let Err(err) = axum::serve(listener, router()).await {
        eprintln!("server error: {err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::healthz;

    #[tokio::test]
    async fn healthz_reports_ok() {
        let body = healthz().await.0;
        assert_eq!(body["status"], "ok");
    }
}
