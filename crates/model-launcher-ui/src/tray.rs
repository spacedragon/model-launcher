use std::sync::{Arc, Weak};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    Open,
    Eject,
    LoadRecent,
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
            "recent" => Some(TrayCommand::LoadRecent),
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
