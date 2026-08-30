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
    AppSnapshot, CloseNotice, CloseNoticeStore, EngineSettings, LogCommands, MetadataVisibility,
    ModelAction, RecentModels, SaveSettings, TokenReveal, TrayCommand, TrayController, ViewModel,
    load_dialog_fields_for, load_dialog_values_for, load_request_for,
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
        recent_models: vec![],
        lifecycle: LifecycleSnapshot::default(),
        capabilities: EngineCapabilities::default(),
        authentication_status: String::new(),
        server_warning: String::new(),
        engine_valid: true,
        engine_diagnostic: None,
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
fn model_search_matches_name_key_and_path_case_insensitively() {
    let mut alpha = model("Alpha Chat", 1);
    alpha.key = ModelKey::parse("team/assistant").unwrap();
    alpha.path = PathBuf::from("/models/Research/alpha.gguf");
    let beta = model("Beta", 1);
    let vm = ViewModel::from_snapshot(AppSnapshot {
        models: vec![alpha.clone(), beta],
        recent_models: vec![],
        lifecycle: LifecycleSnapshot::default(),
        capabilities: EngineCapabilities::default(),
        authentication_status: String::new(),
        server_warning: String::new(),
        engine_valid: true,
        engine_diagnostic: None,
    });

    assert_eq!(vm.filtered_rows("CHAT")[0].id, alpha.id);
    assert_eq!(vm.filtered_rows("assistant")[0].id, alpha.id);
    assert_eq!(vm.filtered_rows("research")[0].id, alpha.id);
    assert!(vm.filtered_rows("missing").is_empty());
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
        recent_models: vec![],
        lifecycle,
        capabilities: EngineCapabilities::default(),
        authentication_status: String::new(),
        server_warning: String::new(),
        engine_valid: true,
        engine_diagnostic: None,
    });
    assert_eq!(
        vm.action(other.id),
        ModelAction::Disabled("Finish active requests or eject the current model first.".into())
    );
    assert_eq!(vm.action(active.id), ModelAction::Eject);
}

#[test]
fn invalid_engine_disables_load_with_probe_diagnostic() {
    let alpha = model("Alpha", 1);
    let vm = ViewModel::from_snapshot(AppSnapshot {
        models: vec![alpha.clone()],
        recent_models: vec![],
        lifecycle: LifecycleSnapshot::default(),
        capabilities: EngineCapabilities::default(),
        authentication_status: String::new(),
        server_warning: String::new(),
        engine_valid: false,
        engine_diagnostic: Some("safe probe diagnostic".into()),
    });
    assert_eq!(
        vm.action(alpha.id),
        ModelAction::Disabled("safe probe diagnostic".into())
    );
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
            .any(|field| field.id == "gpu_layers" && field.visible && field.retained_unsupported)
    );
    assert_eq!(settings.gpu_layers.unwrap().get(), 20);
}

#[test]
fn close_notice_and_plaintext_token_are_each_consumed_once() {
    let mut notice = CloseNotice::default();
    assert!(notice.take().is_some());
    assert!(notice.take().is_none());
    let mut token = TokenReveal::new("secret-token");
    assert_eq!(token.expose(), Some("secret-token"));
    token.clear();
    assert_eq!(token.expose(), None);
    let mut token = TokenReveal::new("secret-token");
    assert_eq!(token.take().as_deref(), Some("secret-token"));
    assert!(token.take().is_none());
    assert!(!format!("{token:?}").contains("secret-token"));
}

#[test]
fn close_notice_consumption_is_persisted() {
    let directory = tempfile::tempdir().unwrap();
    let store = CloseNoticeStore::new(directory.path().join("notice"));
    let mut notice = store.load();
    assert!(notice.take().is_some());
    store.save(&notice).unwrap();
    assert!(store.load().take().is_none());
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
    let copied = logs.copy_text(LogFilter {
        source: None,
        minimum_level: Some(LogLevel::Warn),
    });
    assert!(copied.contains("boom"));
    assert!(!copied.contains("old"));
    assert!(!copied.contains("secret"));
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
    let recent_id = ModelId::new();
    assert_eq!(
        tray.map_command(&format!("recent:{}", recent_id.as_uuid())),
        Some(TrayCommand::LoadRecent(recent_id))
    );
    assert_eq!(opened.load(Ordering::SeqCst), 0);
    for _ in 0..50 {
        tray.open_for_test();
        assert!(tray.has_window());
        tray.close_for_test();
        assert!(!tray.has_window());
    }
    assert_eq!(tray.live_window_count(), 0);
}

#[test]
fn tray_recent_request_resolves_stable_id_after_catalog_reordering() {
    let alpha = model("Alpha", 1);
    let beta = model("Beta", 1);
    let snapshot = AppSnapshot {
        models: vec![beta, alpha.clone()],
        recent_models: vec![alpha.clone()],
        lifecycle: LifecycleSnapshot::default(),
        capabilities: EngineCapabilities::default(),
        authentication_status: String::new(),
        server_warning: String::new(),
        engine_valid: true,
        engine_diagnostic: None,
    };

    let request = load_request_for(&snapshot, alpha.id).unwrap();
    assert_eq!(request.id, alpha.id);
    assert_eq!(request.key, alpha.key.to_string());
}

#[test]
fn load_dialog_adapter_hydrates_every_saved_profile_value() {
    let mut alpha = model("Alpha", 1);
    alpha.launch_profile.settings = LaunchSettings {
        context_length: Some(model_launcher_core::ContextLength::new(8192).unwrap()),
        gpu_layers: Some(model_launcher_core::GpuLayers::new(31)),
        cpu_threads: Some(model_launcher_core::CpuThreads::new(9).unwrap()),
        batch_size: Some(model_launcher_core::BatchSize::new(777).unwrap()),
        parallel_slots: Some(model_launcher_core::ParallelSlots::new(3).unwrap()),
        flash_attention: Some(true),
        kv_cache_type: Some(model_launcher_core::KvCacheType::Q8_0),
    };
    let snapshot = AppSnapshot {
        models: vec![alpha.clone()],
        recent_models: vec![],
        lifecycle: LifecycleSnapshot::default(),
        capabilities: EngineCapabilities::default(),
        authentication_status: String::new(),
        server_warning: String::new(),
        engine_valid: true,
        engine_diagnostic: None,
    };

    let values = load_dialog_values_for(&snapshot, alpha.id).unwrap();
    assert_eq!(values.key, alpha.key.to_string());
    assert_eq!(values.settings, alpha.launch_profile.settings);
}

#[test]
fn load_dialog_adapter_keeps_unsupported_saved_fields_visible_and_read_only() {
    let mut alpha = model("Alpha", 1);
    alpha.launch_profile.settings.gpu_layers = Some(model_launcher_core::GpuLayers::new(23));
    let snapshot = AppSnapshot {
        models: vec![alpha.clone()],
        recent_models: vec![],
        lifecycle: LifecycleSnapshot::default(),
        capabilities: EngineCapabilities {
            context_length: true,
            ..EngineCapabilities::default()
        },
        authentication_status: String::new(),
        server_warning: String::new(),
        engine_valid: true,
        engine_diagnostic: None,
    };

    let gpu = load_dialog_fields_for(&snapshot, alpha.id)
        .unwrap()
        .into_iter()
        .find(|field| field.id == "gpu_layers")
        .unwrap();
    assert!(gpu.visible);
    assert!(gpu.retained_unsupported);
    assert_eq!(
        alpha
            .launch_profile
            .settings
            .render(&snapshot.capabilities)
            .unsupported,
        vec![model_launcher_core::SettingId::GpuLayers]
    );
}
