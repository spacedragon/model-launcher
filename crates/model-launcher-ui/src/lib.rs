use std::{fmt, io, rc::Rc, sync::Arc};

use model_launcher_core::{
    EngineCapabilities, LaunchSettings, LifecycleSnapshot, LogFilter, LogRecord, LogStore, ModelId,
    ModelRecord, SettingId,
};

pub mod tray;
pub use tray::{TrayCommand, TrayController};

slint::include_modules!();

#[derive(Clone)]
pub struct UiActions {
    pub load: Arc<dyn Fn(ModelId) + Send + Sync>,
    pub eject: Arc<dyn Fn() + Send + Sync>,
    pub rescan: Arc<dyn Fn() + Send + Sync>,
}

pub fn run_desktop(
    snapshot: AppSnapshot,
    address: String,
    actions: UiActions,
) -> Result<(), slint::PlatformError> {
    use slint::{ComponentHandle as _, ModelRc, SharedString, VecModel};
    let view_model = ViewModel::from_snapshot(snapshot);
    let rows: Vec<ModelDisplay> = view_model
        .rows()
        .iter()
        .map(|row| {
            let action = view_model.action(row.id);
            let (label, enabled) = match action {
                ModelAction::Load => ("Load", true),
                ModelAction::Eject => ("Eject", true),
                ModelAction::Disabled(_) => ("Load", false),
            };
            ModelDisplay {
                id: row.id.as_uuid().to_string().into(),
                name: row.name.clone().into(),
                key: row.key.clone().into(),
                size: row.size.clone().into(),
                path: row.path.clone().into(),
                status: row.status.clone().into(),
                action: label.into(),
                action_enabled: enabled,
            }
        })
        .collect();
    let window = MainWindow::new()?;
    window.set_models(ModelRc::from(Rc::new(VecModel::from(rows))));
    window.set_base_url(format!("http://{address}").into());
    window.set_service_status(format!("{:?}", view_model.snapshot.lifecycle.state).into());
    window.on_load_model({
        let actions = actions.clone();
        move |raw: SharedString| {
            if let Ok(id) = uuid::Uuid::parse_str(&raw) {
                (actions.load)(ModelId::from_uuid(id));
            }
        }
    });
    window.on_eject_model({
        let actions = actions.clone();
        move |_| (actions.eject)()
    });
    window.on_rescan(move || (actions.rescan)());
    window.run()
}

#[derive(Clone, Debug)]
pub struct AppSnapshot {
    pub models: Vec<ModelRecord>,
    pub lifecycle: LifecycleSnapshot,
    pub capabilities: EngineCapabilities,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRow {
    pub id: ModelId,
    pub name: String,
    pub key: String,
    pub path: String,
    pub size: String,
    pub status: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetadataVisibility {
    pub size: bool,
    pub path: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelAction {
    Load,
    Eject,
    Disabled(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingField {
    pub id: &'static str,
    pub visible: bool,
    pub retained_unsupported: bool,
}

pub struct ViewModel {
    rows: Vec<ModelRow>,
    snapshot: AppSnapshot,
}

impl ViewModel {
    #[must_use]
    pub fn from_snapshot(snapshot: AppSnapshot) -> Self {
        let rows = snapshot
            .models
            .iter()
            .map(|model| ModelRow {
                id: model.id,
                name: model.display_name.clone(),
                key: model.key.to_string(),
                path: model.path.display().to_string(),
                size: format_bytes(model.size_bytes),
                status: if snapshot.lifecycle.desired_model == Some(model.id) {
                    format!("{:?}", snapshot.lifecycle.state)
                } else {
                    format!("{:?}", model.state)
                },
            })
            .collect();
        Self { rows, snapshot }
    }

    #[must_use]
    pub fn rows(&self) -> &[ModelRow] {
        &self.rows
    }

    #[must_use]
    pub const fn metadata_visibility(&self, width: u32) -> MetadataVisibility {
        MetadataVisibility {
            size: width >= 500,
            path: width >= 720,
        }
    }

    #[must_use]
    pub fn action(&self, id: ModelId) -> ModelAction {
        if self.snapshot.lifecycle.desired_model == Some(id) {
            return ModelAction::Eject;
        }
        if self.snapshot.lifecycle.in_flight > 0 {
            return ModelAction::Disabled(
                "Finish active requests or eject the current model first.".into(),
            );
        }
        ModelAction::Load
    }

    #[must_use]
    pub fn setting_fields(
        settings: &LaunchSettings,
        caps: &EngineCapabilities,
    ) -> Vec<SettingField> {
        let rendered = settings.render(caps);
        let unsupported = |id| rendered.unsupported.contains(&id);
        vec![
            field(
                "context_length",
                caps.context_length,
                unsupported(SettingId::ContextLength),
            ),
            field(
                "gpu_layers",
                caps.gpu_layers,
                unsupported(SettingId::GpuLayers),
            ),
            field(
                "cpu_threads",
                caps.cpu_threads,
                unsupported(SettingId::CpuThreads),
            ),
            field(
                "batch_size",
                caps.batch_size,
                unsupported(SettingId::BatchSize),
            ),
            field(
                "parallel_slots",
                caps.parallel_slots,
                unsupported(SettingId::ParallelSlots),
            ),
            field(
                "flash_attention",
                caps.flash_attention,
                unsupported(SettingId::FlashAttention),
            ),
            field(
                "kv_cache_type",
                caps.kv_cache_type,
                unsupported(SettingId::KvCacheType),
            ),
        ]
    }
}

fn field(id: &'static str, visible: bool, retained_unsupported: bool) -> SettingField {
    SettingField {
        id,
        visible,
        retained_unsupported,
    }
}
fn format_bytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
}

#[derive(Clone, Debug, Default)]
pub struct CloseNotice {
    shown: bool,
}
impl CloseNotice {
    pub const fn from_persisted(shown: bool) -> Self {
        Self { shown }
    }
    pub fn take(&mut self) -> Option<&'static str> {
        if self.shown {
            None
        } else {
            self.shown = true;
            Some("Model Launcher keeps running in the notification area.")
        }
    }
    #[must_use]
    pub const fn was_shown(&self) -> bool {
        self.shown
    }
}

pub struct TokenReveal(Option<String>);
impl TokenReveal {
    pub fn new(token: impl Into<String>) -> Self {
        Self(Some(token.into()))
    }
    pub fn take(&mut self) -> Option<String> {
        self.0.take()
    }
}
impl fmt::Debug for TokenReveal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TokenReveal")
            .field(&self.0.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}
impl Drop for TokenReveal {
    fn drop(&mut self) {
        if let Some(value) = self.0.as_mut() {
            unsafe {
                value.as_bytes_mut().fill(0);
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineSettings {
    pub distribution: String,
    pub executable: String,
    pub model_directory: String,
}

type Validate = dyn Fn(&EngineSettings) -> Result<(), String> + Send + Sync;
type Probe = dyn Fn() -> Result<EngineCapabilities, String> + Send + Sync;
pub struct SaveSettings {
    validate: Arc<Validate>,
    probe: Arc<Probe>,
}
impl SaveSettings {
    pub fn new(
        validate: impl Fn(&EngineSettings) -> Result<(), String> + Send + Sync + 'static,
        probe: impl Fn() -> Result<EngineCapabilities, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            validate: Arc::new(validate),
            probe: Arc::new(probe),
        }
    }
    pub fn run(&self, settings: &EngineSettings) -> Result<EngineCapabilities, String> {
        (self.validate)(settings)?;
        (self.probe)()
    }
}

pub struct RecentModels {
    limit: usize,
    ids: Vec<ModelId>,
}
impl RecentModels {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            ids: Vec::new(),
        }
    }
    pub fn record(&mut self, id: ModelId) {
        self.ids.retain(|candidate| *candidate != id);
        self.ids.insert(0, id);
        self.ids.truncate(self.limit);
    }
    #[must_use]
    pub fn ids(&self) -> &[ModelId] {
        &self.ids
    }
}

#[derive(Clone)]
pub struct LogCommands {
    store: LogStore,
}
impl LogCommands {
    pub const fn new(store: LogStore) -> Self {
        Self { store }
    }
    #[must_use]
    pub fn snapshot(&self, filter: LogFilter) -> Vec<LogRecord> {
        self.store.filtered_snapshot(filter)
    }
    pub fn export(&self, writer: impl io::Write) -> io::Result<()> {
        self.store.export_json_lines(writer)
    }
}
