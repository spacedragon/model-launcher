use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use model_launcher_api::{
    ApiModel, Gateway, GatewayConfig, GatewayConfigError, GatewayServer, ManagementModel,
    ManagementModelResolver, ProfileUpdater, UpstreamResolver,
};
use model_launcher_core::{
    AppError, CatalogService, CatalogWatcher, ConfigStore, EngineCapabilities, InferenceEngine,
    LauncherConfig, Lifecycle, LifecycleHandle, LifecycleSnapshot, ModelRecord, ReconcileResult,
};
use tokio::sync::{Mutex, Notify, watch};

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
    #[error("service shutdown: {0}")]
    Shutdown(String),
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
pub trait ServiceObserver: Send + Sync {
    fn shutdown_event(&self, _event: ShutdownEvent) {}
}
struct NoopObserver;
impl ServiceObserver for NoopObserver {}

impl Service {
    pub async fn start(
        options: ServiceOptions,
        engine: Arc<dyn InferenceEngine>,
    ) -> Result<Self, ServiceError> {
        Self::start_observed(options, engine, Arc::new(NoopObserver)).await
    }

    pub async fn start_observed(
        options: ServiceOptions,
        engine: Arc<dyn InferenceEngine>,
        observer: Arc<dyn ServiceObserver>,
    ) -> Result<Self, ServiceError> {
        Self::start_with_background_task(options, engine, observer, None).await
    }

    #[doc(hidden)]
    pub async fn start_with_background_task(
        options: ServiceOptions,
        engine: Arc<dyn InferenceEngine>,
        observer: Arc<dyn ServiceObserver>,
        injected_background: Option<tokio::task::JoinHandle<()>>,
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
        let (watch_stop, mut watcher_task) = if let Some(mut watcher) = watcher {
            let (stop, mut stopped) = watch::channel(false);
            let watcher_models = models.clone();
            let task = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        changed = stopped.changed() => if changed.is_err() || *stopped.borrow() { break; },
                        result = watcher.process_next(&catalog) => match result {
                            Ok(Some(result)) => *watcher_models.write().expect("model lock poisoned") = result.config,
                            Ok(None) => break,
                            Err(_) => {}
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
        let inner = Arc::new(Inner {
            address,
            lifecycle: lifecycle_handle,
            models,
            store,
            shutdown_timeout: options.shutdown_timeout,
            observer,
            resources: Mutex::new(Resources {
                gateway: Some(gateway),
                lifecycle: Some(lifecycle),
                watch_stop,
                watcher_task,
            }),
            shutdown: Mutex::new(ShutdownState::Running),
            shutdown_done: Notify::new(),
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
    let _ = tokio::time::timeout(timeout, lifecycle.wait_for_termination()).await;
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
    pub fn rescan(&self, catalog_dir: PathBuf) -> Result<ReconcileResult, ServiceError> {
        let result = CatalogService::new(catalog_dir, self.inner.store.clone()).reconcile_now()?;
        *self.inner.models.write().expect("model lock poisoned") = result.config.clone();
        Ok(result)
    }
    pub async fn shutdown(&self) -> Result<(), ServiceError> {
        loop {
            let notified = self.inner.shutdown_done.notified();
            let leader = {
                let mut state = self.inner.shutdown.lock().await;
                match &*state {
                    ShutdownState::Running => {
                        *state = ShutdownState::Stopping;
                        true
                    }
                    ShutdownState::Stopping => false,
                    ShutdownState::Done(result) => {
                        return result.clone().map_err(ServiceError::Shutdown);
                    }
                }
            };
            if !leader {
                notified.await;
                continue;
            }
            let result = self.inner.perform_shutdown().await;
            let stored = result.as_ref().map(|_| ()).map_err(ToString::to_string);
            *self.inner.shutdown.lock().await = ShutdownState::Done(stored);
            self.inner.shutdown_done.notify_waiters();
            return result;
        }
    }
}

struct Inner {
    address: SocketAddr,
    lifecycle: LifecycleHandle,
    models: Arc<RwLock<LauncherConfig>>,
    store: ConfigStore,
    shutdown_timeout: Duration,
    observer: Arc<dyn ServiceObserver>,
    resources: Mutex<Resources>,
    shutdown: Mutex<ShutdownState>,
    shutdown_done: Notify,
}

impl Inner {
    fn event(&self, event: ShutdownEvent) {
        self.observer.shutdown_event(event);
    }
    async fn perform_shutdown(&self) -> Result<(), ServiceError> {
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
        self.event(ShutdownEvent::Started(ShutdownPhase::PersistConfig));
        let latest = self.models.read().expect("model lock poisoned").clone();
        if let Err(error) = self.store.save(&latest) {
            errors.push(error.to_string());
        }
        self.event(ShutdownEvent::Completed(ShutdownPhase::PersistConfig));
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
            && tokio::time::timeout(self.shutdown_timeout, lifecycle.wait_for_termination())
                .await
                .is_err()
        {
            errors.push("lifecycle join timed out".into());
        }
        self.event(ShutdownEvent::Completed(ShutdownPhase::JoinLifecycle));
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ServiceError::Shutdown(errors.join("; ")))
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
    Done(Result<(), String>),
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
