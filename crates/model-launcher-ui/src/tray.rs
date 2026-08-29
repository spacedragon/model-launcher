use model_launcher_core::ModelId;
use std::sync::{Arc, Weak};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    Open,
    Eject,
    LoadRecent(ModelId),
    Quit,
}

#[derive(Default)]
struct WindowMarker;

/// Owns only an application callback and tray state while the main window is closed.
pub struct TrayController {
    on_open: Arc<dyn Fn() + Send + Sync>,
    window: Option<Arc<WindowMarker>>,
    last_closed: Weak<WindowMarker>,
}

impl TrayController {
    pub fn new(on_open: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            on_open: Arc::new(on_open),
            window: None,
            last_closed: Weak::new(),
        }
    }
    #[must_use]
    pub fn map_command(&self, command: &str) -> Option<TrayCommand> {
        match command {
            "open" => Some(TrayCommand::Open),
            "eject" => Some(TrayCommand::Eject),
            _ if command.starts_with("recent:") => {
                uuid::Uuid::parse_str(command.trim_start_matches("recent:"))
                    .ok()
                    .map(ModelId::from_uuid)
                    .map(TrayCommand::LoadRecent)
            }
            "quit" => Some(TrayCommand::Quit),
            _ => None,
        }
    }
    pub fn dispatch(&self, command: TrayCommand) {
        if command == TrayCommand::Open {
            (self.on_open)();
        }
    }
    pub fn open_for_test(&mut self) {
        self.window.get_or_insert_with(|| Arc::new(WindowMarker));
    }
    pub fn close_for_test(&mut self) {
        if let Some(window) = self.window.take() {
            self.last_closed = Arc::downgrade(&window);
        }
        debug_assert!(self.last_closed.upgrade().is_none());
    }
    #[must_use]
    pub const fn has_window(&self) -> bool {
        self.window.is_some()
    }
    #[must_use]
    pub fn live_window_count(&self) -> usize {
        usize::from(self.window.is_some()) + usize::from(self.last_closed.upgrade().is_some())
    }
}

#[cfg(windows)]
pub struct NativeTray {
    icon: tray_icon::TrayIcon,
}

#[cfg(windows)]
impl NativeTray {
    pub fn new(
        status_text: &str,
        active_model: Option<&str>,
        recent: &[(ModelId, String)],
        dispatch: std::sync::Arc<dyn Fn(TrayCommand) + Send + Sync>,
    ) -> Result<Self, String> {
        use tray_icon::{TrayIconBuilder, menu::MenuEvent};
        let menu = build_menu(status_text, active_model, recent)?;
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let id = event.id().as_ref();
            let command = match id {
                "open" => Some(TrayCommand::Open),
                "eject" => Some(TrayCommand::Eject),
                "quit" => Some(TrayCommand::Quit),
                id if id.starts_with("recent:") => {
                    uuid::Uuid::parse_str(id.trim_start_matches("recent:"))
                        .ok()
                        .map(ModelId::from_uuid)
                        .map(TrayCommand::LoadRecent)
                }
                _ => None,
            };
            if let Some(command) = command {
                let dispatch = dispatch.clone();
                let _ = slint::invoke_from_event_loop(move || dispatch(command));
            }
        }));
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(format!("Model Launcher — {status_text}"))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self { icon })
    }

    pub fn update(&self, status: &str, active_model: Option<&str>, recent: &[(ModelId, String)]) {
        let _ = self
            .icon
            .set_tooltip(Some(format!("Model Launcher — {status}")));
        if let Ok(menu) = build_menu(status, active_model, recent) {
            self.icon.set_menu(Some(Box::new(menu)));
        }
    }

    pub fn show_close_notice(&self, message: &str) {
        let _ = self.icon.set_tooltip(Some(message));
    }
}

#[cfg(windows)]
fn build_menu(
    status_text: &str,
    active_model: Option<&str>,
    recent: &[(ModelId, String)],
) -> Result<tray_icon::menu::Menu, String> {
    use tray_icon::menu::{Menu, MenuItem, Submenu};
    let status = MenuItem::with_id("status", status_text, false, None);
    let active = MenuItem::with_id(
        "active",
        active_model.unwrap_or("No model loaded"),
        false,
        None,
    );
    let open = MenuItem::with_id("open", "Open Model Launcher", true, None);
    let eject = MenuItem::with_id("eject", "Eject current model", active_model.is_some(), None);
    let recent_menu = Submenu::with_id("recent", "Recent Models", !recent.is_empty());
    for (id, name) in recent.iter().take(8) {
        recent_menu
            .append(&MenuItem::with_id(
                format!("recent:{}", id.as_uuid()),
                name,
                true,
                None,
            ))
            .map_err(|error| error.to_string())?;
    }
    let quit = MenuItem::with_id("quit", "Quit", true, None);
    Menu::with_items(&[&status, &active, &open, &eject, &recent_menu, &quit])
        .map_err(|error| error.to_string())
}
