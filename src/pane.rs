//! Split tree describing how panes tile the window.
//!
//! A split holds *n* children along one axis rather than two, so splitting the
//! same row repeatedly gives evenly sized panes instead of the 50/25/25 that a
//! binary tree produces. Weights are relative; layout normalises by their sum.

pub type PaneId = u64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Axis {
    /// Children sit side by side; the dividers between them are vertical.
    Horizontal,
    /// Children are stacked; the dividers between them are horizontal.
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

pub struct Child {
    pub node: Node,
    /// Share of the parent's extent, relative to its siblings.
    pub weight: f32,
}

impl Child {
    fn new(node: Node) -> Self {
        Child { node, weight: 1.0 }
    }
}

pub enum Node {
    Leaf(PaneId),
    Split { axis: Axis, children: Vec<Child> },
}

/// Gutter between adjacent panes, in physical pixels.
const GAP: f32 = 2.0;
/// A pane may never be squeezed below this fraction of its split.
const MIN_WEIGHT: f32 = 0.05;

impl Node {
    pub fn leaf(id: PaneId) -> Self {
        Node::Leaf(id)
    }

    /// Flattens the tree into `(pane, rect)` pairs covering `rect`.
    pub fn layout(&self, rect: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Node::Leaf(id) => out.push((*id, rect)),
            Node::Split { axis, children } => {
                for (child, r) in children.iter().zip(child_rects(rect, *axis, children)) {
                    child.node.layout(r, out);
                }
            }
        }
    }

    /// Rects of the gutters themselves, for drawing the divider lines.
    pub fn dividers(&self, rect: Rect, out: &mut Vec<Rect>) {
        let Node::Split { axis, children } = self else { return };
        let rects = child_rects(rect, *axis, children);
        for pair in rects.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            out.push(match axis {
                Axis::Horizontal => {
                    Rect { x: a.x + a.w, y: rect.y, w: (b.x - (a.x + a.w)).max(1.0), h: rect.h }
                }
                Axis::Vertical => {
                    Rect { x: rect.x, y: a.y + a.h, w: rect.w, h: (b.y - (a.y + a.h)).max(1.0) }
                }
            });
        }
        for (child, r) in children.iter().zip(rects.iter()) {
            child.node.dividers(*r, out);
        }
    }

    pub fn leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { children, .. } => {
                for c in children {
                    c.node.leaves(out);
                }
            }
        }
    }

    fn holds(&self, target: PaneId) -> bool {
        match self {
            Node::Leaf(id) => *id == target,
            Node::Split { children, .. } => children.iter().any(|c| c.node.holds(target)),
        }
    }

    /// Adds `new_id` next to `target`, splitting along `axis`.
    ///
    /// When `target` already sits in a split of the same axis the new pane
    /// joins that split as a sibling and every member is re-weighted evenly —
    /// so N panes in a row are always 1/N each. Splitting across the other axis
    /// nests a new two-pane split in `target`'s place.
    ///
    /// Returns false if `target` isn't in the tree.
    pub fn split(&mut self, target: PaneId, axis: Axis, new_id: PaneId) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                *self = Node::Split {
                    axis,
                    children: vec![Child::new(Node::Leaf(target)), Child::new(Node::Leaf(new_id))],
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { axis: my_axis, children } => {
                if *my_axis == axis {
                    let at = children
                        .iter()
                        .position(|c| matches!(c.node, Node::Leaf(id) if id == target));
                    if let Some(i) = at {
                        children.insert(i + 1, Child::new(Node::Leaf(new_id)));
                        for c in children.iter_mut() {
                            c.weight = 1.0;
                        }
                        return true;
                    }
                }
                children.iter_mut().any(|c| c.node.split(target, axis, new_id))
            }
        }
    }

    /// Removes `target`. Surviving siblings keep their relative sizes and share
    /// out the freed space; a split left with one child collapses into it.
    /// Returns false if `target` is the root leaf.
    pub fn remove(&mut self, target: PaneId) -> bool {
        if !self.remove_inner(target) {
            return false;
        }
        // Collapsing a split can leave its child nested inside a grandparent of
        // the same axis, which would make that group count as one member and
        // bring back uneven sizing on the next split.
        self.flatten();
        true
    }

    fn remove_inner(&mut self, target: PaneId) -> bool {
        let Node::Split { children, .. } = self else { return false };
        let at = children.iter().position(|c| matches!(c.node, Node::Leaf(id) if id == target));
        if let Some(i) = at {
            children.remove(i);
            if children.len() == 1 {
                let only = children.pop().expect("just checked len == 1");
                *self = only.node;
            }
            return true;
        }
        children.iter_mut().any(|c| c.node.remove_inner(target))
    }

    /// Splices same-axis children into their parent, so a row is always one
    /// flat group. A spliced group's members divide the share it held, which
    /// keeps every pane exactly where it was on screen.
    fn flatten(&mut self) {
        let collapse = {
            let Node::Split { axis, children } = self else { return };
            let my_axis = *axis;
            for c in children.iter_mut() {
                c.node.flatten();
            }
            let mut out: Vec<Child> = Vec::with_capacity(children.len());
            for child in std::mem::take(children) {
                match child.node {
                    Node::Split { axis: inner_axis, children: inner } if inner_axis == my_axis => {
                        let total: f32 = inner.iter().map(|c| c.weight.max(0.0)).sum();
                        let total = if total > 0.0 { total } else { inner.len().max(1) as f32 };
                        for mut grandchild in inner {
                            grandchild.weight = child.weight * (grandchild.weight.max(0.0) / total);
                            out.push(grandchild);
                        }
                    }
                    node => out.push(Child { node, weight: child.weight }),
                }
            }
            *children = out;
            children.len() == 1
        };
        if collapse {
            let only = match self {
                Node::Split { children, .. } => children.pop(),
                Node::Leaf(_) => None,
            };
            if let Some(only) = only {
                *self = only.node;
            }
        }
    }

    /// Nudges the boundary next to `target` within the innermost split of
    /// `axis` that contains it. `delta` is a fraction of that split's extent,
    /// taken from (or given to) the adjacent pane.
    pub fn resize(&mut self, target: PaneId, axis: Axis, delta: f32) -> bool {
        let Node::Split { axis: my_axis, children } = self else { return false };
        // Let a descendant claim it first, so the innermost split wins.
        if children.iter_mut().any(|c| c.node.resize(target, axis, delta)) {
            return true;
        }
        if *my_axis != axis || children.len() < 2 {
            return false;
        }
        let Some(i) = children.iter().position(|c| c.node.holds(target)) else {
            return false;
        };

        let total: f32 = children.iter().map(|c| c.weight.max(0.0)).sum();
        if total <= 0.0 {
            return false;
        }
        for c in children.iter_mut() {
            c.weight = c.weight.max(0.0) / total;
        }

        // Grow into the next pane, or the previous one when we're last.
        let j = if i + 1 < children.len() { i + 1 } else { i - 1 };
        let (wi, wj) = (children[i].weight, children[j].weight);
        let lo = MIN_WEIGHT - wi;
        let hi = wj - MIN_WEIGHT;
        let d = if lo > hi { 0.0 } else { delta.clamp(lo, hi) };
        children[i].weight = wi + d;
        children[j].weight = wj - d;
        true
    }
}

/// Divides `rect` among `children` along `axis`, leaving a gutter between each.
fn child_rects(rect: Rect, axis: Axis, children: &[Child]) -> Vec<Rect> {
    let n = children.len();
    if n == 0 {
        return Vec::new();
    }
    let total: f32 = children.iter().map(|c| c.weight.max(0.0)).sum();
    // Fall back to an even split if the weights are degenerate.
    let total = if total > 0.0 { total } else { n as f32 };

    let gaps = GAP * (n - 1) as f32;
    let (start, span) = match axis {
        Axis::Horizontal => (rect.x, rect.w),
        Axis::Vertical => (rect.y, rect.h),
    };
    let usable = (span - gaps).max(0.0);

    let mut out = Vec::with_capacity(n);
    let mut cursor = start;
    for (i, c) in children.iter().enumerate() {
        // The last child takes whatever is left, so rounding can't leave a seam.
        let size = if i == n - 1 {
            (start + span - cursor).max(0.0)
        } else {
            (usable * (c.weight.max(0.0) / total)).round()
        };
        out.push(match axis {
            Axis::Horizontal => Rect { x: cursor, y: rect.y, w: size, h: rect.h },
            Axis::Vertical => Rect { x: rect.x, y: cursor, w: rect.w, h: size },
        });
        cursor += size + GAP;
    }
    out
}

/// Picks the pane nearest to `from` in the given direction, comparing centres.
/// `dx`/`dy` is the unit direction, e.g. `(-1, 0)` for "focus left".
pub fn neighbor(rects: &[(PaneId, Rect)], from: PaneId, dx: f32, dy: f32) -> Option<PaneId> {
    let (fx, fy) = rects.iter().find(|(id, _)| *id == from).map(|(_, r)| r.center())?;
    rects
        .iter()
        .filter(|(id, _)| *id != from)
        .filter_map(|(id, r)| {
            let (cx, cy) = r.center();
            // Distance along the direction of travel, and perpendicular to it.
            let along = (cx - fx) * dx + (cy - fy) * dy;
            let across = ((cx - fx) * dy.abs() + (cy - fy) * dx.abs()).abs();
            (along > 1.0).then_some((id, along, across))
        })
        // Prefer well-aligned panes, then near ones.
        .min_by(|a, b| {
            (a.2 * 4.0 + a.1).partial_cmp(&(b.2 * 4.0 + b.1)).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(id, _, _)| *id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: Rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };

    fn layout_of(node: &Node) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        node.layout(FULL, &mut out);
        out
    }

    /// Widths in pane-id order, for checking even distribution.
    fn widths(node: &Node) -> Vec<f32> {
        layout_of(node).into_iter().map(|(_, r)| r.w).collect()
    }

    fn assert_even(sizes: &[f32]) {
        let (min, max) =
            sizes.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        // Rounding to whole pixels can leave a pixel or two of slack.
        assert!(max - min <= 2.0, "sizes should be even, got {sizes:?}");
    }

    #[test]
    fn a_single_leaf_fills_the_area() {
        let out = layout_of(&Node::leaf(1));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], (1, FULL));
    }

    #[test]
    fn horizontal_split_puts_panes_side_by_side() {
        let mut t = Node::leaf(1);
        assert!(t.split(1, Axis::Horizontal, 2));
        let out = layout_of(&t);
        assert_eq!(out.len(), 2);
        let (a, b) = (out[0].1, out[1].1);
        assert_eq!(out[0].0, 1);
        assert_eq!(out[1].0, 2);
        assert!(a.x < b.x, "pane 1 should be on the left");
        assert_eq!(a.h, FULL.h);
        assert_eq!(b.h, FULL.h);
        // The panes plus the gutter must span the full width exactly.
        assert!((b.x + b.w - FULL.w).abs() < 0.01);
        assert!(b.x > a.x + a.w, "there must be a gutter between panes");
        assert_even(&[a.w, b.w]);
    }

    #[test]
    fn vertical_split_stacks_panes() {
        let mut t = Node::leaf(1);
        assert!(t.split(1, Axis::Vertical, 2));
        let out = layout_of(&t);
        let (a, b) = (out[0].1, out[1].1);
        assert_eq!(a.w, FULL.w);
        assert!(a.y < b.y);
        assert!((b.y + b.h - FULL.h).abs() < 0.01);
        assert_even(&[a.h, b.h]);
    }

    #[test]
    fn repeated_splits_on_one_axis_stay_even() {
        // The whole point of the n-ary split: 1/N each, not 50/25/25.
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2);
        t.split(2, Axis::Horizontal, 3);
        assert_even(&widths(&t));

        t.split(3, Axis::Horizontal, 4);
        t.split(4, Axis::Horizontal, 5);
        let w = widths(&t);
        assert_eq!(w.len(), 5);
        assert_even(&w);
        // And they still tile the full width.
        let out = layout_of(&t);
        let last = out.last().unwrap().1;
        assert!((last.x + last.w - FULL.w).abs() < 0.01);
    }

    #[test]
    fn splitting_the_first_pane_again_stays_even() {
        // Growth from the left end, not just the right.
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2);
        t.split(1, Axis::Horizontal, 3);
        assert_even(&widths(&t));
        let mut leaves = Vec::new();
        t.leaves(&mut leaves);
        assert_eq!(leaves, vec![1, 3, 2], "the new pane goes next to its source");
    }

    #[test]
    fn a_split_across_the_other_axis_nests() {
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2);
        t.split(2, Axis::Vertical, 3);
        let out = layout_of(&t);
        assert_eq!(out.len(), 3);
        let (a, b, c) = (out[0].1, out[1].1, out[2].1);
        // Pane 1 keeps the full-height left half; 2 and 3 share the right half.
        assert_eq!(a.h, FULL.h);
        assert_even(&[a.w, b.w]);
        assert_even(&[b.h, c.h]);
    }

    #[test]
    fn a_split_reverts_to_even_after_a_manual_resize() {
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2);
        t.resize(1, Axis::Horizontal, 0.3);
        assert!(widths(&t)[0] > widths(&t)[1], "resize should have taken effect");
        t.split(2, Axis::Horizontal, 3);
        assert_even(&widths(&t));
    }

    #[test]
    fn nested_splits_never_overlap() {
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2);
        t.split(2, Axis::Vertical, 3);
        t.split(1, Axis::Vertical, 4);
        let out = layout_of(&t);
        assert_eq!(out.len(), 4);
        for i in 0..out.len() {
            for j in (i + 1)..out.len() {
                let (a, b) = (out[i].1, out[j].1);
                let overlaps =
                    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
                assert!(!overlaps, "{:?} overlaps {:?}", a, b);
            }
        }
    }

    #[test]
    fn splitting_an_unknown_pane_fails() {
        let mut t = Node::leaf(1);
        assert!(!t.split(99, Axis::Horizontal, 2));
    }

    #[test]
    fn removing_a_pane_collapses_into_its_sibling() {
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2);
        assert!(t.remove(1));
        let out = layout_of(&t);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], (2, FULL), "the survivor should reclaim the space");
    }

    #[test]
    fn removing_one_of_three_leaves_the_rest_tiling() {
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2);
        t.split(2, Axis::Horizontal, 3);
        assert!(t.remove(2));
        let out = layout_of(&t);
        assert_eq!(out.len(), 2);
        assert_even(&[out[0].1.w, out[1].1.w]);
        let last = out.last().unwrap().1;
        assert!((last.x + last.w - FULL.w).abs() < 0.01);
    }

    #[test]
    fn removing_a_deeply_nested_pane_works() {
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2);
        t.split(2, Axis::Vertical, 3);
        assert!(t.remove(3));
        let mut leaves = Vec::new();
        t.leaves(&mut leaves);
        assert_eq!(leaves, vec![1, 2]);
    }

    #[test]
    fn removing_the_last_pane_fails() {
        let mut t = Node::leaf(1);
        assert!(!t.remove(1), "the root leaf has no sibling to collapse into");
    }

    #[test]
    fn resize_moves_the_boundary_and_clamps() {
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2);
        let before = widths(&t)[0];
        assert!(t.resize(1, Axis::Horizontal, 0.1));
        assert!(widths(&t)[0] > before, "growing pane 1 should widen it");

        // The clamp means repeated shrinks never invert or zero out a pane.
        for _ in 0..100 {
            t.resize(1, Axis::Horizontal, -0.1);
        }
        let w = widths(&t);
        assert!(w[0] > 0.0 && w[1] > 0.0, "got {w:?}");
    }

    #[test]
    fn resize_of_the_last_pane_borrows_from_its_left() {
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2);
        t.split(2, Axis::Horizontal, 3);
        let before = widths(&t);
        assert!(t.resize(3, Axis::Horizontal, 0.1));
        let after = widths(&t);
        assert!(after[2] > before[2], "the last pane should grow");
        assert!(after[1] < before[1], "its left neighbour should shrink");
        assert!((after[0] - before[0]).abs() < 2.0, "distant panes stay put");
    }

    #[test]
    fn resize_ignores_the_wrong_axis() {
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2);
        assert!(!t.resize(1, Axis::Vertical, 0.1));
    }

    #[test]
    fn neighbor_finds_the_pane_in_each_direction() {
        // 1 | 2  with 2 stacked over 3 on the right half.
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2);
        t.split(2, Axis::Vertical, 3);
        let rects = layout_of(&t);

        assert_eq!(neighbor(&rects, 1, 1.0, 0.0), Some(2), "right of 1 is the top-right pane");
        assert_eq!(neighbor(&rects, 2, 0.0, 1.0), Some(3), "below 2 is 3");
        assert_eq!(neighbor(&rects, 3, 0.0, -1.0), Some(2), "above 3 is 2");
        assert_eq!(neighbor(&rects, 2, -1.0, 0.0), Some(1), "left of 2 is 1");
        assert_eq!(neighbor(&rects, 1, -1.0, 0.0), None, "nothing to the left of 1");
    }

    #[test]
    fn contains_matches_the_half_open_rect() {
        let r = Rect { x: 10.0, y: 10.0, w: 100.0, h: 50.0 };
        assert!(r.contains(10.0, 10.0));
        assert!(r.contains(109.9, 59.9));
        assert!(!r.contains(110.0, 30.0));
        assert!(!r.contains(9.9, 30.0));
    }

    #[test]
    fn dividers_sit_between_panes() {
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2);
        t.split(2, Axis::Horizontal, 3);
        let mut d = Vec::new();
        t.dividers(FULL, &mut d);
        assert_eq!(d.len(), 2, "three panes in a row need two gutters");
        let rects = layout_of(&t);
        for (i, div) in d.iter().enumerate() {
            let (a, b) = (rects[i].1, rects[i + 1].1);
            assert!(div.x >= a.x + a.w - 0.01 && div.x + div.w <= b.x + 0.01);
            assert_eq!(div.h, FULL.h);
        }
    }

    #[test]
    fn a_collapse_does_not_reintroduce_same_axis_nesting() {
        // Reported in review: "split right, split down, split right, close one,
        // split right" used to nest an H group inside the outer H group, so the
        // nested pair counted as one member and sizing went back to 50/25/25.
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2); // H[1,2]
        t.split(2, Axis::Vertical, 3); // H[1, V[2,3]]
        t.split(2, Axis::Horizontal, 4); // H[1, V[H[2,4], 3]]
        t.remove(3); // V collapses; the H must not stay nested
        t.split(1, Axis::Horizontal, 5);

        let w = widths(&t);
        assert_eq!(w.len(), 4);
        assert_even(&w);
    }

    #[test]
    fn collapsing_keeps_panes_where_they_were_on_screen() {
        // Flattening must not shuffle sizes: pane 1 still owns half the width,
        // and 2 and 4 split the half that the closed pane's column freed up.
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2);
        t.split(2, Axis::Vertical, 3);
        t.split(2, Axis::Horizontal, 4);
        t.remove(3);

        let out = layout_of(&t);
        assert_eq!(out.len(), 3);
        let by = |id: PaneId| out.iter().find(|(i, _)| *i == id).unwrap().1;
        assert!((by(1).w - FULL.w / 2.0).abs() <= 2.0, "pane 1 keeps its half");
        assert_even(&[by(2).w, by(4).w]);
        assert!((by(2).w - FULL.w / 4.0).abs() <= 3.0);
        // Every pane now spans the full height, since the V split is gone.
        for id in [1, 2, 4] {
            assert!((by(id).h - FULL.h).abs() < 0.01, "pane {id} should be full height");
        }
    }

    #[test]
    fn the_tree_stays_flat_through_repeated_collapses() {
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2);
        t.split(2, Axis::Vertical, 3);
        t.split(3, Axis::Vertical, 4);
        t.split(4, Axis::Horizontal, 5);
        t.remove(5);
        t.remove(4);
        t.remove(3);
        // Back to two panes side by side, with nothing nested in between.
        let mut leaves = Vec::new();
        t.leaves(&mut leaves);
        assert_eq!(leaves, vec![1, 2]);
        assert_even(&widths(&t));

        t.split(2, Axis::Horizontal, 6);
        assert_even(&widths(&t));
    }

    #[test]
    fn a_cross_axis_split_is_left_nested() {
        // Flattening must only merge matching axes, never collapse a real
        // vertical split into its horizontal parent.
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2);
        t.split(2, Axis::Vertical, 3);
        t.split(3, Axis::Vertical, 4);
        t.remove(4);
        let out = layout_of(&t);
        assert_eq!(out.len(), 3);
        let by = |id: PaneId| out.iter().find(|(i, _)| *i == id).unwrap().1;
        assert!((by(1).h - FULL.h).abs() < 0.01, "pane 1 spans full height");
        assert!(by(2).h < FULL.h * 0.6, "2 and 3 still share a column");
        assert_even(&[by(2).h, by(3).h]);
    }
}
