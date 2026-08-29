#![cfg(windows)]

use model_launcher_core::{EngineCapabilities, LifecycleSnapshot, LogFilter};
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
