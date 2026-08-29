use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use futures_util::FutureExt as _;
use model_launcher_api::{
    ApiModel, Gateway, GatewayConfig, GatewayConfigError, GatewayServer, ManagementModel,
    ManagementModelResolver, ProfileUpdater, UpstreamResolver,
};
use model_launcher_core::{
    AppError, CatalogService, CatalogWatcher, ConfigStore, EngineCapabilities, InferenceEngine,
    LauncherConfig, Lifecycle, LifecycleHandle, LifecycleSnapshot, ModelRecord, ReconcileResult,
};
use tokio::sync::{Mutex, broadcast, watch};

#[derive(Clone)]
pub struct ServiceOptions {
    pub config_dir: PathBuf,
    pub catalog_dir: PathBuf,
    pub gateway: GatewayConfig,
    pub upstream: String,
    pub watch_catalog: bool,
    pub shutdown_timeout: Duration,
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
}

pub struct Service {
    handle: ServiceHandle,
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
        Self::start_inner(options, engine, None, None).await
    }

    #[doc(hidden)]
    pub async fn start_with_background_task(
        options: ServiceOptions,
        engine: Arc<dyn InferenceEngine>,
        injected_background: Option<tokio::task::JoinHandle<()>>,
    ) -> Result<Self, ServiceError> {
        Self::start_inner(options, engine, injected_background, None).await
    }

    #[doc(hidden)]
    pub async fn start_with_watcher_barrier(
        options: ServiceOptions,
        engine: Arc<dyn InferenceEngine>,
        barrier: Arc<WatcherBarrier>,
    ) -> Result<Self, ServiceError> {
        Self::start_inner(options, engine, None, Some(barrier)).await
    }

    async fn start_inner(
        options: ServiceOptions,
        engine: Arc<dyn InferenceEngine>,
        injected_background: Option<tokio::task::JoinHandle<()>>,
        watcher_barrier: Option<Arc<WatcherBarrier>>,
    ) -> Result<Self, ServiceError> {
        let store = ConfigStore::new(&options.config_dir);
        let catalog = CatalogService::new(&options.catalog_dir, store.clone());
        let initial = catalog.reconcile_now()?;
        let watcher = options
            .watch_catalog
            .then(|| CatalogWatcher::watch(&options.catalog_dir, Duration::from_millis(250)))
            .transpose()
            .map_err(|error| ServiceError::Watcher(error.to_string()))?;
        let capabilities = engine.probe_capabilities().await?;
        let models = Arc::new(RwLock::new(initial.config));
        let resolver = Arc::new(ServiceModels {
            models: models.clone(),
            capabilities: capabilities.clone(),
        });
        let lifecycle = Lifecycle::spawn(engine);
        let lifecycle_handle = lifecycle.handle();
        let api_models = api_models(&models.read().expect("model lock poisoned"), &capabilities);
        let upstream = options.upstream.clone();
        let upstream: UpstreamResolver = Arc::new(move |_| Some(upstream.clone()));
        let gateway = match Gateway::new_with_management(
            options.gateway,
            api_models.into(),
            lifecycle_handle.clone(),
            upstream,
            resolver.clone(),
            Arc::new(PersistProfiles {
                store: store.clone(),
                models: models.clone(),
                capabilities,
            }),
        ) {
            Ok(gateway) => gateway,
            Err(error) => {
                cleanup_partial_lifecycle(lifecycle_handle, lifecycle, options.shutdown_timeout)
                    .await;
                return Err(error.into());
            }
        };
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
                            if let Ok(result)=catalog.process_batch(batch) { *watcher_models.write().expect("model lock poisoned") = result.config; }
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
            address,
            lifecycle: lifecycle_handle,
            models,
            store,
            shutdown_timeout: options.shutdown_timeout,
            events,
            shutdown_result,
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
        self.inner.address
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
        }
    }
    pub fn subscribe_shutdown(&self) -> broadcast::Receiver<ShutdownEvent> {
        self.inner.events.subscribe()
    }
    pub fn subscribe_lifecycle(&self) -> watch::Receiver<LifecycleSnapshot> {
        self.inner.lifecycle.subscribe()
    }
    pub async fn rescan(&self, catalog_dir: PathBuf) -> Result<ReconcileResult, ServiceError> {
        let _mutation = self.inner.mutation.lock().await;
        if !matches!(*self.inner.shutdown.lock().await, ShutdownState::Running) {
            return Err(ServiceError::ShuttingDown);
        }
        let result = CatalogService::new(catalog_dir, self.inner.store.clone()).reconcile_now()?;
        *self.inner.models.write().expect("model lock poisoned") = result.config.clone();
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
    address: SocketAddr,
    lifecycle: LifecycleHandle,
    models: Arc<RwLock<LauncherConfig>>,
    store: ConfigStore,
    shutdown_timeout: Duration,
    events: broadcast::Sender<ShutdownEvent>,
    shutdown_result: watch::Sender<Option<Result<(), ShutdownFailure>>>,
    mutation: Arc<Mutex<()>>,
    resources: Mutex<Resources>,
    shutdown: Arc<Mutex<ShutdownState>>,
}

impl Inner {
    fn event(&self, event: ShutdownEvent) {
        let _ = self.events.send(event);
    }
    async fn perform_shutdown(&self) -> Result<(), ShutdownFailure> {
        let (gateway, lifecycle, watch_stop, watcher_task) = {
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
    capabilities: EngineCapabilities,
}
impl ManagementModelResolver for ServiceModels {
    fn resolve(&self, key: &str) -> Option<ManagementModel> {
        self.models
            .read()
            .ok()?
            .models
            .iter()
            .find(|model| model.key.as_str() == key)
            .cloned()
            .map(|model| ManagementModel {
                model,
                capabilities: self.capabilities.clone(),
            })
    }
}
struct PersistProfiles {
    store: ConfigStore,
    models: Arc<RwLock<LauncherConfig>>,
    capabilities: EngineCapabilities,
}
impl ProfileUpdater for PersistProfiles {
    fn apply(
        &self,
        resolved: ManagementModel,
        request: &model_launcher_api::LoadRequest,
    ) -> Result<ModelRecord, AppError> {
        let model = apply_profile(resolved.model, &self.capabilities, request)?;
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
