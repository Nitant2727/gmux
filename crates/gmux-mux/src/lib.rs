//! gmux-mux — the native multiplexer model: sessions, windows, and panes, wiring [`gmux_pty`] and
//! [`gmux_vt`] together. A window holds a binary split tree of panes ([`layout`]); a session is an
//! ordered set of windows (tabs). The daemon/detach split lands in M6.

pub mod attention;
pub mod ids;
pub mod layout;
pub mod pane;
pub mod persist;
pub mod workspace;

pub use attention::Attention;
pub use ids::{PaneId, SessionId, WindowId};
pub use layout::{FocusDir, Rect, SplitDir};
pub use pane::{Pane, PaneEvent, PaneSnapshot};
pub use persist::SessionSnapshot;
pub use workspace::WorkspaceInfo;

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
    /// The split tree root (for persistence/inspection).
    pub fn root(&self) -> &Node {
        &self.root
    }
    /// Build a window from a pre-constructed pane map + split tree (used by session restore).
    pub fn from_parts(panes: HashMap<PaneId, Pane>, root: Node, active: PaneId) -> Window {
        Window { id: WindowId::alloc(), panes, root, active, zoom: false }
    }

    /// Replace this window's split tree wholesale (the remote-tmux mirror path, where the remote's
    /// `%layout-change` is authoritative): insert `added` panes, install `root`, and remove panes
    /// the new tree no longer references, returning them so the caller can dispose of them (e.g.
    /// mark remote mirrors exited). The active pane is kept when still present, else reset to the
    /// tree's first leaf.
    pub fn replace_tree(&mut self, root: Node, added: Vec<Pane>) -> Vec<Pane> {
        for pane in added {
            self.panes.insert(pane.id, pane);
        }
        self.root = root;
        let mut keep = Vec::new();
        self.root.leaves(&mut keep);
        let pruned: Vec<PaneId> =
            self.panes.keys().filter(|id| !keep.contains(id)).copied().collect();
        let removed = pruned.into_iter().filter_map(|id| self.panes.remove(&id)).collect();
        if !self.panes.contains_key(&self.active) {
            self.active = self.root.first_leaf();
        }
        removed
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

    /// Sidebar metadata for this window: name, active pane's cwd, git branch, and attention.
    pub fn workspace_info(&self) -> WorkspaceInfo {
        let cwd = self.active_pane().cwd();
        let branch = cwd.as_deref().and_then(|c| workspace::git_branch(std::path::Path::new(c)));
        let name = cwd
            .as_deref()
            .map(workspace::cwd_name)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "shell".to_string());
        let attention = self.panes().any(|p| p.attention().is_pending());
        WorkspaceInfo { name, cwd, branch, attention }
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

    /// Build a session from pre-constructed windows (used by session restore).
    pub fn from_windows(name: impl Into<String>, windows: Vec<Window>, active: usize) -> Session {
        let active = if windows.is_empty() { 0 } else { active.min(windows.len() - 1) };
        Session { id: SessionId::alloc(), name: name.into(), windows, active }
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

    /// Append a pre-built window without stealing focus (remote windows appearing in the
    /// background must not yank the user off their active tab).
    pub fn push_window(&mut self, window: Window) -> WindowId {
        let id = window.id;
        self.windows.push(window);
        id
    }

    /// Find a window by id (ids are never reused, so a stale id can only miss, never alias).
    pub fn window_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    /// Remove a window by id, fixing the active index. Unlike [`Session::close_active_window`]
    /// this may remove the last window (a remote `%window-close` is not a user gesture to guard).
    pub fn remove_window(&mut self, id: WindowId) -> Option<Window> {
        let idx = self.windows.iter().position(|w| w.id == id)?;
        let win = self.windows.remove(idx);
        if idx < self.active {
            self.active -= 1;
        } else if self.active >= self.windows.len() && self.active > 0 {
            self.active = self.windows.len() - 1;
        }
        Some(win)
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

    /// A one-remote-pane window (no console needed — remote panes have no ConPTY).
    fn remote_window() -> Window {
        let pane = Pane::remote(0, 80, 24, Box::new(|_| {}));
        let id = pane.id;
        let mut panes = HashMap::new();
        panes.insert(id, pane);
        Window::from_parts(panes, Node::leaf(id), id)
    }

    #[test]
    fn push_window_does_not_steal_focus_and_remove_window_fixes_active() {
        let mut s = Session::from_windows("t", vec![remote_window(), remote_window()], 1);
        let kept = s.windows()[1].id;
        let added = s.push_window(remote_window());
        assert_eq!(s.active_index(), 1, "push_window must not move focus");
        assert_eq!(s.window_count(), 3);

        // Removing a window BEFORE the active one shifts the index down with it.
        let first = s.windows()[0].id;
        assert!(s.remove_window(first).is_some());
        assert_eq!(s.active_window().map(|w| w.id), Some(kept));

        // Removing a trailing window leaves the active index clamped and valid.
        assert!(s.remove_window(added).is_some());
        assert_eq!(s.active_window().map(|w| w.id), Some(kept));

        // Unlike close_active_window, the last window may go (a remote %window-close is not a
        // user gesture to guard).
        assert!(s.remove_window(kept).is_some());
        assert_eq!(s.window_count(), 0);
        assert!(s.active_window().is_none());
        assert!(s.remove_window(kept).is_none(), "ids are never reused; a second remove misses");
    }

    #[test]
    fn replace_tree_inserts_prunes_and_reactivates() {
        let mut win = remote_window();
        let old = win.active_id();
        let new_pane = Pane::remote(1, 40, 10, Box::new(|_| {}));
        let new_id = new_pane.id;

        // New tree keeps only the new pane: the old one must be pruned and handed back.
        let removed = win.replace_tree(Node::leaf(new_id), vec![new_pane]);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].id, old);
        assert_eq!(win.pane_count(), 1);
        assert_eq!(win.active_id(), new_id, "active must fall back to a live leaf");
        assert!(win.pane(old).is_none());
    }
}
