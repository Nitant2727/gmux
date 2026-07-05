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
    /// Capture a session's layout + per-pane cwd.
    pub fn capture(session: &Session) -> SessionSnapshot {
        let windows = session
            .windows()
            .iter()
            .map(WindowSnapshot::capture)
            .collect();
        SessionSnapshot { version: SNAPSHOT_VERSION, active: session.active_index(), windows }
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
    fn capture(window: &Window) -> WindowSnapshot {
        // Collect leaf pane ids in tree order and map id -> index.
        let mut ids = Vec::new();
        window.root().leaves(&mut ids);
        let mut index_of = HashMap::new();
        for (i, id) in ids.iter().enumerate() {
            index_of.insert(*id, i);
        }
        let panes = ids
            .iter()
            .map(|id| match window.pane(*id) {
                Some(p) => PaneRecord { cwd: p.cwd(), screen: screen_lines(p) },
                None => PaneRecord { cwd: None, screen: Vec::new() },
            })
            .collect();
        let active = index_of.get(&window.active_id()).copied().unwrap_or(0);
        WindowSnapshot { panes, layout: node_to_snapshot(window.root(), &index_of), active }
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
        Ok(Window::from_parts(map, root, active))
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
            }],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: SessionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap);
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
