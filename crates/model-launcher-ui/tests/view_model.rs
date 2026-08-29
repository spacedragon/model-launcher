use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use model_launcher_core::{
    CatalogIdentity, EngineCapabilities, LaunchProfile, LaunchSettings, LifecycleSnapshot,
    LifecycleState, LogFilter, LogLevel, LogRecord, LogSource, LogStore, LogStoreLimits, ModelId,
    ModelKey, ModelRecord, ModelState,
};
use model_launcher_ui::{
    AppSnapshot, CloseNotice, EngineSettings, LogCommands, MetadataVisibility, ModelAction,
    RecentModels, SaveSettings, TokenReveal, TrayCommand, TrayController, ViewModel,
};

fn model(name: &str, size: u64) -> ModelRecord {
    ModelRecord {
        id: ModelId::new(),
        key: ModelKey::parse(name.to_ascii_lowercase().replace(' ', "-")).unwrap(),
        display_name: name.into(),
        path: PathBuf::from(format!("/models/{name}.gguf")),
        file_identity: CatalogIdentity::default(),
        size_bytes: size,
        state: ModelState::Available,
        launch_profile: LaunchProfile::default(),
    }
}

#[test]
fn snapshot_maps_to_compact_rows_and_prioritizes_narrow_metadata() {
    let alpha = model("Alpha", 2_147_483_648);
    let snapshot = AppSnapshot {
        models: vec![alpha.clone()],
        lifecycle: LifecycleSnapshot::default(),
        capabilities: EngineCapabilities::default(),
    };
    let vm = ViewModel::from_snapshot(snapshot);
    assert_eq!(vm.rows()[0].name, "Alpha");
    assert_eq!(vm.rows()[0].size, "2.0 GB");
    assert_eq!(
        vm.metadata_visibility(900),
        MetadataVisibility {
            size: true,
            path: true
        }
    );
    assert_eq!(
        vm.metadata_visibility(560),
        MetadataVisibility {
            size: true,
            path: false
        }
    );
    assert_eq!(
        vm.metadata_visibility(420),
        MetadataVisibility {
            size: false,
            path: false
        }
    );
}

#[test]
fn busy_disables_other_load_with_explanation_but_keeps_eject_enabled() {
    let active = model("Active", 1);
    let other = model("Other", 1);
    let lifecycle = LifecycleSnapshot {
        state: LifecycleState::Running,
        desired_model: Some(active.id),
        in_flight: 2,
        ..LifecycleSnapshot::default()
    };
    let vm = ViewModel::from_snapshot(AppSnapshot {
        models: vec![active.clone(), other.clone()],
        lifecycle,
        capabilities: EngineCapabilities::default(),
    });
    assert_eq!(
        vm.action(other.id),
        ModelAction::Disabled("Finish active requests or eject the current model first.".into())
    );
    assert_eq!(vm.action(active.id), ModelAction::Eject);
}

#[test]
fn capability_visibility_retains_and_reports_unsupported_values() {
    let settings = LaunchSettings {
        gpu_layers: Some(model_launcher_core::GpuLayers::new(20)),
        ..Default::default()
    };
    let caps = EngineCapabilities {
        context_length: true,
        ..Default::default()
    };
    let fields = ViewModel::setting_fields(&settings, &caps);
    assert!(
        fields
            .iter()
            .any(|field| field.id == "context_length" && field.visible)
    );
    assert!(
        fields
            .iter()
            .any(|field| field.id == "gpu_layers" && !field.visible && field.retained_unsupported)
    );
    assert_eq!(settings.gpu_layers.unwrap().get(), 20);
}

#[test]
fn close_notice_and_plaintext_token_are_each_consumed_once() {
    let mut notice = CloseNotice::default();
    assert!(notice.take().is_some());
    assert!(notice.take().is_none());
    let mut token = TokenReveal::new("secret-token");
    assert_eq!(token.take().as_deref(), Some("secret-token"));
    assert!(token.take().is_none());
    assert!(!format!("{token:?}").contains("secret-token"));
}

#[test]
fn save_engine_settings_validates_identity_then_reprobes() {
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    let save = SaveSettings::new(
        {
            let order = order.clone();
            move |_| {
                order.lock().unwrap().push("validate");
                Ok(())
            }
        },
        {
            let order = order.clone();
            move || {
                order.lock().unwrap().push("probe");
                Ok(EngineCapabilities::default())
            }
        },
    );
    save.run(&EngineSettings::default()).unwrap();
    assert_eq!(*order.lock().unwrap(), ["validate", "probe"]);
}

#[test]
fn recent_models_are_most_recent_first_and_bounded() {
    let mut recent = RecentModels::new(2);
    let a = model("A", 1);
    let b = model("B", 1);
    let c = model("C", 1);
    recent.record(a.id);
    recent.record(b.id);
    recent.record(a.id);
    recent.record(c.id);
    assert_eq!(recent.ids(), &[c.id, a.id]);
}

#[test]
fn log_commands_use_bounded_filtered_redacted_snapshots() {
    let store = LogStore::new(LogStoreLimits::new(2, 4096, 4)).unwrap();
    for (level, message) in [
        (LogLevel::Info, "old"),
        (LogLevel::Warn, "Authorization: Bearer secret"),
        (LogLevel::Error, "boom"),
    ] {
        store.append(LogRecord {
            timestamp_ms: 1,
            source: LogSource::Application,
            level,
            generation: None,
            model_id: None,
            message: message.into(),
            truncated: false,
        });
    }
    let logs = LogCommands::new(store);
    assert_eq!(
        logs.snapshot(LogFilter {
            source: None,
            minimum_level: Some(LogLevel::Warn)
        })
        .len(),
        2
    );
    let mut bytes = Vec::new();
    logs.export(&mut bytes).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(!text.contains("secret"));
    assert!(!text.contains("old"));
}

#[test]
fn tray_maps_commands_without_opening_a_real_window_and_drops_windows() {
    let opened = Arc::new(AtomicUsize::new(0));
    let mut tray = TrayController::new({
        let opened = opened.clone();
        move || {
            opened.fetch_add(1, Ordering::SeqCst);
        }
    });
    assert_eq!(tray.map_command("open"), Some(TrayCommand::Open));
    assert_eq!(opened.load(Ordering::SeqCst), 0);
    for _ in 0..50 {
        tray.open_for_test();
        assert!(tray.has_window());
        tray.close_for_test();
        assert!(!tray.has_window());
    }
    assert_eq!(tray.live_window_count(), 0);
}
