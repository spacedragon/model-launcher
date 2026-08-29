#![cfg(windows)]

use model_launcher_core::{EngineCapabilities, LifecycleSnapshot, LogFilter, LogLevel};
use model_launcher_ui::{AppSnapshot, EngineSettings, UiActions, WindowManager};
use std::sync::Arc;

/// Requires an interactive Windows desktop. Slint 1.16.1's published testing backend omits its
/// font resources, so this cannot use the headless backend (see tests/README.md).
#[test]
#[ignore = "requires an interactive Windows desktop"]
fn real_main_window_weak_reference_dies_for_fifty_recreate_cycles() {
    let snapshot = AppSnapshot {
        models: vec![],
        recent_models: vec![],
        lifecycle: LifecycleSnapshot::default(),
        capabilities: EngineCapabilities::default(),
        authentication_status: String::new(),
        server_warning: String::new(),
        engine_valid: true,
        engine_diagnostic: None,
    };
    let actions = UiActions {
        load: Arc::new(|_| {}),
        eject: Arc::new(|| {}),
        rescan: Arc::new(|| {}),
        snapshot: Arc::new(move || snapshot.clone()),
        quit: Arc::new(|| {}),
        close_notice: Arc::new(|| None),
        save_settings: Arc::new(|_| {}),
        logs: Arc::new(|_: LogFilter| vec![]),
        export_logs: Arc::new(|| {}),
        generate_token: Arc::new(|| {}),
        engine_settings: Arc::new(EngineSettings::default),
    };
    let mut windows = WindowManager::new("127.0.0.1:1234".into(), actions);
    for _ in 0..50 {
        let weak = windows.open().unwrap();
        assert!(weak.upgrade().is_some());
        windows.close().unwrap();
        assert!(weak.upgrade().is_none());
        assert!(windows.last_window_destroyed());
    }
}

#[test]
#[ignore = "requires an interactive Windows desktop"]
fn dynamic_refresh_preserves_unsaved_settings_and_current_log_filter() {
    let snapshot = AppSnapshot {
        models: vec![],
        recent_models: vec![],
        lifecycle: LifecycleSnapshot::default(),
        capabilities: EngineCapabilities::default(),
        authentication_status: String::new(),
        server_warning: String::new(),
        engine_valid: true,
        engine_diagnostic: None,
    };
    let observed_filter = Arc::new(std::sync::Mutex::new(LogFilter::default()));
    let actions = UiActions {
        load: Arc::new(|_| {}),
        eject: Arc::new(|| {}),
        rescan: Arc::new(|| {}),
        snapshot: Arc::new(move || snapshot.clone()),
        quit: Arc::new(|| {}),
        close_notice: Arc::new(|| None),
        save_settings: Arc::new(|_| {}),
        logs: Arc::new({
            let observed_filter = observed_filter.clone();
            move |filter| {
                *observed_filter.lock().unwrap() = filter;
                vec![]
            }
        }),
        export_logs: Arc::new(|| {}),
        generate_token: Arc::new(|| {}),
        engine_settings: Arc::new(EngineSettings::default),
    };
    let mut windows = WindowManager::new("127.0.0.1:1234".into(), actions);
    let weak = windows.open().unwrap();
    let window = weak.upgrade().unwrap();
    window.set_model_directory("unsaved/directory".into());
    window.set_engine_executable("unsaved executable".into());
    window.invoke_filter_logs("All sources".into(), "Error".into());

    windows.refresh_dynamic();

    assert_eq!(window.get_model_directory(), "unsaved/directory");
    assert_eq!(window.get_engine_executable(), "unsaved executable");
    assert_eq!(
        observed_filter.lock().unwrap().minimum_level,
        Some(LogLevel::Error)
    );
}
