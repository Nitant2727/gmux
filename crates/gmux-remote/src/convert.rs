//! Pure tmux-layout → gmux-split-tree conversion.
//!
//! tmux layouts ([`gmux_tmux::Layout`]) are n-ary trees with absolute cell sizes; gmux's
//! [`Node`] is a binary tree with per-split ratios. An n-ary split with children
//! `[c1, c2, …, cn]` becomes right-leaning nested binary splits
//! `Split(c1, Split(c2, … Split(c(n-1), cn)))`, where each level's ratio is
//! `(size(first child) + 0.5) / remaining span` along the split axis (`{}` horizontal →
//! widths, `[]` vertical → heights). The `+ 0.5` targets the midpoint of the interval
//! `[first/span, (first+1)/span)`: gmux's `layout::rects` computes `floor(span * ratio)`, and
//! a ratio sitting exactly on `first/span` floors to `first − 1` for ~4% of geometries under
//! f32 rounding (a 1-cell tmux pane could even become 0-width). The remaining span starts as
//! the parent cell's own extent and shrinks by each peeled child; separator lines (tmux sizes
//! exclude the 1-cell border between siblings) end up absorbed by the last sibling, so a few
//! cells of drift vs the remote geometry is inherent to the pure-ratio tree.

use gmux_mux::layout::{Node, SplitDir};
use gmux_mux::PaneId;
use gmux_tmux::{Cell, Layout};

/// Convert a parsed tmux layout into a gmux split tree.
///
/// `id_of` maps each remote tmux pane id (`%N` without the sigil) to the local [`PaneId`]
/// mirroring it; it is called once per leaf, in leaf (left-to-right) order. Also returns the
/// remote pane ids in that same leaf order, so the caller can line panes up with the tree.
pub fn layout_to_node(
    layout: &Layout,
    id_of: &mut impl FnMut(u64) -> PaneId,
) -> (Node, Vec<u64>) {
    let mut order = Vec::new();
    let node = cell_to_node(&layout.root, id_of, &mut order);
    (node, order)
}

fn cell_to_node(
    cell: &Cell,
    id_of: &mut impl FnMut(u64) -> PaneId,
    order: &mut Vec<u64>,
) -> Node {
    match cell {
        Cell::Leaf { pane, .. } => {
            order.push(*pane);
            Node::Leaf(id_of(*pane))
        }
        Cell::Split { w, h, horizontal, children, .. } => {
            let span = u64::from(if *horizontal { *w } else { *h });
            nest(children, span, *horizontal, id_of, order)
        }
    }
}

/// Fold `children` into right-leaning binary splits over `span` cells along the split axis.
fn nest(
    children: &[Cell],
    span: u64,
    horizontal: bool,
    id_of: &mut impl FnMut(u64) -> PaneId,
    order: &mut Vec<u64>,
) -> Node {
    match children {
        [] => unreachable!("gmux_tmux::parse_layout guarantees splits have >= 1 child"),
        [only] => cell_to_node(only, id_of, order),
        [first, rest @ ..] => {
            let first_size = u64::from(size_along(first, horizontal));
            // Zero-size guard: a degenerate 0-extent split can't be divided meaningfully;
            // fall back to an even split rather than dividing by zero. The +0.5 keeps
            // `floor(span * ratio)` == first_size under f32 rounding (see module docs).
            let ratio = if span == 0 {
                0.5
            } else {
                ((first_size as f64 + 0.5) / span as f64).clamp(0.0, 1.0) as f32
            };
            let dir = if horizontal { SplitDir::Horizontal } else { SplitDir::Vertical };
            let a = cell_to_node(first, id_of, order);
            let b = nest(rest, span.saturating_sub(first_size), horizontal, id_of, order);
            Node::Split { dir, ratio, a: Box::new(a), b: Box::new(b) }
        }
    }
}

/// A cell's extent along the split axis: width for `{}` (horizontal), height for `[]`.
fn size_along(cell: &Cell, horizontal: bool) -> u32 {
    match cell {
        Cell::Leaf { w, h, .. } | Cell::Split { w, h, .. } => {
            if horizontal {
                *w
            } else {
                *h
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Map remote id `n` to local `PaneId(n + 100)` — offset so a test mixing them up fails.
    fn id_of(n: u64) -> PaneId {
        PaneId(n + 100)
    }

    fn convert(layout: &str) -> (Node, Vec<u64>) {
        let layout = gmux_tmux::parse_layout(layout).expect("test layout parses");
        layout_to_node(&layout, &mut id_of)
    }

    fn expect_leaf(node: &Node) -> PaneId {
        match node {
            Node::Leaf(id) => *id,
            other => panic!("expected leaf, got {other:?}"),
        }
    }

    fn expect_split(node: &Node) -> (SplitDir, f32, &Node, &Node) {
        match node {
            Node::Split { dir, ratio, a, b } => (*dir, *ratio, a, b),
            other => panic!("expected split, got {other:?}"),
        }
    }

    fn assert_ratio(actual: f32, expected: f64) {
        assert!(
            (f64::from(actual) - expected).abs() < 1e-6,
            "ratio {actual} != expected {expected}",
        );
    }

    #[test]
    fn single_leaf() {
        let (node, order) = convert("0000,80x24,0,0,7");
        assert_eq!(expect_leaf(&node), PaneId(107));
        assert_eq!(order, vec![7]);
    }

    #[test]
    fn doc_example_two_pane_horizontal() {
        // 159 wide window, two 79-wide panes side by side (1-cell separator between them).
        let (node, order) = convert("bb62,159x48,0,0{79x48,0,0,1,79x48,80,0,2}");
        let (dir, ratio, a, b) = expect_split(&node);
        assert_eq!(dir, SplitDir::Horizontal);
        assert_ratio(ratio, 79.5 / 159.0);
        assert_eq!(expect_leaf(a), PaneId(101));
        assert_eq!(expect_leaf(b), PaneId(102));
        assert_eq!(order, vec![1, 2]);
    }

    #[test]
    fn three_child_vertical_nests_right_leaning() {
        // 32 rows: 10 + 10 + 10 panes + 2 separator rows. By hand (midpoint ratios):
        //   level 0: span 32, first 10          → ratio 10.5/32
        //   level 1: span 32 − 10 = 22, first 10 → ratio 10.5/22
        let (node, order) = convert("0000,80x32,0,0[80x10,0,0,1,80x10,0,11,2,80x10,0,22,3]");
        let (dir, ratio, a, b) = expect_split(&node);
        assert_eq!(dir, SplitDir::Vertical);
        assert_ratio(ratio, 10.5 / 32.0);
        assert_eq!(expect_leaf(a), PaneId(101));
        let (dir_b, ratio_b, ba, bb) = expect_split(b);
        assert_eq!(dir_b, SplitDir::Vertical);
        assert_ratio(ratio_b, 10.5 / 22.0);
        assert_eq!(expect_leaf(ba), PaneId(102));
        assert_eq!(expect_leaf(bb), PaneId(103));
        assert_eq!(order, vec![1, 2, 3]);
    }

    #[test]
    fn mixed_nesting_recurses_per_axis() {
        // Left pane beside a vertical stack: {1, [2, 3]}.
        let (node, order) =
            convert("0000,159x48,0,0{79x48,0,0,1,79x48,80,0[79x23,80,0,2,79x24,80,24,3]}");
        let (dir, ratio, a, b) = expect_split(&node);
        assert_eq!(dir, SplitDir::Horizontal);
        assert_ratio(ratio, 79.5 / 159.0);
        assert_eq!(expect_leaf(a), PaneId(101));
        let (dir_b, ratio_b, ba, bb) = expect_split(b);
        assert_eq!(dir_b, SplitDir::Vertical);
        assert_ratio(ratio_b, 23.5 / 48.0);
        assert_eq!(expect_leaf(ba), PaneId(102));
        assert_eq!(expect_leaf(bb), PaneId(103));
        assert_eq!(order, vec![1, 2, 3]);
    }

    /// Every (first, span) geometry must survive gmux's `floor(span * ratio)` sizing exactly:
    /// the ratio floor-boundary bug made ~4% of pairs come out one cell short (a 1-cell tmux
    /// pane could become a ZERO-width local pane).
    #[test]
    fn ratio_survives_floor_sizing_for_all_geometries() {
        for span in 2u64..=400 {
            for first in 1..span {
                let ratio = ((first as f64 + 0.5) / span as f64).clamp(0.0, 1.0) as f32;
                let sized = (span as f32 * ratio) as u64; // gmux layout::rects arithmetic
                assert_eq!(sized, first, "floor(span*ratio) drifted for ({first}, {span})");
            }
        }
    }

    #[test]
    fn zero_size_split_falls_back_to_even_ratio() {
        // A 0-row vertical split cannot be divided by extent; guard yields 0.5, not NaN.
        let (node, order) = convert("0000,80x0,0,0[80x0,0,0,1,80x0,0,0,2]");
        let (dir, ratio, a, b) = expect_split(&node);
        assert_eq!(dir, SplitDir::Vertical);
        assert_ratio(ratio, 0.5);
        assert_eq!(expect_leaf(a), PaneId(101));
        assert_eq!(expect_leaf(b), PaneId(102));
        assert_eq!(order, vec![1, 2]);
    }
}
