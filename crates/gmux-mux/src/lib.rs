//! gmux-mux — the native multiplexer model: sessions, windows, and panes, wiring [`gmux_pty`] and
//! [`gmux_vt`] together. M1 covers a single pane per window running a real shell; the split tree
//! (multiple panes per window) lands in M3, and the daemon/detach split in M6.

pub mod attention;
pub mod ids;
pub mod pane;

pub use attention::Attention;
pub use ids::{PaneId, SessionId, WindowId};
pub use pane::{Pane, PaneEvent, PaneSnapshot};

// Re-export the types callers need so they don't have to depend on gmux-pty / gmux-vt directly.
pub use gmux_pty::PtySize;
pub use gmux_vt::{Cell, Notification, NotifyKind, ProgressState, Rgb, Urgency};

use std::io;

/// A window: for M1, a single pane (the split tree arrives in M3).
pub struct Window {
    pub id: WindowId,
    pub pane: Pane,
}

impl Window {
    fn new(pane: Pane) -> Self {
        Window { id: WindowId::alloc(), pane }
    }
}

/// A session: an ordered set of windows plus the active index. The detach/attach unit.
pub struct Session {
    pub id: SessionId,
    pub name: String,
    pub windows: Vec<Window>,
    pub active: usize,
}

impl Session {
    fn new(name: impl Into<String>) -> Self {
        Session { id: SessionId::alloc(), name: name.into(), windows: Vec::new(), active: 0 }
    }

    pub fn active_window(&self) -> Option<&Window> {
        self.windows.get(self.active)
    }
}

/// The top-level multiplexer state. In-process for M1..M5; moves behind the daemon at M6.
#[derive(Default)]
pub struct Mux {
    pub sessions: Vec<Session>,
}

impl Mux {
    pub fn new() -> Self {
        Mux::default()
    }

    /// Create an empty session and return its id.
    pub fn new_session(&mut self, name: impl Into<String>) -> SessionId {
        let session = Session::new(name);
        let id = session.id;
        self.sessions.push(session);
        id
    }

    /// Spawn a new window+pane running `command_line` in the given session.
    pub fn new_pane(
        &mut self,
        session: SessionId,
        command_line: &str,
        size: PtySize,
    ) -> io::Result<PaneId> {
        let pane = Pane::spawn(command_line, size)?;
        let pane_id = pane.id;
        let s = self
            .session_mut(session)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no session {session}")))?;
        s.windows.push(Window::new(pane));
        s.active = s.windows.len() - 1;
        Ok(pane_id)
    }

    pub fn session(&self, id: SessionId) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    fn session_mut(&mut self, id: SessionId) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    pub fn session_by_name(&self, name: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.name == name)
    }

    /// Find a pane by id across all sessions/windows.
    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.sessions
            .iter()
            .flat_map(|s| s.windows.iter())
            .map(|w| &w.pane)
            .find(|p| p.id == id)
    }

    /// Total live pane count.
    pub fn pane_count(&self) -> usize {
        self.sessions.iter().map(|s| s.windows.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mux_session_lifecycle_without_spawning() {
        let mut mux = Mux::new();
        let a = mux.new_session("work");
        let b = mux.new_session("scratch");
        assert_ne!(a, b);
        assert_eq!(mux.sessions.len(), 2);
        assert_eq!(mux.session_by_name("work").unwrap().id, a);
        assert!(mux.session(b).unwrap().windows.is_empty());
        assert_eq!(mux.pane_count(), 0);
    }
}
