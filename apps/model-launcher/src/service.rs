use std::{
    collections::VecDeque,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::FutureExt as _;
use model_launcher_api::{
    ApiModel, Authentication, CreatedToken, Gateway, GatewayConfig, GatewayConfigError,
    GatewayServer, LifecycleUpstreamResolver, ManagementModel, ManagementModelResolver,
    ProfileUpdater, UpstreamResolver,
};
use model_launcher_core::{
    AppError, CatalogService, CatalogWatcher, ConfigDiagnostic, ConfigStore, EngineCapabilities,
    InferenceEngine, LauncherConfig, Lifecycle, LifecycleHandle, LifecycleSnapshot, LogFilter,
    LogRecord, LogStore, LogStoreLimits, ModelRecord, ReconcileOptions, ReconcileResult,
    reconcile_catalog, scan,
};
use tokio::sync::{Mutex, broadcast, watch};

#[derive(Clone)]
pub struct ServiceOptions {
    pub config_dir: PathBuf,
    pub catalog_dir: PathBuf,
    pub gateway: GatewayConfig,
    pub watch_catalog: bool,
    pub shutdown_timeout: Duration,
    #[doc(hidden)]
    pub upstream_override: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Core(#[from] AppError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Gateway(#[from] GatewayConfigError),
    #[error("catalog watcher: {0}")]
    Watcher(String),
    #[error("service is shutting down")]
    ShuttingDown,
    #[error("service shutdown: {0}")]
    Shutdown(ShutdownFailure),
    #[error("authentication token: {0}")]
    Authentication(String),
    #[error("invalid server settings: {0}")]
    InvalidServerSettings(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShutdownFailure(Arc<[String]>);
impl std::fmt::Display for ShutdownFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.join("; "))
    }
}

#[derive(Clone, Debug)]
pub struct ServiceSnapshot {
    pub models: Vec<ModelRecord>,
    pub lifecycle: LifecycleSnapshot,
    pub engine_valid: bool,
    pub engine_diagnostic: Option<String>,
    pub config_diagnostic: Option<ConfigDiagnostic>,
    pub catalog_diagnostic: Option<String>,
}

struct EngineHealth {
    valid: AtomicBool,
    diagnostic: RwLock<Option<String>>,
}

struct GuardedEngine {
    inner: Arc<dyn InferenceEngine>,
    health: Arc<EngineHealth>,
}

impl InferenceEngine for GuardedEngine {
    fn spec(&self) -> model_launcher_core::EngineFuture<'_, model_launcher_core::EngineSpec> {
        self.inner.spec()
    }
    fn probe_capabilities(&self) -> model_launcher_core::EngineFuture<'_, EngineCapabilities> {
        self.inner.probe_capabilities()
    }
    fn validate_launch<'a>(
        &'a self,
        model: &'a ModelRecord,
        settings: &'a model_launcher_core::LaunchSettings,
    ) -> model_launcher_core::EngineFuture<'a, ()> {
        if !self.health.valid.load(Ordering::Acquire) {
            return Box::pin(async { Err(AppError::EngineUnavailable) });
        }
        self.inner.validate_launch(model, settings)
    }
    fn spawn<'a>(
        &'a self,
        model: &'a ModelRecord,
        settings: &'a model_launcher_core::LaunchSettings,
    ) -> model_launcher_core::EngineFuture<'a, Box<dyn model_launcher_core::EngineProcess>> {
        if !self.health.valid.load(Ordering::Acquire) {
            return Box::pin(async { Err(AppError::EngineUnavailable) });
        }
        self.inner.spawn(model, settings)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LauncherSettings {
    pub catalog_directory: PathBuf,
    pub engine_distribution: String,
    pub engine_executable: String,
    pub default_launch_settings: model_launcher_core::LaunchSettings,
}

pub struct Service {
    handle: ServiceHandle,
}

#[async_trait::async_trait]
pub trait EngineSettingsManager: Send + Sync {
    async fn validate(
        &self,
        distribution: &str,
        executable: &str,
    ) -> Result<EngineCapabilities, String>;
    fn apply(&self, distribution: String, executable: String);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownPhase {
    StopGateway,
    StopLifecycle,
    PersistConfig,
    StopWatcher,
    JoinLifecycle,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownEvent {
    Started(ShutdownPhase),
    Completed(ShutdownPhase),
}
#[doc(hidden)]
#[derive(Clone, Copy)]
pub enum WatcherBarrierPoint {
    BeforeGate,
    InsideGate,
}
#[doc(hidden)]
pub struct WatcherBarrier {
    point: WatcherBarrierPoint,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl WatcherBarrier {
    pub fn new(point: WatcherBarrierPoint) -> Arc<Self> {
        Arc::new(Self {
            point,
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        })
    }
    pub async fn entered(&self) {
        self.entered.notified().await;
    }
    pub fn release(&self) {
        self.release.notify_waiters();
    }
    async fn pause(&self) {
        self.entered.notify_one();
        self.release.notified().await;
    }
}
impl Service {
    pub async fn start(
        options: ServiceOptions,
        engine: Arc<dyn InferenceEngine>,
    ) -> Result<Self, ServiceError> {
        let logs = LogStore::new(LogStoreLimits::new(2_000, 2 * 1024 * 1024, 256))?;
        Self::start_inner(options, engine, logs, None, None, None).await
    }

    pub async fn start_with_log_store(
        options: ServiceOptions,
        engine: Arc<dyn InferenceEngine>,
        logs: LogStore,
    ) -> Result<Self, ServiceError> {
        Self::start_inner(options, engine, logs, None, None, None).await
    }

    pub async fn start_with_desktop_dependencies(
        options: ServiceOptions,
        engine: Arc<dyn InferenceEngine>,
        logs: LogStore,
        settings: Arc<dyn EngineSettingsManager>,
    ) -> Result<Self, ServiceError> {
        Self::start_inner(options, engine, logs, Some(settings), None, None).await
    }

    #[doc(hidden)]
    pub async fn start_with_background_task(
        options: ServiceOptions,
        engine: Arc<dyn InferenceEngine>,
        injected_background: Option<tokio::task::JoinHandle<()>>,
    ) -> Result<Self, ServiceError> {
        let logs = LogStore::new(LogStoreLimits::new(2_000, 2 * 1024 * 1024, 256))?;
        Self::start_inner(options, engine, logs, None, injected_background, None).await
    }

    #[doc(hidden)]
    pub async fn start_with_watcher_barrier(
        options: ServiceOptions,
        engine: Arc<dyn InferenceEngine>,
        barrier: Arc<WatcherBarrier>,
    ) -> Result<Self, ServiceError> {
        let logs = LogStore::new(LogStoreLimits::new(2_000, 2 * 1024 * 1024, 256))?;
        Self::start_inner(options, engine, logs, None, None, Some(barrier)).await
    }

    async fn start_inner(
        options: ServiceOptions,
        engine: Arc<dyn InferenceEngine>,
        logs: LogStore,
        settings: Option<Arc<dyn EngineSettingsManager>>,
        injected_background: Option<tokio::task::JoinHandle<()>>,
        watcher_barrier: Option<Arc<WatcherBarrier>>,
    ) -> Result<Self, ServiceError> {
        let store = ConfigStore::new(&options.config_dir);
        let loaded = store.load_with_diagnostic()?;
        let config_diagnostic = loaded.diagnostic.clone();
        let persisted = loaded.config;
        let configured_catalog = persisted.catalog_directory.is_some();
        let catalog_root = persisted
            .catalog_directory
            .clone()
            .unwrap_or_else(|| options.catalog_dir.clone());
        let mut catalog_diagnostic = None;
        if !configured_catalog
            && !catalog_root.is_dir()
            && let Err(error) = std::fs::create_dir_all(&catalog_root)
        {
            catalog_diagnostic = Some(format!("model directory is unavailable: {error}"));
        }
        if !catalog_root.is_dir() {
            catalog_diagnostic.get_or_insert_with(|| "model directory is unavailable".into());
        }
        let catalog = CatalogService::new(&catalog_root, store.clone());
        let initial = if catalog_root.is_dir() {
            catalog.reconcile_now()?
        } else {
            let mut config = persisted;
            for model in &mut config.models {
                model.state = model_launcher_core::ModelState::Missing;
            }
            ReconcileResult {
                config,
                diagnostics: Vec::new(),
            }
        };
        if let (Some(manager), Some(distribution), Some(executable)) = (
            settings.as_ref(),
            initial.config.engine_distribution.clone(),
            initial.config.engine_executable.clone(),
        ) {
            manager.apply(distribution, executable);
        }
        let tokens = match &options.gateway.authentication {
            Authentication::Tokens(tokens) => {
                tokens
                    .replace_phc_hashes(initial.config.auth_token_hashes.clone())
                    .map_err(|error| ServiceError::Authentication(error.to_string()))?;
                tokens.clone()
            }
            Authentication::Disabled => {
                let tokens = Arc::new(model_launcher_api::TokenStore::default());
                tokens
                    .replace_phc_hashes(initial.config.auth_token_hashes.clone())
                    .map_err(|error| ServiceError::Authentication(error.to_string()))?;
                tokens
            }
        };
        let (changes, _) = watch::channel(0_u64);
        let watcher = (catalog_root.is_dir() && options.watch_catalog)
            .then(|| CatalogWatcher::watch(&catalog_root, Duration::from_millis(250)))
            .transpose()
            .map_err(|error| ServiceError::Watcher(error.to_string()))?;
        let (initial_capabilities, initial_diagnostic) = match engine.probe_capabilities().await {
            Ok(capabilities) => (capabilities, None),
            Err(error) => (EngineCapabilities::default(), Some(error.to_string())),
        };
        let health = Arc::new(EngineHealth {
            valid: AtomicBool::new(initial_diagnostic.is_none()),
            diagnostic: RwLock::new(initial_diagnostic),
        });
        let engine: Arc<dyn InferenceEngine> = Arc::new(GuardedEngine {
            inner: engine,
            health: health.clone(),
        });
        let capabilities = Arc::new(RwLock::new(initial_capabilities));
        let models = Arc::new(RwLock::new(initial.config));
        let resolver = Arc::new(ServiceModels {
            models: models.clone(),
            capabilities: capabilities.clone(),
        });
        let lifecycle = Lifecycle::spawn(engine.clone());
        let lifecycle_handle = lifecycle.handle();
        let api_models = api_models(
            &models.read().expect("model lock poisoned"),
            &capabilities.read().expect("capabilities lock"),
        );
        let lifecycle_upstream = LifecycleUpstreamResolver::new(lifecycle_handle.clone());
        let upstream_override = options.upstream_override.clone();
        let upstream = Arc::new(move |model: &ModelRecord| {
            lifecycle_upstream
                .resolve(model)
                .or_else(|| upstream_override.clone())
        });
        let gateway_limits = options.gateway.limits;
        let profiles: Arc<dyn ProfileUpdater> = Arc::new(PersistProfiles {
            store: store.clone(),
            models: models.clone(),
            capabilities: capabilities.clone(),
        });
        let gateway = match Gateway::new_with_management(
            options.gateway,
            api_models.into(),
            lifecycle_handle.clone(),
            upstream.clone(),
            resolver.clone(),
            profiles.clone(),
        ) {
            Ok(gateway) => gateway,
            Err(error) => {
                cleanup_partial_lifecycle(lifecycle_handle, lifecycle, options.shutdown_timeout)
                    .await;
                return Err(error.into());
            }
        };
        let authentication_policy = gateway.authentication_policy();
        let gateway = match gateway.start().await {
            Ok(server) => server,
            Err(error) => {
                cleanup_partial_lifecycle(lifecycle_handle, lifecycle, options.shutdown_timeout)
                    .await;
                return Err(error.into());
            }
        };
        let address = gateway.local_addr();
        let mutation = Arc::new(Mutex::new(()));
        let shutdown = Arc::new(Mutex::new(ShutdownState::Running));
        let (watch_stop, mut watcher_task) = if let Some(mut watcher) = watcher {
            let (stop, mut stopped) = watch::channel(false);
            let watcher_models = models.clone();
            let watcher_mutation = mutation.clone();
            let watcher_shutdown = shutdown.clone();
            let watcher_barrier = watcher_barrier.clone();
            let watcher_changes = changes.clone();
            let task = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        changed = stopped.changed() => if changed.is_err() || *stopped.borrow() { break; },
                        batch = watcher.wait_next_batch() => {
                            let Some(batch)=batch else { break; };
                            if let Some(barrier)=&watcher_barrier && matches!(barrier.point, WatcherBarrierPoint::BeforeGate) { barrier.pause().await; }
                            let _gate=watcher_mutation.lock().await;
                            if !matches!(*watcher_shutdown.lock().await, ShutdownState::Running) { break; }
                            if let Some(barrier)=&watcher_barrier && matches!(barrier.point, WatcherBarrierPoint::InsideGate) { barrier.pause().await; }
                            if let Ok(result)=catalog.process_batch(batch) {
                                *watcher_models.write().expect("model lock poisoned") = result.config;
                                watcher_changes.send_modify(|generation| *generation = generation.wrapping_add(1));
                            }
                        }
                    }
                }
            });
            (Some(stop), Some(task))
        } else {
            (None, None)
        };
        if injected_background.is_some() {
            watcher_task = injected_background;
        }
        let (events, _) = broadcast::channel(32);
        let (shutdown_result, _) = watch::channel(None);
        let inner = Arc::new(Inner {
            address: RwLock::new(address),
            lifecycle: lifecycle_handle,
            capabilities: resolver.capabilities.clone(),
            logs,
            tokens,
            authentication_policy: RwLock::new(authentication_policy),
            gateway_limits,
            upstream,
            resolver,
            profiles,
            settings,
            engine,
            health,
            config_diagnostic,
            catalog_diagnostic: RwLock::new(catalog_diagnostic),
            models,
            catalog_root: RwLock::new(catalog_root),
            watch_catalog: options.watch_catalog,
            recent: RwLock::new(VecDeque::new()),
            store,
            shutdown_timeout: options.shutdown_timeout,
            events,
            shutdown_result,
            changes,
            mutation,
            resources: Mutex::new(Resources {
                gateway: Some(gateway),
                lifecycle: Some(lifecycle),
                watch_stop,
                watcher_task,
            }),
            shutdown,
        });
        Ok(Self {
            handle: ServiceHandle { inner },
        })
    }

    pub fn handle(&self) -> ServiceHandle {
        self.handle.clone()
    }
}

async fn cleanup_partial_lifecycle(
    handle: LifecycleHandle,
    lifecycle: Lifecycle,
    timeout: Duration,
) {
    let _ = tokio::time::timeout(timeout, handle.shutdown()).await;
    drop(handle);
    let _ = lifecycle.wait_for_termination_bounded(timeout).await;
}

#[derive(Clone)]
/// UI-independent command handle.
///
/// Call [`ServiceHandle::shutdown`] for an observed, bounded shutdown. Dropping all handles is
/// still safe: owned gateway and lifecycle senders are dropped, which stops acceptance and causes
/// the lifecycle actor to clean up its owned process, but drop cannot report persistence errors.
pub struct ServiceHandle {
    inner: Arc<Inner>,
}

impl ServiceHandle {
    pub fn local_addr(&self) -> SocketAddr {
        *self.inner.address.read().expect("address lock poisoned")
    }
    pub fn server_settings(&self) -> (SocketAddr, bool) {
        let policy = self
            .inner
            .authentication_policy
            .read()
            .expect("authentication policy lock poisoned")
            .clone();
        let auth_enabled = matches!(
            *policy.read().expect("authentication lock poisoned"),
            Authentication::Tokens(_)
        );
        (self.local_addr(), auth_enabled)
    }
    pub fn snapshot(&self) -> ServiceSnapshot {
        ServiceSnapshot {
            models: self
                .inner
                .models
                .read()
                .expect("model lock poisoned")
                .models
                .clone(),
            lifecycle: self.inner.lifecycle.snapshot(),
            engine_valid: self.inner.health.valid.load(Ordering::Acquire),
            engine_diagnostic: self
                .inner
                .health
                .diagnostic
                .read()
                .expect("engine diagnostic lock poisoned")
                .clone(),
            config_diagnostic: self.inner.config_diagnostic.clone(),
            catalog_diagnostic: self
                .inner
                .catalog_diagnostic
                .read()
                .expect("catalog diagnostic lock poisoned")
                .clone(),
        }
    }
    pub fn capabilities(&self) -> EngineCapabilities {
        self.inner
            .capabilities
            .read()
            .expect("capabilities lock")
            .clone()
    }
    pub fn recent_models(&self) -> Vec<ModelRecord> {
        let models = self.inner.models.read().expect("model lock poisoned");
        self.inner
            .recent
            .read()
            .expect("recent lock poisoned")
            .iter()
            .filter_map(|id| {
                models
                    .models
                    .iter()
                    .find(|model| {
                        model.id == *id
                            && matches!(model.state, model_launcher_core::ModelState::Available)
                    })
                    .cloned()
            })
            .collect()
    }
    pub fn engine_settings(&self) -> (Option<String>, Option<String>) {
        let config = self.inner.models.read().expect("model lock poisoned");
        (
            config.engine_distribution.clone(),
            config.engine_executable.clone(),
        )
    }
    pub fn launcher_settings(&self) -> LauncherSettings {
        let config = self.inner.models.read().expect("model lock poisoned");
        LauncherSettings {
            catalog_directory: self
                .inner
                .catalog_root
                .read()
                .expect("catalog root lock poisoned")
                .clone(),
            engine_distribution: config.engine_distribution.clone().unwrap_or_default(),
            engine_executable: config.engine_executable.clone().unwrap_or_default(),
            default_launch_settings: config.default_launch_settings.clone(),
        }
    }
    pub fn log_store(&self) -> LogStore {
        self.inner.logs.clone()
    }
    pub fn logs(&self, filter: LogFilter) -> Vec<LogRecord> {
        self.inner.logs.filtered_snapshot(filter)
    }
    pub fn export_logs(&self, path: impl AsRef<std::path::Path>) -> Result<(), ServiceError> {
        let file = std::fs::File::create(path)?;
        self.inner.logs.export_json_lines(file)?;
        Ok(())
    }
    pub async fn generate_token(&self) -> Result<CreatedToken, ServiceError> {
        let _mutation = self.inner.mutation.lock().await;
        if !matches!(*self.inner.shutdown.lock().await, ShutdownState::Running) {
            return Err(ServiceError::ShuttingDown);
        }
        let authentication_policy = self
            .inner
            .authentication_policy
            .read()
            .expect("authentication policy lock poisoned")
            .clone();
        if matches!(
            *authentication_policy
                .read()
                .expect("authentication lock poisoned"),
            Authentication::Disabled
        ) {
            return Err(ServiceError::Authentication(
                "authentication is disabled".into(),
            ));
        }
        let tokens = &self.inner.tokens;
        let prior = tokens.phc_hashes();
        let created = tokens
            .create()
            .map_err(|error| ServiceError::Authentication(error.to_string()))?;
        let hashes = tokens.phc_hashes();
        let latest = match self.inner.store.update(|config| {
            config.auth_token_hashes = hashes.clone();
            Ok(())
        }) {
            Ok(latest) => latest,
            Err(error) => {
                let _ = tokens.replace_phc_hashes(prior);
                return Err(error.into());
            }
        };
        self.inner
            .models
            .write()
            .expect("model lock poisoned")
            .auth_token_hashes = latest.auth_token_hashes;
        self.inner.changed();
        Ok(created)
    }

    pub async fn save_server_settings(
        &self,
        bind: IpAddr,
        port: u16,
        auth_enabled: bool,
    ) -> Result<(), ServiceError> {
        let _mutation = self.inner.mutation.lock().await;
        if !matches!(*self.inner.shutdown.lock().await, ShutdownState::Running) {
            return Err(ServiceError::ShuttingDown);
        }
        if port == 0 {
            return Err(ServiceError::InvalidServerSettings(
                "port must be between 1 and 65535".into(),
            ));
        }
        let requested = SocketAddr::new(bind, port);
        let current = self.local_addr();
        let authentication = if auth_enabled {
            Authentication::Tokens(self.inner.tokens.clone())
        } else {
            Authentication::Disabled
        };
        if requested == current {
            let latest = self.inner.store.update(|config| {
                config.bind_address = bind.to_string();
                config.port = port;
                config.auth_enabled = auth_enabled;
                Ok(())
            })?;
            *self.inner.models.write().expect("model lock poisoned") = latest;
            *self
                .inner
                .authentication_policy
                .read()
                .expect("authentication policy lock poisoned")
                .write()
                .expect("authentication lock poisoned") = authentication;
            self.inner.changed();
            return Ok(());
        }

        let replacement = self.inner.gateway(requested, authentication)?;
        let replacement_policy = replacement.authentication_policy();
        let requires_handoff = requested.port() == current.port()
            && requested.ip() != current.ip()
            && (requested.ip().is_unspecified() || current.ip().is_unspecified());
        let rollback_authentication = self.inner.authentication();
        if requires_handoff {
            let old = self.inner.resources.lock().await.gateway.take();
            if let Some(old) = old {
                let _ = old.stop().await;
            }
        }
        let replacement = match replacement.start().await {
            Ok(replacement) => replacement,
            Err(error) => {
                if requires_handoff {
                    self.inner
                        .restore_gateway(current, rollback_authentication)
                        .await?;
                }
                return Err(error.into());
            }
        };
        let latest = match self.inner.store.update(|config| {
            config.bind_address = bind.to_string();
            config.port = replacement.local_addr().port();
            config.auth_enabled = auth_enabled;
            Ok(())
        }) {
            Ok(latest) => latest,
            Err(error) => {
                let _ = replacement.stop().await;
                if requires_handoff {
                    self.inner
                        .restore_gateway(current, rollback_authentication)
                        .await?;
                }
                return Err(error.into());
            }
        };
        let old = {
            let mut resources = self.inner.resources.lock().await;
            resources.gateway.replace(replacement)
        };
        *self.inner.address.write().expect("address lock poisoned") =
            SocketAddr::new(bind, latest.port);
        *self
            .inner
            .authentication_policy
            .write()
            .expect("authentication policy lock poisoned") = replacement_policy;
        *self.inner.models.write().expect("model lock poisoned") = latest;
        self.inner.changed();
        if let Some(old) = old {
            // The replacement is already live and durably committed. `stop` aborts an old
            // listener after its grace period, so a timeout must not make the applied save look
            // rolled back to the caller.
            let _ = old.stop().await;
        }
        Ok(())
    }
    pub async fn save_engine_settings(
        &self,
        distribution: String,
        executable: String,
    ) -> Result<EngineCapabilities, ServiceError> {
        let _mutation = self.inner.mutation.lock().await;
        if !matches!(*self.inner.shutdown.lock().await, ShutdownState::Running) {
            return Err(ServiceError::ShuttingDown);
        }
        let manager = self.inner.settings.as_ref().ok_or_else(|| {
            ServiceError::Authentication("engine settings are not configurable".into())
        })?;
        let caps = manager
            .validate(&distribution, &executable)
            .await
            .map_err(ServiceError::Authentication)?;
        if !matches!(*self.inner.shutdown.lock().await, ShutdownState::Running) {
            return Err(ServiceError::ShuttingDown);
        }
        let latest = self.inner.store.update(|config| {
            config.engine_distribution = Some(distribution.clone());
            config.engine_executable = Some(executable.clone());
            Ok(())
        })?;
        let mut config = self.inner.models.write().expect("model lock poisoned");
        config.engine_distribution = latest.engine_distribution;
        config.engine_executable = latest.engine_executable;
        manager.apply(distribution, executable);
        *self.inner.capabilities.write().expect("capabilities lock") = caps.clone();
        self.inner.health.valid.store(true, Ordering::Release);
        *self
            .inner
            .health
            .diagnostic
            .write()
            .expect("engine diagnostic lock poisoned") = None;
        self.inner.changed();
        Ok(caps)
    }
    pub async fn save_launcher_settings(
        &self,
        settings: LauncherSettings,
    ) -> Result<EngineCapabilities, ServiceError> {
        let _mutation = self.inner.mutation.lock().await;
        if !matches!(*self.inner.shutdown.lock().await, ShutdownState::Running) {
            return Err(ServiceError::ShuttingDown);
        }
        if !settings.catalog_directory.is_dir() {
            return Err(ServiceError::Authentication(
                "model directory must be an existing directory".into(),
            ));
        }
        let manager = self.inner.settings.as_ref().ok_or_else(|| {
            ServiceError::Authentication("engine settings are not configurable".into())
        })?;
        let caps = manager
            .validate(&settings.engine_distribution, &settings.engine_executable)
            .await
            .map_err(ServiceError::Authentication)?;
        if !matches!(*self.inner.shutdown.lock().await, ShutdownState::Running) {
            return Err(ServiceError::ShuttingDown);
        }
        let root = settings.catalog_directory.clone();
        let scanned = scan(&root);
        if !scanned.complete {
            return Err(ServiceError::Authentication(
                "model directory could not be scanned completely".into(),
            ));
        }
        let prepared_watcher = self.inner.prepare_watcher(&root)?;
        let latest = self.inner.store.update(|config| {
            config.catalog_directory = Some(root.clone());
            config.engine_distribution = Some(settings.engine_distribution.clone());
            config.engine_executable = Some(settings.engine_executable.clone());
            config.default_launch_settings = settings.default_launch_settings.clone();
            *config = reconcile_catalog(config, scanned, ReconcileOptions::default()).config;
            Ok(())
        })?;
        *self.inner.models.write().expect("model lock poisoned") = latest;
        *self
            .inner
            .catalog_root
            .write()
            .expect("catalog root lock poisoned") = root.clone();
        self.inner.install_watcher(prepared_watcher, root).await;
        *self
            .inner
            .catalog_diagnostic
            .write()
            .expect("catalog diagnostic lock poisoned") = None;
        manager.apply(settings.engine_distribution, settings.engine_executable);
        *self.inner.capabilities.write().expect("capabilities lock") = caps.clone();
        self.inner.health.valid.store(true, Ordering::Release);
        *self
            .inner
            .health
            .diagnostic
            .write()
            .expect("engine diagnostic lock poisoned") = None;
        self.inner.changed();
        Ok(caps)
    }
    pub async fn load(&self, id: model_launcher_core::ModelId) -> Result<(), ServiceError> {
        let model = self
            .inner
            .models
            .read()
            .expect("model lock poisoned")
            .models
            .iter()
            .find(|model| model.id == id)
            .cloned()
            .ok_or(AppError::ModelNotFound)?;
        self.inner.lifecycle.load(model).await?;
        self.inner.record_recent(id);
        self.inner.changed();
        Ok(())
    }
    pub async fn load_model_with_profile(
        &self,
        id: model_launcher_core::ModelId,
        key: String,
        settings: model_launcher_core::LaunchSettings,
    ) -> Result<(), ServiceError> {
        let _mutation = self.inner.mutation.lock().await;
        let key = model_launcher_core::ModelKey::parse(key)?;
        let mut model = self
            .inner
            .models
            .read()
            .expect("model lock poisoned")
            .models
            .iter()
            .find(|model| model.id == id)
            .cloned()
            .ok_or(AppError::ModelNotFound)?;
        if self
            .inner
            .models
            .read()
            .expect("model lock poisoned")
            .models
            .iter()
            .any(|candidate| candidate.id != id && candidate.key == key)
        {
            return Err(AppError::InvalidModelKey.into());
        }
        model.key = key;
        model.launch_profile.settings = settings;
        self.inner
            .engine
            .validate_launch(&model, &model.launch_profile.settings)
            .await?;
        let next = self.inner.store.update(|config| {
            let record = config
                .models
                .iter_mut()
                .find(|record| record.id == id)
                .ok_or(AppError::ModelNotFound)?;
            *record = model.clone();
            Ok(())
        })?;
        *self.inner.models.write().expect("model lock poisoned") = next;
        self.inner.lifecycle.load(model).await?;
        self.inner.record_recent(id);
        self.inner.changed();
        Ok(())
    }
    pub async fn eject(&self) -> Result<(), ServiceError> {
        self.inner.lifecycle.eject().await?;
        Ok(())
    }
    pub fn subscribe_shutdown(&self) -> broadcast::Receiver<ShutdownEvent> {
        self.inner.events.subscribe()
    }
    pub fn subscribe_lifecycle(&self) -> watch::Receiver<LifecycleSnapshot> {
        self.inner.lifecycle.subscribe()
    }
    pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.inner.changes.subscribe()
    }
    pub async fn rescan(&self, catalog_dir: PathBuf) -> Result<ReconcileResult, ServiceError> {
        let _mutation = self.inner.mutation.lock().await;
        if !matches!(*self.inner.shutdown.lock().await, ShutdownState::Running) {
            return Err(ServiceError::ShuttingDown);
        }
        let result = CatalogService::new(catalog_dir, self.inner.store.clone()).reconcile_now()?;
        *self.inner.models.write().expect("model lock poisoned") = result.config.clone();
        self.inner.changed();
        Ok(result)
    }
    pub async fn shutdown(&self) -> Result<(), ServiceError> {
        let mut result = self.inner.shutdown_result.subscribe();
        let (spawn, done) = {
            let mut state = self.inner.shutdown.lock().await;
            match &*state {
                ShutdownState::Running => {
                    *state = ShutdownState::Stopping;
                    (true, None)
                }
                ShutdownState::Stopping => (false, None),
                ShutdownState::Done(raw) => (false, Some(raw.clone())),
            }
        };
        if let Some(raw) = done {
            return raw.map_err(ServiceError::Shutdown);
        }
        if spawn {
            let inner = self.inner.clone();
            tokio::spawn(async move {
                let raw = std::panic::AssertUnwindSafe(inner.perform_shutdown())
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| {
                        Err(ShutdownFailure(Arc::from([
                            "shutdown task panicked".to_owned()
                        ])))
                    });
                *inner.shutdown.lock().await = ShutdownState::Done(raw.clone());
                inner.shutdown_result.send_replace(Some(raw));
            });
        }
        loop {
            if let Some(raw) = result.borrow().clone() {
                return raw.map_err(ServiceError::Shutdown);
            }
            if result.changed().await.is_err() {
                return Err(ServiceError::Shutdown(ShutdownFailure(Arc::from([
                    "shutdown result channel closed".to_owned(),
                ]))));
            }
        }
    }
}

struct Inner {
    address: RwLock<SocketAddr>,
    lifecycle: LifecycleHandle,
    capabilities: Arc<RwLock<EngineCapabilities>>,
    logs: LogStore,
    tokens: Arc<model_launcher_api::TokenStore>,
    authentication_policy: RwLock<Arc<RwLock<Authentication>>>,
    gateway_limits: model_launcher_api::GatewayLimits,
    upstream: UpstreamResolver,
    resolver: Arc<ServiceModels>,
    profiles: Arc<dyn ProfileUpdater>,
    settings: Option<Arc<dyn EngineSettingsManager>>,
    engine: Arc<dyn InferenceEngine>,
    health: Arc<EngineHealth>,
    config_diagnostic: Option<ConfigDiagnostic>,
    catalog_diagnostic: RwLock<Option<String>>,
    models: Arc<RwLock<LauncherConfig>>,
    catalog_root: RwLock<PathBuf>,
    watch_catalog: bool,
    recent: RwLock<VecDeque<model_launcher_core::ModelId>>,
    store: ConfigStore,
    shutdown_timeout: Duration,
    events: broadcast::Sender<ShutdownEvent>,
    shutdown_result: watch::Sender<Option<Result<(), ShutdownFailure>>>,
    changes: watch::Sender<u64>,
    mutation: Arc<Mutex<()>>,
    resources: Mutex<Resources>,
    shutdown: Arc<Mutex<ShutdownState>>,
}

impl Inner {
    fn authentication(&self) -> Authentication {
        let policy = self
            .authentication_policy
            .read()
            .expect("authentication policy lock poisoned")
            .clone();
        policy.read().expect("authentication lock poisoned").clone()
    }

    fn gateway(
        &self,
        bind: SocketAddr,
        authentication: Authentication,
    ) -> Result<Gateway, GatewayConfigError> {
        let models = api_models(
            &self.models.read().expect("model lock poisoned"),
            &self.capabilities.read().expect("capabilities lock"),
        );
        Gateway::new_with_management(
            GatewayConfig {
                bind,
                authentication,
                limits: self.gateway_limits,
            },
            models.into(),
            self.lifecycle.clone(),
            self.upstream.clone(),
            self.resolver.clone(),
            self.profiles.clone(),
        )
    }

    async fn restore_gateway(
        &self,
        bind: SocketAddr,
        authentication: Authentication,
    ) -> Result<(), ServiceError> {
        let restored = self.gateway(bind, authentication)?;
        let restored_policy = restored.authentication_policy();
        let restored = restored.start().await?;
        *self
            .authentication_policy
            .write()
            .expect("authentication policy lock poisoned") = restored_policy;
        self.resources.lock().await.gateway = Some(restored);
        Ok(())
    }

    fn changed(&self) {
        self.changes
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    fn prepare_watcher(
        &self,
        root: &std::path::Path,
    ) -> Result<Option<CatalogWatcher>, ServiceError> {
        if !self.watch_catalog {
            return Ok(None);
        }
        CatalogWatcher::watch(root, Duration::from_millis(250))
            .map(Some)
            .map_err(|error| ServiceError::Watcher(error.to_string()))
    }

    async fn install_watcher(&self, watcher: Option<CatalogWatcher>, root: PathBuf) {
        let Some(mut watcher) = watcher else { return };
        let (stop, mut stopped) = watch::channel(false);
        let catalog = CatalogService::new(root, self.store.clone());
        let models = self.models.clone();
        let mutation = self.mutation.clone();
        let shutdown = self.shutdown.clone();
        let changes = self.changes.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = stopped.changed() => if changed.is_err() || *stopped.borrow() { break; },
                    batch = watcher.wait_next_batch() => {
                        let Some(batch) = batch else { break; };
                        let _gate = mutation.lock().await;
                        if !matches!(*shutdown.lock().await, ShutdownState::Running) { break; }
                        if let Ok(result) = catalog.process_batch(batch) {
                            *models.write().expect("model lock poisoned") = result.config;
                            changes.send_modify(|generation| *generation = generation.wrapping_add(1));
                        }
                    }
                }
            }
        });
        let mut resources = self.resources.lock().await;
        if let Some(old_stop) = resources.watch_stop.replace(stop) {
            let _ = old_stop.send(true);
        }
        if let Some(old_task) = resources.watcher_task.replace(task) {
            old_task.abort();
            let _ = old_task.await;
        }
    }

    fn record_recent(&self, id: model_launcher_core::ModelId) {
        let mut recent = self.recent.write().expect("recent lock poisoned");
        recent.retain(|candidate| *candidate != id);
        recent.push_front(id);
        recent.truncate(8);
    }

    fn event(&self, event: ShutdownEvent) {
        let _ = self.events.send(event);
    }
    async fn perform_shutdown(&self) -> Result<(), ShutdownFailure> {
        let (gateway, lifecycle, watch_stop, watcher_task) = {
            let _mutation = self.mutation.lock().await;
            let mut resources = self.resources.lock().await;
            (
                resources.gateway.take(),
                resources.lifecycle.take(),
                resources.watch_stop.take(),
                resources.watcher_task.take(),
            )
        };
        let mut errors = Vec::new();
        self.event(ShutdownEvent::Started(ShutdownPhase::StopGateway));
        if let Some(gateway) = gateway
            && let Err(error) = gateway.stop().await
        {
            errors.push(error.to_string());
        }
        self.event(ShutdownEvent::Completed(ShutdownPhase::StopGateway));
        self.event(ShutdownEvent::Started(ShutdownPhase::StopLifecycle));
        match tokio::time::timeout(self.shutdown_timeout, self.lifecycle.shutdown()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => errors.push(error.to_string()),
            Err(_) => errors.push("lifecycle shutdown timed out".into()),
        }
        self.event(ShutdownEvent::Completed(ShutdownPhase::StopLifecycle));
        {
            let _mutation = self.mutation.lock().await;
            self.event(ShutdownEvent::Started(ShutdownPhase::PersistConfig));
            let latest = self.models.read().expect("model lock poisoned").clone();
            if let Err(error) = self.store.update(|disk| {
                disk.models = latest.models.clone();
                Ok(())
            }) {
                errors.push(error.to_string());
            }
            self.event(ShutdownEvent::Completed(ShutdownPhase::PersistConfig));
        }
        self.event(ShutdownEvent::Started(ShutdownPhase::StopWatcher));
        if let Some(stop) = watch_stop {
            let _ = stop.send(true);
        }
        if let Some(mut task) = watcher_task
            && tokio::time::timeout(self.shutdown_timeout, &mut task)
                .await
                .is_err()
        {
            task.abort();
            let _ = task.await;
            errors.push("catalog watcher join timed out".into());
        }
        self.event(ShutdownEvent::Completed(ShutdownPhase::StopWatcher));
        self.event(ShutdownEvent::Started(ShutdownPhase::JoinLifecycle));
        if let Some(lifecycle) = lifecycle
            && !lifecycle
                .wait_for_termination_bounded(self.shutdown_timeout)
                .await
        {
            errors.push("lifecycle join timed out; actor aborted and joined".into());
        }
        self.event(ShutdownEvent::Completed(ShutdownPhase::JoinLifecycle));
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ShutdownFailure(errors.into()))
        }
    }
}

struct Resources {
    gateway: Option<GatewayServer>,
    lifecycle: Option<Lifecycle>,
    watch_stop: Option<watch::Sender<bool>>,
    watcher_task: Option<tokio::task::JoinHandle<()>>,
}
enum ShutdownState {
    Running,
    Stopping,
    Done(Result<(), ShutdownFailure>),
}

struct ServiceModels {
    models: Arc<RwLock<LauncherConfig>>,
    capabilities: Arc<RwLock<EngineCapabilities>>,
}
impl ManagementModelResolver for ServiceModels {
    fn models(&self) -> Option<Vec<ApiModel>> {
        Some(api_models(
            &self.models.read().expect("model lock poisoned"),
            &self.capabilities.read().expect("capabilities lock"),
        ))
    }

    fn resolve(&self, key: &str) -> Option<ManagementModel> {
        let capabilities = self.capabilities.read().ok()?.clone();
        self.models
            .read()
            .ok()?
            .models
            .iter()
            .find(|model| model.key.as_str() == key)
            .cloned()
            .map(|model| ManagementModel {
                model,
                capabilities,
            })
    }
}
struct PersistProfiles {
    store: ConfigStore,
    models: Arc<RwLock<LauncherConfig>>,
    capabilities: Arc<RwLock<EngineCapabilities>>,
}
impl ProfileUpdater for PersistProfiles {
    fn apply(
        &self,
        resolved: ManagementModel,
        request: &model_launcher_api::LoadRequest,
    ) -> Result<ModelRecord, AppError> {
        let capabilities = self
            .capabilities
            .read()
            .map_err(|_| AppError::ConfigFormat("capabilities lock poisoned".into()))?;
        let model = apply_profile(resolved.model, &capabilities, request)?;
        let next = self.store.update(|config| {
            let record = config
                .models
                .iter_mut()
                .find(|record| record.id == model.id)
                .ok_or(AppError::ModelNotFound)?;
            *record = model.clone();
            Ok(())
        })?;
        *self
            .models
            .write()
            .map_err(|_| AppError::ConfigFormat("model lock poisoned".into()))? = next;
        Ok(model)
    }
}

fn apply_profile(
    mut model: ModelRecord,
    capabilities: &EngineCapabilities,
    request: &model_launcher_api::LoadRequest,
) -> Result<ModelRecord, AppError> {
    use model_launcher_core::{BatchSize, ContextLength};
    if request.context_length.is_some() && !capabilities.context_length {
        return Err(AppError::InvalidSetting("context_length"));
    }
    if request.eval_batch_size.is_some() && !capabilities.batch_size {
        return Err(AppError::InvalidSetting("eval_batch_size"));
    }
    if request.flash_attention.is_some() && !capabilities.flash_attention {
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

fn api_models(config: &LauncherConfig, capabilities: &EngineCapabilities) -> Vec<ApiModel> {
    config
        .models
        .iter()
        .cloned()
        .map(|record| ApiModel {
            record,
            publisher: "local".into(),
            architecture: None,
            quantization: None,
            params_string: None,
            capabilities: capabilities.clone(),
        })
        .collect()
}
