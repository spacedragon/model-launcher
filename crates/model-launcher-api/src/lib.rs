mod auth;
mod management;
mod models;
mod proxy;

pub use auth::*;
pub use models::*;

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use model_launcher_core::{AppError, LifecycleHandle, ModelRecord};
use serde::Serialize;
use tokio::{
    net::TcpListener,
    sync::{Semaphore, oneshot},
};

pub type UpstreamResolver = Arc<dyn Fn(&ModelRecord) -> Option<String> + Send + Sync>;

#[derive(Clone, Copy, Debug)]
pub struct GatewayLimits {
    pub max_body_bytes: usize,
    pub max_headers: usize,
    pub max_header_bytes: usize,
    pub max_connections: usize,
    pub startup_timeout: Duration,
}

impl Default for GatewayLimits {
    fn default() -> Self {
        Self {
            max_body_bytes: 8 * 1024 * 1024,
            max_headers: 64,
            max_header_bytes: 32 * 1024,
            max_connections: 128,
            startup_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Clone)]
pub enum Authentication {
    Disabled,
    Tokens(Arc<TokenStore>),
}

#[derive(Clone)]
pub struct GatewayConfig {
    pub bind: SocketAddr,
    pub authentication: Authentication,
    pub allow_insecure_lan: bool,
    pub limits: GatewayLimits,
}

impl GatewayConfig {
    pub fn validate(&self) -> Result<Vec<&'static str>, GatewayConfigError> {
        let lan = !self.bind.ip().is_loopback();
        if lan
            && matches!(self.authentication, Authentication::Disabled)
            && !self.allow_insecure_lan
        {
            return Err(GatewayConfigError::UnauthenticatedLan);
        }
        Ok(
            if lan && matches!(self.authentication, Authentication::Disabled) {
                vec!["API is exposed to the LAN without authentication"]
            } else {
                Vec::new()
            },
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GatewayConfigError {
    #[error("non-loopback API binding without authentication requires explicit allow_insecure_lan")]
    UnauthenticatedLan,
}

#[derive(Clone)]
pub(crate) struct AppState {
    models: Arc<[ApiModel]>,
    lifecycle: LifecycleHandle,
    upstream: UpstreamResolver,
    client: reqwest::Client,
    limits: GatewayLimits,
    connections: Arc<Semaphore>,
    authentication: Authentication,
}

impl AppState {
    fn find_model(&self, key: &str) -> Result<&ApiModel, ApiError> {
        self.models
            .iter()
            .find(|model| model.record.key.as_str() == key)
            .ok_or_else(|| ApiError::not_found("model_not_found", "model was not found"))
    }
}

pub struct Gateway {
    config: GatewayConfig,
    state: AppState,
}

impl Gateway {
    pub fn new(
        config: GatewayConfig,
        models: Vec<ApiModel>,
        lifecycle: LifecycleHandle,
        upstream: UpstreamResolver,
    ) -> Result<Self, GatewayConfigError> {
        config.validate()?;
        let client = reqwest::Client::builder()
            .build()
            .expect("reqwest client configuration is valid");
        Ok(Self {
            state: AppState {
                models: models.into(),
                lifecycle,
                upstream,
                client,
                limits: config.limits,
                connections: Arc::new(Semaphore::new(config.limits.max_connections)),
                authentication: config.authentication.clone(),
            },
            config,
        })
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/api/v1/models", get(management::list))
            .route("/api/v1/models/load", post(management::load))
            .route("/api/v1/models/unload", post(management::unload))
            .route("/v1/models", get(management::openai_models))
            .route("/v1/chat/completions", post(proxy::proxy))
            .route("/v1/completions", post(proxy::proxy))
            .route_layer(middleware::from_fn_with_state(
                self.state.clone(),
                authenticate,
            ))
            .route_layer(middleware::from_fn_with_state(
                self.state.clone(),
                limit_headers,
            ))
            .layer(DefaultBodyLimit::max(self.state.limits.max_body_bytes))
            .with_state(self.state.clone())
    }

    pub async fn start(self) -> std::io::Result<GatewayServer> {
        let listener = TcpListener::bind(self.config.bind).await?;
        let address = listener.local_addr()?;
        let (shutdown, stop) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, self.router())
                .with_graceful_shutdown(async {
                    let _ = stop.await;
                })
                .await
        });
        Ok(GatewayServer {
            address,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }
}

pub struct GatewayServer {
    address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}
impl GatewayServer {
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.address
    }
    pub async fn stop(mut self) -> std::io::Result<()> {
        if let Some(stop) = self.shutdown.take() {
            let _ = stop.send(());
        }
        self.task
            .take()
            .expect("server task exists")
            .await
            .map_err(std::io::Error::other)?
    }
}
impl Drop for GatewayServer {
    fn drop(&mut self) {
        if let Some(stop) = self.shutdown.take() {
            let _ = stop.send(());
        }
    }
}

async fn authenticate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let allowed = match &state.authentication {
        Authentication::Disabled => true,
        Authentication::Tokens(tokens) => request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|token| tokens.verify(token)),
    };
    if allowed {
        next.run(request).await
    } else {
        ApiError::unauthorized().into_response()
    }
}

async fn limit_headers(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let header_bytes = request
        .headers()
        .iter()
        .map(|(name, value)| name.as_str().len().saturating_add(value.as_bytes().len()))
        .try_fold(0_usize, usize::checked_add);
    if request.headers().len() > state.limits.max_headers
        || header_bytes.is_none_or(|bytes| bytes > state.limits.max_header_bytes)
    {
        ApiError::headers("too_many_headers", "too many request headers").into_response()
    } else {
        next.run(request).await
    }
}

#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    retry_after: bool,
}
impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
            retry_after: false,
        }
    }
    pub(crate) fn bad_request(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }
    fn payload(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, code, message)
    }
    fn headers(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE, code, message)
    }
    fn not_found(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::NOT_FOUND, code, message)
    }
    fn unavailable(code: &'static str, message: &'static str) -> Self {
        let mut error = Self::new(StatusCode::SERVICE_UNAVAILABLE, code, message);
        error.retry_after = true;
        error
    }
    fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "authentication failed",
        )
    }
    fn core(error: AppError) -> Self {
        match error {
            AppError::ModelNotFound => Self::not_found("model_not_found", "model was not found"),
            AppError::ModelBusy => {
                Self::new(StatusCode::CONFLICT, "model_busy", "another model is busy")
            }
            AppError::ModelStarting => Self::unavailable("model_starting", "model is starting"),
            AppError::InvalidSetting(_) | AppError::InvalidModelKey => {
                Self::bad_request(error.code(), "invalid model configuration")
            }
            _ => Self::unavailable(error.code(), "model engine is unavailable"),
        }
    }
}

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}
#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: &'static str,
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(ErrorEnvelope {
                error: ErrorBody {
                    code: self.code,
                    message: self.message,
                },
            }),
        )
            .into_response();
        if self.retry_after {
            response.headers_mut().insert(
                header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("1"),
            );
        }
        response
    }
}

#[must_use]
pub fn is_loopback(address: IpAddr) -> bool {
    address.is_loopback()
}
