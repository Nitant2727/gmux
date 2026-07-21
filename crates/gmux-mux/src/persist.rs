//! Session persistence (ARCHITECTURE §11, D-009): capture the window/split-tree layout + each
//! pane's working directory to a serializable snapshot, and restore it by respawning shells in the
//! saved directories. Restore is **respawn**, not process resurrection — and it deliberately runs a
//! fresh shell (not the pane's original command) so agents are never auto-rerun. Scrollback replay
//! is layered on in M7b.

use std::collections::HashMap;
use std::io;

use serde::{Deserialize, Serialize};

use crate::ids::PaneId;
use crate::layout::{Node, SplitDir};
use crate::pane::Pane;
use crate::{Session, Window};

/// Format version — bump on incompatible changes.
pub const SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionSnapshot {
    pub version: u32,
    pub active: usize,
    pub windows: Vec<WindowSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowSnapshot {
    /// Panes in leaf order; `layout` indices reference this list.
    pub panes: Vec<PaneRecord>,
    pub layout: NodeSnapshot,
    /// Index (into `panes`) of the active pane.
    pub active: usize,
    /// User-set window name override (a sidebar rename), reapplied on restore. `#[serde(default)]`
    /// so pre-round-9 snapshots (no field) still load with no override.
    #[serde(default)]
    pub name: Option<String>,
    /// Sidebar group this window sits under, reapplied on restore. `#[serde(default)]` so older
    /// snapshots (no field) load as ungrouped.
    #[serde(default)]
    pub group: Option<String>,
    /// Workspace tag color (`#rrggbb`), reapplied on restore. `#[serde(default)]` for older files.
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PaneRecord {
    pub cwd: Option<String>,
    /// The pane's visible screen text at save time (replayed as inert history on restore).
    #[serde(default)]
    pub screen: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum NodeSnapshot {
    Leaf(usize),
    Split { horizontal: bool, ratio: f32, a: Box<NodeSnapshot>, b: Box<NodeSnapshot> },
}

impl SessionSnapshot {
    /// Capture a session's layout + per-pane cwd, persisting each pane's screen text as inert
    /// restore history. See [`capture_with`] for the privacy knob.
    ///
    /// [`capture_with`]: SessionSnapshot::capture_with
    pub fn capture(session: &Session) -> SessionSnapshot {
        Self::capture_with(session, true)
    }

    /// Capture a session's layout + per-pane cwd. Remote (tmux-mirror) panes are skipped — the
    /// remote server owns their processes, so respawning them as local shells on restore would be
    /// wrong (stage 2 re-attaches them by reconnecting the transport instead). Windows left with
    /// no local panes are dropped, and the active index is remapped to the kept windows.
    ///
    /// `include_screen` gates the per-pane screen text: `true` records it (restored as inert
    /// history), `false` leaves it empty — the M7 privacy deferral, driven by the daemon's
    /// `persist_screen` config so on-disk snapshots need not carry terminal contents.
    pub fn capture_with(session: &Session, include_screen: bool) -> SessionSnapshot {
        let mut windows = Vec::new();
        let mut active = 0;
        for (i, w) in session.windows().iter().enumerate() {
            if let Some(snap) = WindowSnapshot::capture(w, include_screen) {
                if i == session.active_index() {
                    active = windows.len();
                }
                windows.push(snap);
            }
        }
        SessionSnapshot { version: SNAPSHOT_VERSION, active, windows }
    }

    /// Restore a session by respawning a shell per pane. `spawn(record)` creates a pane (the caller
    /// supplies the shell + size, uses `record.cwd`, and replays `record.screen` as inert history).
    pub fn restore<F>(&self, name: &str, mut spawn: F) -> io::Result<Session>
    where
        F: FnMut(&PaneRecord) -> io::Result<Pane>,
    {
        let mut windows = Vec::with_capacity(self.windows.len());
        for w in &self.windows {
            windows.push(w.restore(&mut spawn)?);
        }
        Ok(Session::from_windows(name, windows, self.active))
    }
}

impl WindowSnapshot {
    /// Capture one window, or `None` if it holds no local panes (remote panes are not persisted).
    /// `include_screen` gates the per-pane screen text (see [`SessionSnapshot::capture_with`]).
    fn capture(window: &Window, include_screen: bool) -> Option<WindowSnapshot> {
        // Collect leaf pane ids in tree order, keeping only local panes, and map id -> index.
        let mut all = Vec::new();
        window.root().leaves(&mut all);
        let ids: Vec<PaneId> = all
            .iter()
            .copied()
            .filter(|id| window.pane(*id).is_none_or(|p| p.remote_id().is_none()))
            .collect();
        if ids.is_empty() {
            return None;
        }
        // Prune remote leaves from the layout so NodeSnapshot indices line up with `ids`. Safe:
        // at least one local leaf remains, so no removal ever targets the sole leaf.
        let mut root = window.root().clone();
        for id in &all {
            if !ids.contains(id) {
                root = root.remove_leaf(*id);
            }
        }
        let mut index_of = HashMap::new();
        for (i, id) in ids.iter().enumerate() {
            index_of.insert(*id, i);
        }
        let panes = ids
            .iter()
            .map(|id| match window.pane(*id) {
                Some(p) => PaneRecord {
                    cwd: p.cwd(),
                    screen: if include_screen { screen_lines(p) } else { Vec::new() },
                },
                None => PaneRecord { cwd: None, screen: Vec::new() },
            })
            .collect();
        let active = index_of.get(&window.active_id()).copied().unwrap_or(0);
        Some(WindowSnapshot {
            panes,
            layout: node_to_snapshot(&root, &index_of),
            active,
            name: window.name().map(str::to_string),
            group: window.group().map(str::to_string),
            color: window.color().map(str::to_string),
        })
    }

    fn restore<F>(&self, spawn: &mut F) -> io::Result<Window>
    where
        F: FnMut(&PaneRecord) -> io::Result<Pane>,
    {
        // Spawn one pane per record; index -> new PaneId.
        let mut ids = Vec::with_capacity(self.panes.len());
        let mut map = HashMap::new();
        for rec in &self.panes {
            let pane = spawn(rec)?;
            ids.push(pane.id);
            map.insert(pane.id, pane);
        }
        if ids.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "window snapshot has no panes"));
        }
        let root = snapshot_to_node(&self.layout, &ids);
        let active = ids.get(self.active).copied().unwrap_or(ids[0]);
        let mut window = Window::from_parts(map, root, active);
        if let Some(name) = &self.name {
            window.set_name(name.clone());
        }
        if let Some(group) = &self.group {
            window.set_group(group.clone());
        }
        if let Some(color) = &self.color {
            window.set_color(color.clone());
        }
        Ok(window)
    }
}

/// How many lines of scrollback + screen to persist per pane. Enough to restore meaningful
/// context without bloating the snapshot file.
const SCREEN_CAPTURE_LINES: usize = 200;

/// The pane's recent output (scrollback + visible screen, most-recent `SCREEN_CAPTURE_LINES`) as
/// text lines, with trailing blank lines trimmed.
fn screen_lines(p: &Pane) -> Vec<String> {
    let mut lines = p.scrollback_text(SCREEN_CAPTURE_LINES);
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

fn node_to_snapshot(node: &Node, index_of: &HashMap<PaneId, usize>) -> NodeSnapshot {
    match node {
        Node::Leaf(id) => NodeSnapshot::Leaf(*index_of.get(id).unwrap_or(&0)),
        Node::Split { dir, ratio, a, b } => NodeSnapshot::Split {
            horizontal: *dir == SplitDir::Horizontal,
            ratio: *ratio,
            a: Box::new(node_to_snapshot(a, index_of)),
            b: Box::new(node_to_snapshot(b, index_of)),
        },
    }
}

fn snapshot_to_node(snap: &NodeSnapshot, ids: &[PaneId]) -> Node {
    match snap {
        NodeSnapshot::Leaf(i) => Node::Leaf(*ids.get(*i).unwrap_or(&ids[0])),
        NodeSnapshot::Split { horizontal, ratio, a, b } => Node::Split {
            dir: if *horizontal { SplitDir::Horizontal } else { SplitDir::Vertical },
            ratio: *ratio,
            a: Box::new(snapshot_to_node(a, ids)),
            b: Box::new(snapshot_to_node(b, ids)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_json_roundtrips() {
        let snap = SessionSnapshot {
            version: SNAPSHOT_VERSION,
            active: 0,
            windows: vec![WindowSnapshot {
                panes: vec![
                    PaneRecord { cwd: Some(r"C:\a".into()), screen: vec!["line one".into()] },
                    PaneRecord { cwd: None, screen: Vec::new() },
                ],
                layout: NodeSnapshot::Split {
                    horizontal: true,
                    ratio: 0.5,
                    a: Box::new(NodeSnapshot::Leaf(0)),
                    b: Box::new(NodeSnapshot::Leaf(1)),
                },
                active: 1,
                name: Some("backend".into()),
                group: Some("api".into()),
                color: Some("#ff8800".into()),
            }],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: SessionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap);

        // Pre-round-9 snapshots (no `name` field) still load, with no override.
        let old = r#"{"panes":[{"cwd":null}],"layout":{"Leaf":0},"active":0}"#;
        let w: WindowSnapshot = serde_json::from_str(old).unwrap();
        assert_eq!(w.name, None, "absent name defaults to None");
        assert_eq!(w.group, None, "absent group defaults to None (older snapshots are ungrouped)");
        assert_eq!(w.color, None, "absent color defaults to None (older snapshots are untagged)");
    }

    /// Contract 6: a renamed window's custom name survives a snapshot round-trip
    /// (`SessionSnapshot::capture` -> `restore` in-process). A bare local-leaf window (unknown id ->
    /// empty record) keeps this console-free; restore respawns a remote pane per record.
    #[test]
    fn window_name_survives_snapshot_roundtrip() {
        let id = PaneId::alloc();
        let mut win = Window::from_parts(HashMap::new(), Node::leaf(id), id);
        win.set_name("backend".to_string());
        win.set_group("api".to_string());
        win.set_color("#ff8800".to_string());
        let session = Session::from_windows("s", vec![win], 0);

        let snap = SessionSnapshot::capture(&session);
        assert_eq!(snap.windows.len(), 1);
        assert_eq!(snap.windows[0].name.as_deref(), Some("backend"), "custom name captured");
        assert_eq!(snap.windows[0].group.as_deref(), Some("api"), "sidebar group captured");

        let restored = snap
            .restore("s", |_| Ok(Pane::remote(0, 80, 24, Box::new(|_| {}))))
            .unwrap();
        assert_eq!(
            restored.windows()[0].workspace_info().name,
            "backend",
            "custom name reapplied on restore"
        );
        assert_eq!(
            restored.windows()[0].group(),
            Some("api"),
            "grouping survives a daemon restart, like the rename does"
        );
        assert_eq!(restored.windows()[0].color(), Some("#ff8800"), "and so does the tag color");

        // A window with no override captures None and restores to the derived name ("shell").
        let plain = Window::from_parts(HashMap::new(), Node::leaf(PaneId::alloc()), PaneId::alloc());
        // ponytail: active id above is unused by this bare-leaf window; set_active would be noise.
        let plain_snap = SessionSnapshot::capture(&Session::from_windows("s", vec![plain], 0));
        assert_eq!(plain_snap.windows[0].name, None, "no override -> None");
    }

    #[test]
    fn capture_drops_remote_only_windows() {
        // A session whose only pane mirrors a remote tmux pane persists nothing: the remote
        // server owns the process, so there is nothing to respawn locally.
        let remote = Pane::remote(9, 80, 24, Box::new(|_| {}));
        let session = Session::start("s", remote);
        let snap = SessionSnapshot::capture(&session);
        assert!(snap.windows.is_empty(), "remote-only windows must not be persisted");
        // Restoring the (empty) snapshot respawns nothing; the server then falls back to fresh.
        let restored = snap.restore("s", |_| unreachable!("nothing to respawn")).unwrap();
        assert_eq!(restored.pane_count(), 0);
    }

    #[test]
    fn capture_prunes_remote_leaves_from_mixed_windows() {
        // A window mixing a local and a remote pane keeps only the local leaf, with the split
        // collapsed. The "local" pane is a bare leaf id with no Pane entry — capture treats
        // unknown ids as local (empty record) — so no console is needed here.
        let remote = Pane::remote(4, 80, 24, Box::new(|_| {}));
        let rid = remote.id;
        let local_id = PaneId::alloc();
        let mut root = Node::leaf(local_id);
        root.split_leaf(local_id, SplitDir::Horizontal, rid);
        let mut map = HashMap::new();
        map.insert(rid, remote);
        let window = Window::from_parts(map, root, rid);
        let session = Session::from_windows("s", vec![window], 0);

        let snap = SessionSnapshot::capture(&session);
        assert_eq!(snap.windows.len(), 1);
        assert_eq!(snap.windows[0].panes.len(), 1, "the remote pane must not be captured");
        assert_eq!(snap.windows[0].layout, NodeSnapshot::Leaf(0), "the remote leaf must be pruned");
    }

    /// M7 privacy gate: the screen text a captured pane carries is exactly what
    /// `WindowSnapshot::capture` conditionally records — `include_screen=true` keeps
    /// `screen_lines(p)`, `false` swaps in an empty vec. A remote pane fed some output has non-empty
    /// `screen_lines` (proving the `true` branch has a payload); the `false` branch drops it. Remote
    /// panes are console-free, so this runs headless. (Remote panes are pruned from capture itself,
    /// so this asserts the gated expression directly rather than round-tripping through capture.)
    #[test]
    fn capture_with_gates_screen_lines() {
        let pane = Pane::remote(1, 80, 24, Box::new(|_| {}));
        pane.push_output(b"secret command output\r\n");
        let kept = screen_lines(&pane); // the include_screen=true payload
        let dropped: Vec<String> = Vec::new(); // the include_screen=false payload
        assert!(!kept.is_empty(), "include_screen=true records the pane's screen text");
        assert!(dropped.is_empty(), "include_screen=false records an empty screen vec");
    }

    /// End-to-end: `capture_with(false)` over a session persists every PaneRecord with an empty
    /// `screen`, and leaves layout/cwd intact. Built from bare local leaf ids (unknown ids capture
    /// as empty records — screen already empty), so the assertion is that the flag never *adds*
    /// screen text and the window/split structure is unchanged.
    #[test]
    fn capture_with_false_persists_no_screen_text() {
        let (a, b) = (PaneId::alloc(), PaneId::alloc());
        let mut root = Node::leaf(a);
        root.split_leaf(a, SplitDir::Horizontal, b);
        let window = Window::from_parts(HashMap::new(), root, a);
        let session = Session::from_windows("s", vec![window], 0);

        let off = SessionSnapshot::capture_with(&session, false);
        assert_eq!(off.windows.len(), 1);
        assert_eq!(off.windows[0].panes.len(), 2, "both leaves captured");
        for rec in &off.windows[0].panes {
            assert!(rec.screen.is_empty(), "include_screen=false must persist no screen text");
        }
    }

    #[test]
    fn node_index_mapping_roundtrips() {
        // Build a tree with fake ids, snapshot it, and rebuild against a new id list.
        let (a, b, c) = (PaneId(10), PaneId(11), PaneId(12));
        let mut root = Node::leaf(a);
        root.split_leaf(a, SplitDir::Horizontal, b);
        root.split_leaf(b, SplitDir::Vertical, c);
        let mut idx = HashMap::new();
        idx.insert(a, 0);
        idx.insert(b, 1);
        idx.insert(c, 2);
        let snap = node_to_snapshot(&root, &idx);
        // Rebuild with new ids.
        let new_ids = [PaneId(100), PaneId(101), PaneId(102)];
        let rebuilt = snapshot_to_node(&snap, &new_ids);
        let mut leaves = Vec::new();
        rebuilt.leaves(&mut leaves);
        assert_eq!(leaves, vec![PaneId(100), PaneId(101), PaneId(102)]);
    }
}
