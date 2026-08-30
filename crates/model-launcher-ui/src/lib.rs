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
    pub load: Arc<dyn Fn(UiLoadRequest) + Send + Sync>,
    pub eject: Arc<dyn Fn() + Send + Sync>,
    pub rescan: Arc<dyn Fn() + Send + Sync>,
    pub snapshot: Arc<dyn Fn() -> AppSnapshot + Send + Sync>,
    pub quit: Arc<dyn Fn() + Send + Sync>,
    pub close_notice: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    pub save_settings: Arc<dyn Fn(EngineSettings) + Send + Sync>,
    pub logs: Arc<dyn Fn(LogFilter) -> Vec<LogRecord> + Send + Sync>,
    pub export_logs: Arc<dyn Fn() + Send + Sync>,
    pub generate_token: Arc<dyn Fn() + Send + Sync>,
    pub engine_settings: Arc<dyn Fn() -> EngineSettings + Send + Sync>,
}

#[derive(Clone)]
pub struct UiLoadRequest {
    pub id: ModelId,
    pub key: String,
    pub settings: LaunchSettings,
}

#[must_use]
pub fn load_request_for(snapshot: &AppSnapshot, id: ModelId) -> Option<UiLoadRequest> {
    snapshot
        .models
        .iter()
        .find(|model| model.id == id)
        .map(|model| UiLoadRequest {
            id: model.id,
            key: model.key.to_string(),
            settings: model.launch_profile.settings.clone(),
        })
}

#[must_use]
pub fn load_dialog_values_for(snapshot: &AppSnapshot, id: ModelId) -> Option<UiLoadRequest> {
    load_request_for(snapshot, id)
}

#[must_use]
pub fn load_dialog_fields_for(snapshot: &AppSnapshot, id: ModelId) -> Option<Vec<SettingField>> {
    let model = snapshot.models.iter().find(|model| model.id == id)?;
    Some(ViewModel::setting_fields(
        &model.launch_profile.settings,
        &snapshot.capabilities,
    ))
}

#[cfg(not(windows))]
pub fn run_desktop(
    snapshot: AppSnapshot,
    address: String,
    actions: UiActions,
) -> Result<(), slint::PlatformError> {
    use slint::{ComponentHandle as _, ModelRc, VecModel};
    let view_model = ViewModel::from_snapshot(snapshot);
    let rows = display_rows(&view_model, "");
    let window = MainWindow::new()?;
    window.set_models(ModelRc::from(Rc::new(VecModel::from(rows))));
    window.set_base_url(format!("http://{address}").into());
    hydrate_server(&window, &view_model.snapshot);
    window.set_service_status(format!("{:?}", view_model.snapshot.lifecycle.state).into());
    hydrate_capabilities(&window, &view_model.snapshot.capabilities);
    set_logs(&window, (actions.logs)(LogFilter::default()));
    hydrate_engine_settings(&window, (actions.engine_settings)());
    install_load_callback(&window, actions.clone());
    install_prepare_load(&window, actions.clone());
    install_model_search(&window, actions.clone());
    window.on_eject_model({
        let actions = actions.clone();
        move |_| (actions.eject)()
    });
    window.on_rescan({
        let actions = actions.clone();
        move || (actions.rescan)()
    });
    window.on_save_settings({
        let actions = actions.clone();
        move |model_directory,
              distribution,
              executable,
              context,
              gpu,
              threads,
              batch,
              parallel,
              flash,
              kv| {
            (actions.save_settings)(EngineSettings {
                model_directory: model_directory.into(),
                distribution: distribution.into(),
                executable: executable.into(),
                defaults: launch_settings(context, gpu, threads, batch, parallel, flash, &kv),
            })
        }
    });
    window.on_export_logs({
        let actions = actions.clone();
        move || (actions.export_logs)()
    });
    let weak = window.as_weak();
    window.on_generate_token({
        let actions = actions.clone();
        move || (actions.generate_token)()
    });
    window.on_dismiss_token(move || {
        if let Some(window) = weak.upgrade() {
            window.set_generated_token("".into());
        }
        clear_token_reveal();
    });
    window.on_copy_text(|text| {
        use copypasta::{ClipboardContext, ClipboardProvider as _};
        if let Ok(mut clipboard) = ClipboardContext::new() {
            let _ = clipboard.set_contents(text.into());
        }
    });
    install_log_filter(
        &window,
        actions.clone(),
        Arc::new(std::sync::RwLock::new(LogFilter::default())),
    );
    window.run()
}

pub struct WindowManager {
    window: Option<MainWindow>,
    last_closed: slint::Weak<MainWindow>,
    address: String,
    actions: UiActions,
    token_reveal: Option<TokenReveal>,
    token_timeout: slint::Timer,
    current_log_filter: Arc<std::sync::RwLock<LogFilter>>,
}

impl WindowManager {
    pub fn new(address: String, actions: UiActions) -> Self {
        Self {
            window: None,
            last_closed: slint::Weak::default(),
            address,
            actions,
            token_reveal: None,
            token_timeout: slint::Timer::default(),
            current_log_filter: Arc::new(std::sync::RwLock::new(LogFilter::default())),
        }
    }

    pub fn open(&mut self) -> Result<slint::Weak<MainWindow>, slint::PlatformError> {
        use slint::ComponentHandle as _;
        if let Some(window) = &self.window {
            window.show()?;
            return Ok(window.as_weak());
        }
        let window = create_window(
            (self.actions.snapshot)(),
            self.address.clone(),
            self.actions.clone(),
            self.current_log_filter.clone(),
        )?;
        #[cfg(windows)]
        window.window().on_close_requested(|| {
            let show_notice = DESKTOP.with_borrow(|desktop| {
                let Some(desktop) = desktop.as_ref() else {
                    return false;
                };
                let Some(message) = (desktop.windows.actions.close_notice)() else {
                    return false;
                };
                if let Some(window) = desktop.windows.window.as_ref() {
                    window.set_close_notice_message(message.into());
                    window.set_close_notice_open(true);
                }
                true
            });
            if show_notice {
                slint::CloseRequestResponse::KeepWindowShown
            } else {
                let _ = slint::invoke_from_event_loop(|| {
                    DESKTOP.with_borrow_mut(|desktop| {
                        if let Some(desktop) = desktop.as_mut() {
                            let _ = desktop.windows.close();
                        }
                    })
                });
                slint::CloseRequestResponse::HideWindow
            }
        });
        let weak = window.as_weak();
        window.show()?;
        self.window = Some(window);
        Ok(weak)
    }

    pub fn close(&mut self) -> Result<(), slint::PlatformError> {
        use slint::ComponentHandle as _;
        if let Some(window) = self.window.take() {
            self.clear_token();
            self.last_closed = window.as_weak();
            window.hide()?;
            drop(window);
        }
        debug_assert!(self.last_closed.upgrade().is_none());
        Ok(())
    }

    #[cfg(windows)]
    fn reveal_token(&mut self, token: String) {
        self.clear_token();
        self.token_reveal = Some(TokenReveal::new(token));
        if let (Some(window), Some(token)) = (
            self.window.as_ref(),
            self.token_reveal.as_ref().and_then(TokenReveal::expose),
        ) {
            window.set_generated_token(token.into());
        }
        self.token_timeout.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_secs(60),
            clear_token_reveal,
        );
    }

    fn clear_token(&mut self) {
        self.token_timeout.stop();
        if let Some(window) = self.window.as_ref() {
            window.set_generated_token("".into());
        }
        if let Some(token) = self.token_reveal.as_mut() {
            token.clear();
        }
        self.token_reveal = None;
    }

    pub fn refresh_dynamic(&self) {
        use slint::{ModelRc, VecModel};
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let view_model = ViewModel::from_snapshot((self.actions.snapshot)());
        window.set_models(ModelRc::from(Rc::new(VecModel::from(display_rows(
            &view_model,
            window.get_search_query().as_str(),
        )))));
        window.set_service_status(format!("{:?}", view_model.snapshot.lifecycle.state).into());
        hydrate_server(window, &view_model.snapshot);
        let filter = *self
            .current_log_filter
            .read()
            .expect("log filter lock poisoned");
        set_logs(window, (self.actions.logs)(filter));
    }

    pub fn hydrate_settings_and_capabilities(&self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let snapshot = (self.actions.snapshot)();
        hydrate_capabilities(window, &snapshot.capabilities);
        hydrate_engine_settings(window, (self.actions.engine_settings)());
    }

    #[must_use]
    pub fn last_window_destroyed(&self) -> bool {
        self.last_closed.upgrade().is_none()
    }
}

fn create_window(
    snapshot: AppSnapshot,
    address: String,
    actions: UiActions,
    current_log_filter: Arc<std::sync::RwLock<LogFilter>>,
) -> Result<MainWindow, slint::PlatformError> {
    use slint::{ComponentHandle as _, ModelRc, VecModel};
    let view_model = ViewModel::from_snapshot(snapshot);
    let rows = display_rows(&view_model, "");
    let window = MainWindow::new()?;
    window.set_models(ModelRc::from(Rc::new(VecModel::from(rows))));
    window.set_base_url(format!("http://{address}").into());
    hydrate_server(&window, &view_model.snapshot);
    window.set_service_status(format!("{:?}", view_model.snapshot.lifecycle.state).into());
    hydrate_capabilities(&window, &view_model.snapshot.capabilities);
    set_logs(&window, (actions.logs)(LogFilter::default()));
    hydrate_engine_settings(&window, (actions.engine_settings)());
    install_load_callback(&window, actions.clone());
    install_prepare_load(&window, actions.clone());
    install_model_search(&window, actions.clone());
    window.on_eject_model({
        let actions = actions.clone();
        move |_| (actions.eject)()
    });
    window.on_rescan({
        let actions = actions.clone();
        move || (actions.rescan)()
    });
    window.on_save_settings({
        let actions = actions.clone();
        move |model_directory,
              distribution,
              executable,
              context,
              gpu,
              threads,
              batch,
              parallel,
              flash,
              kv| {
            (actions.save_settings)(EngineSettings {
                model_directory: model_directory.into(),
                distribution: distribution.into(),
                executable: executable.into(),
                defaults: launch_settings(context, gpu, threads, batch, parallel, flash, &kv),
            })
        }
    });
    window.on_export_logs({
        let actions = actions.clone();
        move || (actions.export_logs)()
    });
    window.on_generate_token({
        let actions = actions.clone();
        move || (actions.generate_token)()
    });
    window.on_dismiss_token({
        let weak = window.as_weak();
        move || {
            if let Some(window) = weak.upgrade() {
                window.set_generated_token("".into());
            }
            clear_token_reveal();
        }
    });
    #[cfg(windows)]
    window.on_confirm_close_to_tray(|| {
        let _ = slint::invoke_from_event_loop(|| {
            DESKTOP.with_borrow_mut(|desktop| {
                if let Some(desktop) = desktop.as_mut() {
                    let _ = desktop.windows.close();
                }
            })
        });
    });
    #[cfg(not(windows))]
    window.on_confirm_close_to_tray(|| {});
    window.on_copy_text(|text| {
        use copypasta::{ClipboardContext, ClipboardProvider as _};
        if let Ok(mut clipboard) = ClipboardContext::new() {
            let _ = clipboard.set_contents(text.into());
        }
    });
    install_log_filter(&window, actions.clone(), current_log_filter);
    Ok(window)
}

fn display_rows(view_model: &ViewModel, query: &str) -> Vec<ModelDisplay> {
    view_model
        .filtered_rows(query)
        .into_iter()
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
        .collect()
}

fn install_model_search(window: &MainWindow, actions: UiActions) {
    use slint::{ComponentHandle as _, ModelRc, VecModel};
    let weak = window.as_weak();
    window.on_search_models(move |query| {
        let view_model = ViewModel::from_snapshot((actions.snapshot)());
        if let Some(window) = weak.upgrade() {
            window.set_models(ModelRc::from(Rc::new(VecModel::from(display_rows(
                &view_model,
                query.as_str(),
            )))));
        }
    });
}

fn install_prepare_load(window: &MainWindow, actions: UiActions) {
    use slint::ComponentHandle as _;
    let weak = window.as_weak();
    window.on_prepare_load(move |raw| {
        let Ok(uuid) = uuid::Uuid::parse_str(&raw) else {
            return;
        };
        let snapshot = (actions.snapshot)();
        let id = ModelId::from_uuid(uuid);
        let Some(values) = load_dialog_values_for(&snapshot, id) else {
            return;
        };
        let Some(fields) = load_dialog_fields_for(&snapshot, id) else {
            return;
        };
        let Some(window) = weak.upgrade() else {
            return;
        };
        let settings = values.settings;
        window.set_selected_model_id(raw);
        window.set_selected_model_key(values.key.into());
        window
            .set_selected_context(settings.context_length.map_or(4096, |value| value.get()) as i32);
        window.set_selected_gpu(settings.gpu_layers.map_or(0, |value| value.get()) as i32);
        window.set_selected_threads(settings.cpu_threads.map_or(1, |value| value.get()) as i32);
        window.set_selected_batch(settings.batch_size.map_or(512, |value| value.get()) as i32);
        window.set_selected_parallel(settings.parallel_slots.map_or(1, |value| value.get()) as i32);
        window.set_selected_flash(settings.flash_attention.unwrap_or(false));
        window.set_selected_kv(
            match settings
                .kv_cache_type
                .unwrap_or(model_launcher_core::KvCacheType::F16)
            {
                model_launcher_core::KvCacheType::F16 => "f16",
                model_launcher_core::KvCacheType::Q8_0 => "q8_0",
                model_launcher_core::KvCacheType::Q4_0 => "q4_0",
            }
            .into(),
        );
        for field in fields {
            let (visible, unsupported) = (field.visible, field.retained_unsupported);
            match field.id {
                "context_length" => {
                    window.set_selected_show_context(visible);
                    window.set_selected_unsupported_context(unsupported);
                    window.set_selected_clear_context(false);
                }
                "gpu_layers" => {
                    window.set_selected_show_gpu(visible);
                    window.set_selected_unsupported_gpu(unsupported);
                    window.set_selected_clear_gpu(false);
                }
                "cpu_threads" => {
                    window.set_selected_show_threads(visible);
                    window.set_selected_unsupported_threads(unsupported);
                    window.set_selected_clear_threads(false);
                }
                "batch_size" => {
                    window.set_selected_show_batch(visible);
                    window.set_selected_unsupported_batch(unsupported);
                    window.set_selected_clear_batch(false);
                }
                "parallel_slots" => {
                    window.set_selected_show_parallel(visible);
                    window.set_selected_unsupported_parallel(unsupported);
                    window.set_selected_clear_parallel(false);
                }
                "flash_attention" => {
                    window.set_selected_show_flash(visible);
                    window.set_selected_unsupported_flash(unsupported);
                    window.set_selected_clear_flash(false);
                }
                "kv_cache_type" => {
                    window.set_selected_show_kv(visible);
                    window.set_selected_unsupported_kv(unsupported);
                    window.set_selected_clear_kv(false);
                }
                _ => {}
            }
        }
        window.set_load_dialog_open(true);
    });
}

fn log_lines(records: &[LogRecord]) -> slint::ModelRc<slint::SharedString> {
    use slint::{ModelRc, SharedString, VecModel};
    let lines: Vec<SharedString> = records
        .iter()
        .map(|record| {
            format!(
                "{} {:?} {:?}  {}",
                record.timestamp_ms, record.source, record.level, record.message
            )
            .into()
        })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(lines)))
}

fn log_text(records: &[LogRecord]) -> String {
    records
        .iter()
        .map(|record| {
            format!(
                "{} {:?} {:?}  {}",
                record.timestamp_ms, record.source, record.level, record.message
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn set_logs(window: &MainWindow, records: Vec<LogRecord>) {
    window.set_log_copy_text(log_text(&records).into());
    window.set_logs(log_lines(&records));
}

fn install_log_filter(
    window: &MainWindow,
    actions: UiActions,
    current_filter: Arc<std::sync::RwLock<LogFilter>>,
) {
    use slint::ComponentHandle as _;
    let weak = window.as_weak();
    window.on_filter_logs(move |source, level| {
        let source = match source.as_str() {
            "Application" => Some(model_launcher_core::LogSource::Application),
            "Engine stdout" => Some(model_launcher_core::LogSource::EngineStdout),
            "Engine stderr" => Some(model_launcher_core::LogSource::EngineStderr),
            _ => None,
        };
        let minimum_level = match level.as_str() {
            "Trace" => Some(model_launcher_core::LogLevel::Trace),
            "Debug" => Some(model_launcher_core::LogLevel::Debug),
            "Info" => Some(model_launcher_core::LogLevel::Info),
            "Warn" => Some(model_launcher_core::LogLevel::Warn),
            "Error" => Some(model_launcher_core::LogLevel::Error),
            _ => None,
        };
        let filter = LogFilter {
            source,
            minimum_level,
        };
        *current_filter.write().expect("log filter lock poisoned") = filter;
        if let Some(window) = weak.upgrade() {
            set_logs(&window, (actions.logs)(filter));
        }
    });
}

fn hydrate_engine_settings(window: &MainWindow, settings: EngineSettings) {
    window.set_model_directory(settings.model_directory.into());
    window.set_engine_distribution(settings.distribution.into());
    window.set_engine_executable(settings.executable.into());
    window.set_default_context(
        settings
            .defaults
            .context_length
            .map_or(4096, |value| value.get()) as i32,
    );
    window.set_default_gpu(settings.defaults.gpu_layers.map_or(0, |value| value.get()) as i32);
    window.set_default_threads(settings.defaults.cpu_threads.map_or(0, |value| value.get()) as i32);
    window.set_default_batch(
        settings
            .defaults
            .batch_size
            .map_or(512, |value| value.get()) as i32,
    );
    window.set_default_parallel(
        settings
            .defaults
            .parallel_slots
            .map_or(1, |value| value.get()) as i32,
    );
    window.set_default_flash(settings.defaults.flash_attention.unwrap_or(false));
    window.set_default_kv(
        match settings
            .defaults
            .kv_cache_type
            .unwrap_or(model_launcher_core::KvCacheType::F16)
        {
            model_launcher_core::KvCacheType::F16 => "f16",
            model_launcher_core::KvCacheType::Q8_0 => "q8_0",
            model_launcher_core::KvCacheType::Q4_0 => "q4_0",
        }
        .into(),
    );
}

fn hydrate_server(window: &MainWindow, snapshot: &AppSnapshot) {
    window.set_authentication_status(snapshot.authentication_status.clone().into());
    window.set_server_warning(snapshot.server_warning.clone().into());
    window.set_engine_diagnostic(
        snapshot
            .engine_diagnostic
            .clone()
            .unwrap_or_default()
            .into(),
    );
}

fn launch_settings(
    context: i32,
    gpu: i32,
    threads: i32,
    batch: i32,
    parallel: i32,
    flash: bool,
    kv: &str,
) -> LaunchSettings {
    LaunchSettings {
        context_length: model_launcher_core::ContextLength::new(context.max(1) as u32).ok(),
        gpu_layers: Some(model_launcher_core::GpuLayers::new(gpu.max(0) as u32)),
        cpu_threads: model_launcher_core::CpuThreads::new(threads.max(0) as u32).ok(),
        batch_size: model_launcher_core::BatchSize::new(batch.max(1) as u32).ok(),
        parallel_slots: model_launcher_core::ParallelSlots::new(parallel.max(1) as u32).ok(),
        flash_attention: Some(flash),
        kv_cache_type: Some(match kv {
            "q8_0" => model_launcher_core::KvCacheType::Q8_0,
            "q4_0" => model_launcher_core::KvCacheType::Q4_0,
            _ => model_launcher_core::KvCacheType::F16,
        }),
    }
}

fn hydrate_capabilities(window: &MainWindow, caps: &EngineCapabilities) {
    window.set_cap_context(caps.context_length);
    window.set_cap_gpu(caps.gpu_layers);
    window.set_cap_threads(caps.cpu_threads);
    window.set_cap_batch(caps.batch_size);
    window.set_cap_parallel(caps.parallel_slots);
    window.set_cap_flash(caps.flash_attention);
    window.set_cap_kv(caps.kv_cache_type);
}

fn install_load_callback(window: &MainWindow, actions: UiActions) {
    window.on_load_model(
        move |raw,
              key,
              context,
              gpu,
              threads,
              batch,
              parallel,
              flash,
              kv,
              clear_context,
              clear_gpu,
              clear_threads,
              clear_batch,
              clear_parallel,
              clear_flash,
              clear_kv| {
            let Ok(uuid) = uuid::Uuid::parse_str(&raw) else {
                return;
            };
            let settings = LaunchSettings {
                context_length: (!clear_context)
                    .then(|| model_launcher_core::ContextLength::new(context as u32).ok())
                    .flatten(),
                gpu_layers: (!clear_gpu).then(|| model_launcher_core::GpuLayers::new(gpu as u32)),
                cpu_threads: (!clear_threads)
                    .then(|| model_launcher_core::CpuThreads::new(threads as u32).ok())
                    .flatten(),
                batch_size: (!clear_batch)
                    .then(|| model_launcher_core::BatchSize::new(batch as u32).ok())
                    .flatten(),
                parallel_slots: (!clear_parallel)
                    .then(|| model_launcher_core::ParallelSlots::new(parallel as u32).ok())
                    .flatten(),
                flash_attention: (!clear_flash).then_some(flash),
                kv_cache_type: (!clear_kv).then(|| match kv.as_str() {
                    "q8_0" => model_launcher_core::KvCacheType::Q8_0,
                    "q4_0" => model_launcher_core::KvCacheType::Q4_0,
                    _ => model_launcher_core::KvCacheType::F16,
                }),
            };
            (actions.load)(UiLoadRequest {
                id: ModelId::from_uuid(uuid),
                key: key.into(),
                settings,
            });
        },
    );
}

#[cfg(windows)]
thread_local! {
    static DESKTOP: std::cell::RefCell<Option<WindowsDesktop>> = const { std::cell::RefCell::new(None) };
}

#[cfg(windows)]
struct WindowsDesktop {
    windows: WindowManager,
    tray: tray::NativeTray,
}

#[cfg(windows)]
pub fn request_refresh() {
    let _ = slint::invoke_from_event_loop(|| {
        DESKTOP.with_borrow(|desktop| {
            if let Some(desktop) = desktop.as_ref() {
                desktop.windows.refresh_dynamic();
                let snapshot = (desktop.windows.actions.snapshot)();
                let active = snapshot
                    .lifecycle
                    .desired_model
                    .and_then(|id| snapshot.models.iter().find(|model| model.id == id))
                    .map(|model| model.display_name.as_str());
                let recent: Vec<_> = snapshot
                    .recent_models
                    .iter()
                    .map(|model| (model.id, model.display_name.clone()))
                    .collect();
                desktop
                    .tray
                    .update(&format!("{:?}", snapshot.lifecycle.state), active, &recent);
            }
        });
    });
}

#[cfg(windows)]
pub fn report_status(message: impl Into<String>) {
    let message = message.into();
    let _ = slint::invoke_from_event_loop(move || {
        DESKTOP.with_borrow(|desktop| {
            if let Some(window) = desktop
                .as_ref()
                .and_then(|desktop| desktop.windows.window.as_ref())
            {
                window.set_operation_status(message.into());
            }
        });
    });
}

#[cfg(windows)]
pub fn report_settings_saved() {
    let _ = slint::invoke_from_event_loop(|| {
        DESKTOP.with_borrow(|desktop| {
            if let Some(desktop) = desktop.as_ref() {
                desktop.windows.hydrate_settings_and_capabilities();
                if let Some(window) = desktop.windows.window.as_ref() {
                    window.set_operation_status("Settings saved".into());
                }
            }
        });
    });
}

#[cfg(windows)]
pub fn report_generated_token(token: String) {
    let _ = slint::invoke_from_event_loop(move || {
        DESKTOP.with_borrow_mut(|desktop| {
            if let Some(desktop) = desktop.as_mut() {
                desktop.windows.reveal_token(token);
            }
        });
    });
}

#[cfg(windows)]
fn clear_token_reveal() {
    DESKTOP.with_borrow_mut(|desktop| {
        if let Some(desktop) = desktop.as_mut() {
            desktop.windows.clear_token();
        }
    });
}

#[cfg(not(windows))]
pub fn request_refresh() {}

#[cfg(not(windows))]
pub fn report_status(_message: impl Into<String>) {}

#[cfg(not(windows))]
pub fn report_settings_saved() {}

#[cfg(not(windows))]
pub fn report_generated_token(_token: String) {}

#[cfg(not(windows))]
fn clear_token_reveal() {}

#[cfg(windows)]
fn dispatch_windows(command: TrayCommand) {
    DESKTOP.with_borrow_mut(|desktop| {
        let Some(desktop) = desktop.as_mut() else {
            return;
        };
        match command {
            TrayCommand::Open => {
                let _ = desktop.windows.open();
            }
            TrayCommand::Eject => (desktop.windows.actions.eject)(),
            TrayCommand::LoadRecent(id) => {
                if let Some(request) = load_request_for(&(desktop.windows.actions.snapshot)(), id) {
                    (desktop.windows.actions.load)(request);
                }
            }
            TrayCommand::Quit => (desktop.windows.actions.quit)(),
        }
    });
}

#[cfg(windows)]
pub fn run_desktop(
    snapshot: AppSnapshot,
    address: String,
    actions: UiActions,
) -> Result<(), slint::PlatformError> {
    let active = snapshot
        .lifecycle
        .desired_model
        .and_then(|id| snapshot.models.iter().find(|model| model.id == id))
        .map(|model| model.display_name.as_str());
    let recent: Vec<(ModelId, String)> = snapshot
        .recent_models
        .iter()
        .take(8)
        .map(|model| (model.id, model.display_name.clone()))
        .collect();
    let dispatch: Arc<dyn Fn(TrayCommand) + Send + Sync> = Arc::new(dispatch_windows);
    let tray = tray::NativeTray::new(
        &format!("{:?}", snapshot.lifecycle.state),
        active,
        &recent,
        dispatch,
    )
    .map_err(|error| slint::PlatformError::Other(error.to_string()))?;
    let mut windows = WindowManager::new(address, actions);
    windows.open()?;
    DESKTOP.set(Some(WindowsDesktop { windows, tray }));
    let result = slint::run_event_loop_until_quit();
    DESKTOP.set(None);
    result
}

#[derive(Clone, Debug)]
pub struct AppSnapshot {
    pub models: Vec<ModelRecord>,
    pub recent_models: Vec<ModelRecord>,
    pub lifecycle: LifecycleSnapshot,
    pub capabilities: EngineCapabilities,
    pub authentication_status: String,
    pub server_warning: String,
    pub engine_valid: bool,
    pub engine_diagnostic: Option<String>,
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
    pub fn filtered_rows(&self, query: &str) -> Vec<&ModelRow> {
        let query = query.trim().to_lowercase();
        self.rows
            .iter()
            .filter(|row| {
                query.is_empty()
                    || row.name.to_lowercase().contains(&query)
                    || row.key.to_lowercase().contains(&query)
                    || row.path.to_lowercase().contains(&query)
            })
            .collect()
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
        if !self.snapshot.engine_valid {
            return ModelAction::Disabled(
                self.snapshot
                    .engine_diagnostic
                    .clone()
                    .unwrap_or_else(|| "Engine settings must be validated before loading.".into()),
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

fn field(id: &'static str, supported: bool, retained_unsupported: bool) -> SettingField {
    SettingField {
        id,
        visible: supported || retained_unsupported,
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

pub struct CloseNoticeStore {
    path: std::path::PathBuf,
}
impl CloseNoticeStore {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
    #[must_use]
    pub fn load(&self) -> CloseNotice {
        CloseNotice::from_persisted(self.path.exists())
    }
    pub fn save(&self, notice: &CloseNotice) -> io::Result<()> {
        if notice.was_shown() {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&self.path, b"shown")?;
        }
        Ok(())
    }
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
    #[must_use]
    pub fn expose(&self) -> Option<&str> {
        self.0.as_deref()
    }
    pub fn clear(&mut self) {
        if let Some(value) = self.0.as_mut() {
            unsafe {
                value.as_bytes_mut().fill(0);
            }
        }
        self.0 = None;
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
        self.clear();
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineSettings {
    pub distribution: String,
    pub executable: String,
    pub model_directory: String,
    pub defaults: LaunchSettings,
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
    #[must_use]
    pub fn copy_text(&self, filter: LogFilter) -> String {
        self.snapshot(filter)
            .into_iter()
            .map(|record| {
                format!(
                    "{} {:?} {:?}  {}",
                    record.timestamp_ms, record.source, record.level, record.message
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
