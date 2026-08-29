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
    body::Body,
    extract::{DefaultBodyLimit, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use model_launcher_core::{
    AppError, BatchSize, ContextLength, EngineCapabilities, LifecycleHandle, ModelRecord,
};
use serde::Serialize;
use tokio::{
    net::TcpListener,
    sync::{Semaphore, oneshot},
    task::JoinSet,
};
use tower::ServiceExt as _;

pub type UpstreamResolver = Arc<dyn Fn(&ModelRecord) -> Option<String> + Send + Sync>;

#[derive(Clone)]
pub struct ManagementModel {
    pub model: ModelRecord,
    pub capabilities: EngineCapabilities,
}
pub trait ManagementModelResolver: Send + Sync {
    fn resolve(&self, key: &str) -> Option<ManagementModel>;
}
pub trait ProfileUpdater: Send + Sync {
    fn apply(&self, model: ManagementModel, request: &LoadRequest)
    -> Result<ModelRecord, AppError>;
}

struct StaticManagementResolver {
    models: Arc<[ApiModel]>,
}
impl ManagementModelResolver for StaticManagementResolver {
    fn resolve(&self, key: &str) -> Option<ManagementModel> {
        self.models
            .iter()
            .find(|value| value.record.key.as_str() == key)
            .map(|value| ManagementModel {
                model: value.record.clone(),
                capabilities: value.capabilities.clone(),
            })
    }
}
struct CapabilityProfileUpdater;
impl ProfileUpdater for CapabilityProfileUpdater {
    fn apply(
        &self,
        resolved: ManagementModel,
        request: &LoadRequest,
    ) -> Result<ModelRecord, AppError> {
        let mut model = resolved.model;
        if request.context_length.is_some() && !resolved.capabilities.context_length {
            return Err(AppError::InvalidSetting("context_length"));
        }
        if request.eval_batch_size.is_some() && !resolved.capabilities.batch_size {
            return Err(AppError::InvalidSetting("eval_batch_size"));
        }
        if request.flash_attention.is_some() && !resolved.capabilities.flash_attention {
            return Err(AppError::InvalidSetting("flash_attention"));
        }
        if request.num_experts.is_some() {
            return Err(AppError::InvalidSetting("num_experts"));
        }
        if request.offload_kv_cache_to_gpu.is_some() {
            return Err(AppError::InvalidSetting("offload_kv_cache_to_gpu"));
        }
        model.launch_profile.settings.context_length =
            request.context_length.map(ContextLength::new).transpose()?;
        model.launch_profile.settings.batch_size =
            request.eval_batch_size.map(BatchSize::new).transpose()?;
        model.launch_profile.settings.flash_attention = request.flash_attention;
        Ok(model)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct GatewayLimits {
    pub max_body_bytes: usize,
    pub max_headers: usize,
    pub max_header_bytes: usize,
    pub max_connections: usize,
    pub max_in_flight_requests: usize,
    pub startup_timeout: Duration,
    pub shutdown_grace: Duration,
    pub upstream_connect_timeout: Duration,
    pub upstream_header_timeout: Duration,
}

impl Default for GatewayLimits {
    fn default() -> Self {
        Self {
            max_body_bytes: 8 * 1024 * 1024,
            max_headers: 64,
            max_header_bytes: 32 * 1024,
            max_connections: 128,
            max_in_flight_requests: 128,
            startup_timeout: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(5),
            upstream_connect_timeout: Duration::from_secs(5),
            upstream_header_timeout: Duration::from_secs(30),
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
    pub limits: GatewayLimits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GatewayWarning {
    UnauthenticatedNonLoopback,
}

impl GatewayConfig {
    pub fn validate(&self) -> Vec<GatewayWarning> {
        let lan = !self.bind.ip().is_loopback();
        if lan && matches!(self.authentication, Authentication::Disabled) {
            vec![GatewayWarning::UnauthenticatedNonLoopback]
        } else {
            Vec::new()
        }
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
    management: Arc<dyn ManagementModelResolver>,
    profiles: Arc<dyn ProfileUpdater>,
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
        let models: Arc<[ApiModel]> = models.into();
        let management = Arc::new(StaticManagementResolver {
            models: models.clone(),
        });
        Self::new_with_management(
            config,
            models,
            lifecycle,
            upstream,
            management,
            Arc::new(CapabilityProfileUpdater),
        )
    }

    pub fn new_with_management(
        config: GatewayConfig,
        models: Arc<[ApiModel]>,
        lifecycle: LifecycleHandle,
        upstream: UpstreamResolver,
        management: Arc<dyn ManagementModelResolver>,
        profiles: Arc<dyn ProfileUpdater>,
    ) -> Result<Self, GatewayConfigError> {
        let _warnings = config.validate();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(config.limits.upstream_connect_timeout)
            .build()
            .expect("reqwest client configuration is valid");
        Ok(Self {
            state: AppState {
                management,
                profiles,
                models,
                lifecycle,
                upstream,
                client,
                limits: config.limits,
                connections: Arc::new(Semaphore::new(config.limits.max_in_flight_requests)),
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
        self.start_with_acceptor(address, Arc::new(TcpAcceptor(listener)))
            .await
    }

    pub async fn start_with_acceptor(
        self,
        address: SocketAddr,
        acceptor: Arc<dyn Accept>,
    ) -> std::io::Result<GatewayServer> {
        let (shutdown, stop) = oneshot::channel();
        let grace = self.config.limits.shutdown_grace;
        let permits = Arc::new(Semaphore::new(self.config.limits.max_connections));
        let router = self.router();
        let task = tokio::spawn(run_server(acceptor, router, permits, stop));
        Ok(GatewayServer {
            address,
            shutdown: Some(shutdown),
            task: Some(task),
            grace,
        })
    }
}

pub struct GatewayServer {
    address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    grace: Duration,
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
        let mut task = self.task.take().expect("server task exists");
        match tokio::time::timeout(self.grace, &mut task).await {
            Ok(joined) => joined.map_err(std::io::Error::other)?,
            Err(_) => {
                task.abort();
                let _ = task.await;
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "gateway shutdown grace expired",
                ))
            }
        }
    }
}
impl Drop for GatewayServer {
    fn drop(&mut self) {
        if let Some(stop) = self.shutdown.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn authenticate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let allowed = match &state.authentication {
        Authentication::Disabled => true,
        Authentication::Tokens(tokens) => {
            match bearer_token(request.headers().get(header::AUTHORIZATION)) {
                Some(token) => tokens.verify(token).await,
                None => false,
            }
        }
    };
    if allowed {
        next.run(request).await
    } else {
        ApiError::unauthorized().into_response()
    }
}

fn bearer_token(value: Option<&axum::http::HeaderValue>) -> Option<&str> {
    let value = value?.to_str().ok()?;
    let mut parts = value.split_ascii_whitespace();
    let scheme = parts.next()?;
    let token = parts.next()?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(token)
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
        ApiError::headers(
            "header_limits_exceeded",
            "request headers exceed configured limits",
        )
        .into_response()
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

async fn run_server(
    acceptor: Arc<dyn Accept>,
    router: Router,
    permits: Arc<Semaphore>,
    mut stop: oneshot::Receiver<()>,
) -> std::io::Result<()> {
    let mut connections = JoinSet::new();
    let (connection_stop, _) = tokio::sync::watch::channel(false);
    loop {
        let accepted = tokio::select! {
            _ = &mut stop => break,
            value = accept_one(acceptor.as_ref(), permits.clone()) => value,
        };
        let (stream, permit) = match accepted {
            Ok(value) => value,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::ConnectionAborted
                ) =>
            {
                tokio::task::yield_now().await;
                continue;
            }
            Err(error) => return Err(error),
        };
        let app = router.clone();
        let mut connection_stop = connection_stop.subscribe();
        connections.spawn(async move {
            let service = hyper::service::service_fn(
                move |request: hyper::Request<hyper::body::Incoming>| {
                    let app = app.clone();
                    async move { app.oneshot(request.map(Body::new)).await }
                },
            );
            let connection = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service);
            tokio::pin!(connection);
            tokio::select! {
                _ = &mut connection => {},
                _ = connection_stop.changed() => { connection.as_mut().graceful_shutdown(); let _ = connection.await; }
            }
            drop(permit);
        });
    }
    let _ = connection_stop.send(true);
    while connections.join_next().await.is_some() {}
    Ok(())
}

async fn accept_one(
    acceptor: &dyn Accept,
    permits: Arc<Semaphore>,
) -> std::io::Result<(tokio::net::TcpStream, tokio::sync::OwnedSemaphorePermit)> {
    let permit = permits
        .acquire_owned()
        .await
        .map_err(|_| std::io::Error::other("gateway connection admission closed"))?;
    let (stream, _) = acceptor.accept().await?;
    Ok((stream, permit))
}

#[async_trait::async_trait]
pub trait Accept: Send + Sync {
    async fn accept(&self) -> std::io::Result<(tokio::net::TcpStream, SocketAddr)>;
}

struct TcpAcceptor(TcpListener);
#[async_trait::async_trait]
impl Accept for TcpAcceptor {
    async fn accept(&self) -> std::io::Result<(tokio::net::TcpStream, SocketAddr)> {
        self.0.accept().await
    }
}
