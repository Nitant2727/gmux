//! gmux-mux — the native multiplexer model: sessions, windows, and panes, wiring [`gmux_pty`] and
//! [`gmux_vt`] together. A window holds a binary split tree of panes ([`layout`]); a session is an
//! ordered set of windows (tabs). The daemon/detach split lands in M6.

pub mod attention;
pub mod ids;
pub mod layout;
pub mod pane;

pub use attention::Attention;
pub use ids::{PaneId, SessionId, WindowId};
pub use layout::{FocusDir, Rect, SplitDir};
pub use pane::{Pane, PaneEvent, PaneSnapshot};

// Re-export the types callers need so they don't have to depend on gmux-pty / gmux-vt directly.
pub use gmux_pty::PtySize;
pub use gmux_vt::{Cell, Notification, NotifyKind, ProgressState, Rgb, Urgency};

use std::collections::HashMap;
use std::io;

use layout::Node;

/// A window: a binary split tree of panes with one active pane and an optional zoom.
pub struct Window {
    pub id: WindowId,
    panes: HashMap<PaneId, Pane>,
    root: Node,
    active: PaneId,
    zoom: bool,
}

impl Window {
    fn new(pane: Pane) -> Self {
        let id = pane.id;
        let mut panes = HashMap::new();
        panes.insert(id, pane);
        Window { id: WindowId::alloc(), panes, root: Node::leaf(id), active: id, zoom: false }
    }

    pub fn active_id(&self) -> PaneId {
        self.active
    }
    pub fn active_pane(&self) -> &Pane {
        self.panes.get(&self.active).expect("active pane always exists")
    }
    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }
    pub fn panes(&self) -> impl Iterator<Item = &Pane> {
        self.panes.values()
    }
    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }
    pub fn set_active(&mut self, id: PaneId) {
        if self.panes.contains_key(&id) {
            self.active = id;
        }
    }
    pub fn zoomed(&self) -> bool {
        self.zoom
    }
    pub fn toggle_zoom(&mut self) {
        self.zoom = !self.zoom;
    }

    /// Split the active pane, insert `new_pane`, and focus it.
    pub fn split(&mut self, dir: SplitDir, new_pane: Pane) -> PaneId {
        let nid = new_pane.id;
        self.root.split_leaf(self.active, dir, nid);
        self.panes.insert(nid, new_pane);
        self.active = nid;
        self.zoom = false;
        nid
    }

    /// Close the active pane, collapsing its split. Returns the removed pane, or `None` if it was
    /// the window's last pane (the caller should then close the window).
    pub fn close_active(&mut self) -> Option<Pane> {
        if self.panes.len() <= 1 {
            return None;
        }
        let victim = self.active;
        let root = std::mem::replace(&mut self.root, Node::leaf(victim));
        self.root = root.remove_leaf(victim);
        let pane = self.panes.remove(&victim);
        self.active = self.root.first_leaf();
        self.zoom = false;
        pane
    }

    /// Move focus spatially within a `(w, h)` area.
    pub fn focus_dir(&mut self, dir: FocusDir, w: u32, h: u32) {
        let rs = self.layout_rects(w, h);
        if let Some(n) = layout::neighbor(&rs, self.active, dir) {
            self.active = n;
        }
    }

    /// Grow the active pane by `delta` (fraction) against its split sibling.
    pub fn resize_active(&mut self, delta: f32) {
        self.root.resize_leaf(self.active, delta);
    }

    /// Each pane's rectangle within a `(w, h)` area. When zoomed, the active pane fills it.
    pub fn layout_rects(&self, w: u32, h: u32) -> Vec<(PaneId, Rect)> {
        if self.zoom {
            return vec![(self.active, Rect { x: 0, y: 0, w, h })];
        }
        let mut rs = Vec::new();
        layout::rects(&self.root, Rect { x: 0, y: 0, w, h }, &mut rs);
        rs
    }
}

/// A session: an ordered set of windows (tabs) plus the active index. The detach/attach unit.
pub struct Session {
    pub id: SessionId,
    pub name: String,
    windows: Vec<Window>,
    active: usize,
}

impl Session {
    /// Create a session whose first window runs `pane`.
    pub fn start(name: impl Into<String>, pane: Pane) -> Self {
        Session {
            id: SessionId::alloc(),
            name: name.into(),
            windows: vec![Window::new(pane)],
            active: 0,
        }
    }

    fn empty(name: impl Into<String>) -> Self {
        Session { id: SessionId::alloc(), name: name.into(), windows: Vec::new(), active: 0 }
    }

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }
    pub fn windows(&self) -> &[Window] {
        &self.windows
    }
    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active_window(&self) -> Option<&Window> {
        self.windows.get(self.active)
    }
    pub fn active_window_mut(&mut self) -> Option<&mut Window> {
        self.windows.get_mut(self.active)
    }

    /// Add a new window (tab) running `pane` and focus it.
    pub fn add_window(&mut self, pane: Pane) -> WindowId {
        let w = Window::new(pane);
        let id = w.id;
        self.windows.push(w);
        self.active = self.windows.len() - 1;
        id
    }

    pub fn next_window(&mut self) {
        if !self.windows.is_empty() {
            self.active = (self.active + 1) % self.windows.len();
        }
    }
    pub fn prev_window(&mut self) {
        if !self.windows.is_empty() {
            self.active = (self.active + self.windows.len() - 1) % self.windows.len();
        }
    }

    /// Close the active window if more than one remains; returns whether a window was closed.
    pub fn close_active_window(&mut self) -> bool {
        if self.windows.len() <= 1 {
            return false;
        }
        self.windows.remove(self.active);
        if self.active >= self.windows.len() {
            self.active = self.windows.len() - 1;
        }
        true
    }

    /// Find a pane by id across all windows.
    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.windows.iter().find_map(|w| w.pane(id))
    }

    /// Remove a pane by id (e.g. its process exited), collapsing its split and dropping the window
    /// if it becomes empty. Returns whether the pane was found.
    pub fn remove_pane(&mut self, id: PaneId) -> bool {
        for wi in 0..self.windows.len() {
            if self.windows[wi].pane(id).is_some() {
                self.windows[wi].set_active(id);
                if self.windows[wi].close_active().is_none() {
                    // Was the window's last pane; drop the window.
                    self.windows.remove(wi);
                    if !self.windows.is_empty() && self.active >= self.windows.len() {
                        self.active = self.windows.len() - 1;
                    }
                }
                return true;
            }
        }
        false
    }

    pub fn pane_count(&self) -> usize {
        self.windows.iter().map(|w| w.pane_count()).sum()
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
        let session = Session::empty(name);
        let id = session.id;
        self.sessions.push(session);
        id
    }

    /// Spawn a new window (tab) running `command_line` in the given session.
    pub fn new_window(&mut self, session: SessionId, command_line: &str, size: PtySize) -> io::Result<PaneId> {
        let pane = Pane::spawn(command_line, size)?;
        let pane_id = pane.id;
        let s = self.session_mut(session).ok_or_else(|| not_found(session))?;
        s.add_window(pane);
        Ok(pane_id)
    }

    /// Split the active pane of the session's active window.
    pub fn split_active(
        &mut self,
        session: SessionId,
        dir: SplitDir,
        command_line: &str,
        size: PtySize,
    ) -> io::Result<PaneId> {
        let pane = Pane::spawn(command_line, size)?;
        let pane_id = pane.id;
        let s = self.session_mut(session).ok_or_else(|| not_found(session))?;
        let w = s.active_window_mut().ok_or_else(|| io::Error::other("session has no window"))?;
        w.split(dir, pane);
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

    /// Find a pane by id across all sessions.
    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.sessions.iter().find_map(|s| s.pane(id))
    }

    /// Total live pane count.
    pub fn pane_count(&self) -> usize {
        self.sessions.iter().map(|s| s.pane_count()).sum()
    }
}

fn not_found(session: SessionId) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("no session {session}"))
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
        assert_eq!(mux.session(b).unwrap().window_count(), 0);
        assert_eq!(mux.pane_count(), 0);
    }
}
