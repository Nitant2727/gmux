//! The per-window split tree: a binary tree of panes with ratios, plus pure geometry (compute
//! each pane's rectangle) and spatial focus movement. Kept pane-free so it is fully unit-testable.

use crate::ids::PaneId;

/// How a split divides its area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDir {
    /// Side by side (a = left, b = right); divides width.
    Horizontal,
    /// Stacked (a = top, b = bottom); divides height.
    Vertical,
}

/// A direction to move focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

/// A pixel rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    fn cx(&self) -> i64 {
        self.x as i64 + self.w as i64 / 2
    }
    fn cy(&self) -> i64 {
        self.y as i64 + self.h as i64 / 2
    }
}

/// The split tree.
#[derive(Clone, Debug)]
pub enum Node {
    Leaf(PaneId),
    Split { dir: SplitDir, ratio: f32, a: Box<Node>, b: Box<Node> },
}

impl Node {
    pub fn leaf(id: PaneId) -> Node {
        Node::Leaf(id)
    }

    /// Replace `Leaf(target)` with a split of `target` and `new` (equal ratio). Returns whether
    /// the target was found.
    pub fn split_leaf(&mut self, target: PaneId, dir: SplitDir, new: PaneId) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                *self = Node::Split {
                    dir,
                    ratio: 0.5,
                    a: Box::new(Node::Leaf(target)),
                    b: Box::new(Node::Leaf(new)),
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.split_leaf(target, dir, new) || b.split_leaf(target, dir, new),
        }
    }

    /// Remove `target`, collapsing its parent split into the sibling subtree. Caller must ensure
    /// `target` is not the sole leaf.
    pub fn remove_leaf(self, target: PaneId) -> Node {
        match self {
            Node::Leaf(id) => Node::Leaf(id),
            Node::Split { dir, ratio, a, b } => {
                if matches!(&*a, Node::Leaf(x) if *x == target) {
                    return *b;
                }
                if matches!(&*b, Node::Leaf(x) if *x == target) {
                    return *a;
                }
                Node::Split {
                    dir,
                    ratio,
                    a: Box::new(a.remove_leaf(target)),
                    b: Box::new(b.remove_leaf(target)),
                }
            }
        }
    }

    /// Adjust the ratio of the split that directly parents `target` (grow/shrink that pane).
    pub fn resize_leaf(&mut self, target: PaneId, delta: f32) -> bool {
        if let Node::Split { ratio, a, b, .. } = self {
            let a_is = matches!(&**a, Node::Leaf(x) if *x == target);
            let b_is = matches!(&**b, Node::Leaf(x) if *x == target);
            if a_is || b_is {
                let d = if a_is { delta } else { -delta };
                *ratio = (*ratio + d).clamp(0.1, 0.9);
                return true;
            }
            return a.resize_leaf(target, delta) || b.resize_leaf(target, delta);
        }
        false
    }

    pub fn first_leaf(&self) -> PaneId {
        match self {
            Node::Leaf(id) => *id,
            Node::Split { a, .. } => a.first_leaf(),
        }
    }

    pub fn leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { a, b, .. } => {
                a.leaves(out);
                b.leaves(out);
            }
        }
    }

    pub fn leaf_count(&self) -> usize {
        let mut v = Vec::new();
        self.leaves(&mut v);
        v.len()
    }
}

/// Compute each leaf's rectangle within `area`.
pub fn rects(node: &Node, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
    match node {
        Node::Leaf(id) => out.push((*id, area)),
        Node::Split { dir, ratio, a, b } => match dir {
            SplitDir::Horizontal => {
                let aw = ((area.w as f32) * ratio) as u32;
                rects(a, Rect { x: area.x, y: area.y, w: aw, h: area.h }, out);
                rects(b, Rect { x: area.x + aw, y: area.y, w: area.w.saturating_sub(aw), h: area.h }, out);
            }
            SplitDir::Vertical => {
                let ah = ((area.h as f32) * ratio) as u32;
                rects(a, Rect { x: area.x, y: area.y, w: area.w, h: ah }, out);
                rects(b, Rect { x: area.x, y: area.y + ah, w: area.w, h: area.h.saturating_sub(ah) }, out);
            }
        },
    }
}

/// The best pane to focus when moving `dir` from `active`, given computed rects. Picks the nearest
/// pane whose center lies in that direction (with a penalty on the perpendicular axis).
pub fn neighbor(rects: &[(PaneId, Rect)], active: PaneId, dir: FocusDir) -> Option<PaneId> {
    let cur = rects.iter().find(|(id, _)| *id == active)?.1;
    let (acx, acy) = (cur.cx(), cur.cy());
    rects
        .iter()
        .filter(|(id, _)| *id != active)
        .filter_map(|(id, r)| {
            let (cx, cy) = (r.cx(), r.cy());
            let in_dir = match dir {
                FocusDir::Left => cx < acx,
                FocusDir::Right => cx > acx,
                FocusDir::Up => cy < acy,
                FocusDir::Down => cy > acy,
            };
            if !in_dir {
                return None;
            }
            let dist = match dir {
                FocusDir::Left | FocusDir::Right => (acx - cx).abs() + (acy - cy).abs() * 2,
                FocusDir::Up | FocusDir::Down => (acy - cy).abs() + (acx - cx).abs() * 2,
            };
            Some((*id, dist))
        })
        .min_by_key(|(_, d)| *d)
        .map(|(id, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::PaneId;

    fn p(n: u64) -> PaneId {
        PaneId(n)
    }

    #[test]
    fn split_then_rects_divides_area() {
        let mut root = Node::leaf(p(1));
        assert!(root.split_leaf(p(1), SplitDir::Horizontal, p(2)));
        let mut rs = Vec::new();
        rects(&root, Rect { x: 0, y: 0, w: 100, h: 40 }, &mut rs);
        assert_eq!(rs.len(), 2);
        // left half, right half
        assert_eq!(rs[0], (p(1), Rect { x: 0, y: 0, w: 50, h: 40 }));
        assert_eq!(rs[1], (p(2), Rect { x: 50, y: 0, w: 50, h: 40 }));
    }

    #[test]
    fn vertical_split_divides_height() {
        let mut root = Node::leaf(p(1));
        root.split_leaf(p(1), SplitDir::Vertical, p(2));
        let mut rs = Vec::new();
        rects(&root, Rect { x: 0, y: 0, w: 80, h: 40 }, &mut rs);
        assert_eq!(rs[0].1, Rect { x: 0, y: 0, w: 80, h: 20 });
        assert_eq!(rs[1].1, Rect { x: 0, y: 20, w: 80, h: 20 });
    }

    #[test]
    fn remove_collapses_sibling() {
        let mut root = Node::leaf(p(1));
        root.split_leaf(p(1), SplitDir::Horizontal, p(2));
        root.split_leaf(p(2), SplitDir::Vertical, p(3)); // now 1 | (2 / 3)
        assert_eq!(root.leaf_count(), 3);
        let root = root.remove_leaf(p(3)); // sibling 2 collapses up
        assert_eq!(root.leaf_count(), 2);
        let mut v = Vec::new();
        root.leaves(&mut v);
        assert!(v.contains(&p(1)) && v.contains(&p(2)) && !v.contains(&p(3)));
    }

    #[test]
    fn neighbor_moves_spatially() {
        // 1 | 2  (side by side)
        let mut root = Node::leaf(p(1));
        root.split_leaf(p(1), SplitDir::Horizontal, p(2));
        let mut rs = Vec::new();
        rects(&root, Rect { x: 0, y: 0, w: 100, h: 40 }, &mut rs);
        assert_eq!(neighbor(&rs, p(1), FocusDir::Right), Some(p(2)));
        assert_eq!(neighbor(&rs, p(2), FocusDir::Left), Some(p(1)));
        assert_eq!(neighbor(&rs, p(1), FocusDir::Left), None);
        assert_eq!(neighbor(&rs, p(1), FocusDir::Up), None);
    }

    #[test]
    fn resize_leaf_adjusts_parent_ratio() {
        let mut root = Node::leaf(p(1));
        root.split_leaf(p(1), SplitDir::Horizontal, p(2));
        assert!(root.resize_leaf(p(1), 0.2));
        let mut rs = Vec::new();
        rects(&root, Rect { x: 0, y: 0, w: 100, h: 40 }, &mut rs);
        assert_eq!(rs[0].1.w, 70); // 0.5 + 0.2
    }
}
