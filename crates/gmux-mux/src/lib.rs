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
pub use workspace::{PrBadge, PrStatus, WorkspaceInfo};

// Re-export the types callers need so they don't have to depend on gmux-pty / gmux-vt directly.
pub use gmux_pty::PtySize;
pub use gmux_vt::{Cell, Notification, NotifyKind, Palette, ProgressState, Rgb, Urgency};

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
    /// User-set name override (a sidebar rename); `None` uses the derived workspace name. Persisted
    /// via [`WindowSnapshot::name`], so a rename survives a daemon restart.
    name: Option<String>,
    /// Sidebar group this window belongs under (`None` = ungrouped, listed before every group).
    /// Persisted like `name`, so grouping survives a daemon restart.
    group: Option<String>,
    /// User-chosen `#rrggbb` tag color for this workspace's sidebar row. Persisted like `name`.
    color: Option<String>,
    /// A pull request badge (number + state) pushed in via `gmux pr`. Persisted like `name`, so a
    /// badge survives a daemon restart (the CLI need not re-resolve it every launch).
    pr: Option<workspace::PrBadge>,
    /// The workspace's directory. Every pane opened in this window — the first one, splits, and
    /// panes restored from a snapshot — starts here, so a workspace stays anchored to its project
    /// instead of drifting with whatever directory a shell was last `cd`'d into. Persisted.
    workspace_dir: Option<String>,
}

impl Window {
    fn new(pane: Pane) -> Self {
        let id = pane.id;
        let mut panes = HashMap::new();
        panes.insert(id, pane);
        Window { id: WindowId::alloc(), panes, root: Node::leaf(id), active: id, zoom: false, name: None, group: None, color: None, pr: None, workspace_dir: None }
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
        Window { id: WindowId::alloc(), panes, root, active, zoom: false, name: None, group: None, color: None, pr: None, workspace_dir: None }
    }

    /// Set (or clear) this window's custom name override. An empty `name` clears it back to the
    /// derived workspace name (see [`Window::workspace_info`]).
    pub fn set_name(&mut self, name: String) {
        self.name = if name.is_empty() { None } else { Some(name) };
    }

    /// This window's custom name override (a sidebar rename), or `None` if it uses the derived
    /// workspace name. Read by [`WindowSnapshot::capture`] to persist the override across restarts.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Put this window in a sidebar group, or take it out (empty name = ungrouped).
    pub fn set_group(&mut self, group: String) {
        self.group = if group.is_empty() { None } else { Some(group) };
    }

    /// This window's sidebar group, or `None` when it is ungrouped.
    pub fn group(&self) -> Option<&str> {
        self.group.as_deref()
    }

    /// Tag this workspace with a `#rrggbb` color, or clear it with an empty string.
    pub fn set_color(&mut self, color: String) {
        self.color = if color.is_empty() { None } else { Some(color) };
    }

    /// This workspace's tag color, or `None` when it is untagged.
    pub fn color(&self) -> Option<&str> {
        self.color.as_deref()
    }

    /// Set (or clear, with `None`) this workspace's pull-request badge.
    pub fn set_pr(&mut self, pr: Option<workspace::PrBadge>) {
        self.pr = pr;
    }

    /// This workspace's pull-request badge, if one is set.
    pub fn pr(&self) -> Option<&workspace::PrBadge> {
        self.pr.as_ref()
    }

    /// Anchor this workspace to a directory (empty clears it back to "wherever the shell starts").
    pub fn set_workspace_dir(&mut self, dir: String) {
        self.workspace_dir = if dir.is_empty() { None } else { Some(dir) };
    }

    /// The workspace's directory: where every new pane in this window opens.
    pub fn workspace_dir(&self) -> Option<&str> {
        self.workspace_dir.as_deref()
    }

    /// Whether any pane in this window has running children (a build, an agent) — the sidebar
    /// spins a activity indicator while true.
    pub fn is_busy(&self) -> bool {
        self.panes().any(|p| p.is_busy())
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

    /// Swap two panes' positions in this window's split tree (a drag-and-drop rearrange). The
    /// layout's shape is untouched; only which pane sits in which slot changes.
    pub fn swap_panes(&mut self, a: PaneId, b: PaneId) -> bool {
        self.root.swap_leaves(a, b)
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

    /// Drag-resize the divider for `pane` (the top/left pane of the dragged divider) by fractional
    /// ratio deltas `dx` (vertical divider) / `dy` (horizontal divider). Does NOT change focus — a
    /// drag on any pane's divider must not steal the active pane. A gone `pane` is a no-op.
    pub fn resize_pane(&mut self, pane: PaneId, dx: f32, dy: f32) {
        self.root.resize_leaf_of(pane, dx, dy);
    }

    /// Sidebar metadata for this window: name, active pane's cwd, git branch, and attention.
    pub fn workspace_info(&self) -> WorkspaceInfo {
        let cwd = self.active_pane().cwd();
        let branch = cwd.as_deref().and_then(|c| workspace::git_branch(std::path::Path::new(c)));
        // A custom name (sidebar rename) wins over the derived cwd name.
        let name = self.name.clone().unwrap_or_else(|| {
            cwd.as_deref()
                .map(workspace::cwd_name)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "shell".to_string())
        });
        let attention = self.panes().any(|p| p.attention().is_pending());
        let unread = self.panes().map(|p| p.unread()).sum();
        WorkspaceInfo { name, cwd, branch, attention, unread }
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

    /// Activate the window at `index` (a sidebar click). No-op (returns `false`) if out of range.
    pub fn select_window(&mut self, index: usize) -> bool {
        if index < self.windows.len() {
            self.active = index;
            true
        } else {
            false
        }
    }

    /// Focus a specific pane by id, activating its window and making it that window's active pane.
    /// No-op (returns `false`) if no window holds the pane.
    pub fn focus_pane(&mut self, id: PaneId) -> bool {
        if let Some(wi) = self.windows.iter().position(|w| w.pane(id).is_some()) {
            self.active = wi;
            self.windows[wi].set_active(id);
            true
        } else {
            false
        }
    }

    /// Reorder tabs: move the window at `from` to index `to` (a sidebar drag-drop). Both indices
    /// are clamped to the window count. The active tab follows the moved window when it *was* the
    /// moved one; otherwise the active index shifts to track its window's new position.
    pub fn move_window(&mut self, from: usize, to: usize) {
        let len = self.windows.len();
        if len == 0 {
            return;
        }
        let from = from.min(len - 1);
        let to = to.min(len - 1);
        if from == to {
            return;
        }
        let win = self.windows.remove(from);
        self.windows.insert(to, win);
        self.active = reindex_after_move(self.active, from, to);
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

    /// Take a pane out of whatever window holds it — collapsing its split, dropping the window if
    /// it empties, fixing the active index — WITHOUT disposing of it. The remote mirror re-homes
    /// panes moved between remote windows (`join-pane`/`break-pane`) this way: extracted here,
    /// re-inserted into the destination window's tree.
    pub fn extract_pane(&mut self, id: PaneId) -> Option<Pane> {
        let wi = self.windows.iter().position(|w| w.pane(id).is_some())?;
        if self.windows[wi].pane_count() <= 1 {
            let mut win = self.windows.remove(wi);
            if wi < self.active {
                self.active -= 1;
            } else if self.active >= self.windows.len() && self.active > 0 {
                self.active = self.windows.len() - 1;
            }
            win.panes.remove(&id)
        } else {
            self.windows[wi].set_active(id);
            self.windows[wi].close_active()
        }
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

/// New index of the tracked (active) tab after moving the window at `from` to `to`. Models the
/// `remove(from)` + `insert(to)` the reorder does: the moved window itself lands at `to`; any other
/// tracked index shifts if the removal or insertion straddles it. Pure, so unit-tested directly.
fn reindex_after_move(active: usize, from: usize, to: usize) -> usize {
    if active == from {
        return to;
    }
    // After remove(from): indices past `from` drop by one.
    let after_remove = if active > from { active - 1 } else { active };
    // After insert(to): indices at/after `to` rise by one.
    if after_remove >= to {
        after_remove + 1
    } else {
        after_remove
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
    fn select_window_and_focus_pane_target_by_index_and_id() {
        let mut s = Session::from_windows("t", vec![remote_window(), remote_window()], 0);
        let w1_pane = s.windows()[1].active_id();

        // Out-of-range select is a no-op; in-range moves the active index.
        assert!(!s.select_window(5));
        assert_eq!(s.active_index(), 0);
        assert!(s.select_window(1));
        assert_eq!(s.active_index(), 1);

        // focus_pane jumps to whichever window holds the pane and activates it there.
        let w0_pane = s.windows()[0].active_id();
        assert!(s.focus_pane(w0_pane));
        assert_eq!(s.active_index(), 0);
        assert_eq!(s.active_window().unwrap().active_id(), w0_pane);

        // A pane in another window pulls focus back to that window.
        assert!(s.focus_pane(w1_pane));
        assert_eq!(s.active_index(), 1);

        // An unknown pane id is a no-op.
        let orphan = Pane::remote(99, 80, 24, Box::new(|_| {}));
        assert!(!s.focus_pane(orphan.id));
        assert_eq!(s.active_index(), 1);
    }

    #[test]
    fn move_window_reorders_and_tracks_active() {
        // Four tabs; capture their ids so we can assert order independent of indices.
        let mut s = Session::from_windows(
            "t",
            vec![remote_window(), remote_window(), remote_window(), remote_window()],
            2,
        );
        let ids: Vec<_> = s.windows().iter().map(|w| w.id).collect();

        // Move the ACTIVE tab (index 2) to the front: it follows to index 0.
        s.move_window(2, 0);
        assert_eq!(s.active_index(), 0, "the moved active tab follows to its new index");
        assert_eq!(s.windows()[0].id, ids[2]);
        assert_eq!(s.windows().iter().map(|w| w.id).collect::<Vec<_>>(), vec![ids[2], ids[0], ids[1], ids[3]]);

        // Now active is at 0 (ids[2]). Move a tab from after it (index 3, ids[3]) to before it
        // (index 0): the active tab shifts right by one to make room.
        s.move_window(3, 0);
        assert_eq!(s.windows()[0].id, ids[3]);
        assert_eq!(s.active_index(), 1, "active shifts to track its window after an insert before it");
        assert_eq!(s.windows()[1].id, ids[2], "active still points at the same window");

        // A clamped / no-op move leaves the active index untouched.
        let before = s.active_index();
        s.move_window(99, 99); // both clamp to len-1 -> from == to -> no-op
        assert_eq!(s.active_index(), before);
    }

    #[test]
    fn reindex_after_move_cases() {
        // Moving the tracked index itself: it follows to `to`.
        assert_eq!(reindex_after_move(2, 2, 0), 0);
        // Move right across the tracked index: it shifts left by one.
        assert_eq!(reindex_after_move(2, 0, 3), 1);
        // Move left across the tracked index: it shifts right by one.
        assert_eq!(reindex_after_move(1, 3, 0), 2);
        // A move entirely on one side of the tracked index leaves it put.
        assert_eq!(reindex_after_move(0, 2, 3), 0);
    }

    /// Drag-resize adjusts the split ratio without changing the active pane (a divider drag on a
    /// non-focused pane must not steal focus). Two side-by-side panes, active = the right one.
    #[test]
    fn resize_pane_changes_ratio_without_changing_focus() {
        let a = Pane::remote(1, 80, 24, Box::new(|_| {}));
        let b = Pane::remote(2, 80, 24, Box::new(|_| {}));
        let (ida, idb) = (a.id, b.id);
        let root = Node::Split {
            dir: SplitDir::Horizontal,
            ratio: 0.5,
            a: Box::new(Node::Leaf(ida)),
            b: Box::new(Node::Leaf(idb)),
        };
        let mut panes = HashMap::new();
        panes.insert(ida, a);
        panes.insert(idb, b);
        let mut win = Window::from_parts(panes, root, idb); // right pane active

        // Drag the vertical divider right: grow the LEFT pane (the divider's top/left side).
        win.resize_pane(ida, 0.2, 0.0);
        assert_eq!(win.active_id(), idb, "a divider drag must not change focus");
        let rs = win.layout_rects(100, 40);
        let wa = rs.iter().find(|(id, _)| *id == ida).unwrap().1.w;
        assert_eq!(wa, 70, "left pane grew to 0.5 + 0.2 of the width");
    }

    /// A custom name override wins over the derived workspace name; an empty name clears it back.
    /// A remote pane has no cwd, so the derived name is "shell".
    #[test]
    fn set_name_overrides_and_clears_workspace_name() {
        let mut win = remote_window();
        assert_eq!(win.workspace_info().name, "shell", "derived name with no cwd");
        win.set_name("backend".to_string());
        assert_eq!(win.workspace_info().name, "backend", "custom name wins");
        win.set_name(String::new());
        assert_eq!(win.workspace_info().name, "shell", "empty name clears the override");
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
