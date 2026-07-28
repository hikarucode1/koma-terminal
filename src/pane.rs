//! Binary split tree describing how panes tile the window — the same shape
//! tmux/wezterm use. Leaves hold pane ids; layout turns the tree into rects.

pub type PaneId = u64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Axis {
    /// Children sit side by side; the divider between them is vertical.
    Horizontal,
    /// Children are stacked; the divider between them is horizontal.
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

pub enum Node {
    Leaf(PaneId),
    Split { axis: Axis, ratio: f32, a: Box<Node>, b: Box<Node> },
}

/// Half-width of the gutter drawn between two panes, in physical pixels.
const HALF_GAP: f32 = 1.0;

impl Node {
    pub fn leaf(id: PaneId) -> Self {
        Node::Leaf(id)
    }

    /// Flattens the tree into `(pane, rect)` pairs covering `rect`.
    pub fn layout(&self, rect: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Node::Leaf(id) => out.push((*id, rect)),
            Node::Split { axis, ratio, a, b } => {
                let (ra, rb) = split_rect(rect, *axis, *ratio);
                a.layout(ra, out);
                b.layout(rb, out);
            }
        }
    }

    /// Rects of the dividers themselves, for drawing the gutter lines.
    pub fn dividers(&self, rect: Rect, out: &mut Vec<Rect>) {
        if let Node::Split { axis, ratio, a, b } = self {
            let (ra, rb) = split_rect(rect, *axis, *ratio);
            match axis {
                Axis::Horizontal => out.push(Rect {
                    x: ra.x + ra.w,
                    y: rect.y,
                    w: (rb.x - (ra.x + ra.w)).max(1.0),
                    h: rect.h,
                }),
                Axis::Vertical => out.push(Rect {
                    x: rect.x,
                    y: ra.y + ra.h,
                    w: rect.w,
                    h: (rb.y - (ra.y + ra.h)).max(1.0),
                }),
            }
            a.dividers(ra, out);
            b.dividers(rb, out);
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

    /// Replaces `target`'s leaf with a split holding `target` and `new_id`.
    /// Returns false if `target` isn't in the tree.
    pub fn split(&mut self, target: PaneId, axis: Axis, new_id: PaneId) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                *self = Node::Split {
                    axis,
                    ratio: 0.5,
                    a: Box::new(Node::Leaf(target)),
                    b: Box::new(Node::Leaf(new_id)),
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => {
                a.split(target, axis, new_id) || b.split(target, axis, new_id)
            }
        }
    }

    /// Removes `target`, collapsing its parent split into the sibling.
    /// Returns false if `target` is the root leaf (nothing left to collapse into).
    pub fn remove(&mut self, target: PaneId) -> bool {
        match self {
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => {
                // If either child *is* the target leaf, replace self with the sibling.
                if matches!(**a, Node::Leaf(id) if id == target) {
                    let sibling = std::mem::replace(&mut **b, Node::Leaf(target));
                    *self = sibling;
                    return true;
                }
                if matches!(**b, Node::Leaf(id) if id == target) {
                    let sibling = std::mem::replace(&mut **a, Node::Leaf(target));
                    *self = sibling;
                    return true;
                }
                a.remove(target) || b.remove(target)
            }
        }
    }

    /// Nudges the ratio of the nearest ancestor split of `axis` containing
    /// `target`. `delta` is a fraction of that split's extent.
    pub fn resize(&mut self, target: PaneId, axis: Axis, delta: f32) -> bool {
        let Node::Split { axis: my_axis, ratio, a, b } = self else {
            return false;
        };
        // Let a descendant split claim it first, so the innermost one wins.
        if a.resize(target, axis, delta) || b.resize(target, axis, delta) {
            return true;
        }
        if *my_axis != axis {
            return false;
        }
        let mut in_a = Vec::new();
        a.leaves(&mut in_a);
        let mut in_b = Vec::new();
        b.leaves(&mut in_b);
        if in_a.contains(&target) {
            *ratio = (*ratio + delta).clamp(0.05, 0.95);
            true
        } else if in_b.contains(&target) {
            *ratio = (*ratio - delta).clamp(0.05, 0.95);
            true
        } else {
            false
        }
    }
}

fn split_rect(r: Rect, axis: Axis, ratio: f32) -> (Rect, Rect) {
    match axis {
        Axis::Horizontal => {
            let split_at = (r.w * ratio).round();
            let a = Rect { x: r.x, y: r.y, w: (split_at - HALF_GAP).max(0.0), h: r.h };
            let bx = r.x + split_at + HALF_GAP;
            let b = Rect { x: bx, y: r.y, w: (r.x + r.w - bx).max(0.0), h: r.h };
            (a, b)
        }
        Axis::Vertical => {
            let split_at = (r.h * ratio).round();
            let a = Rect { x: r.x, y: r.y, w: r.w, h: (split_at - HALF_GAP).max(0.0) };
            let by = r.y + split_at + HALF_GAP;
            let b = Rect { x: r.x, y: by, w: r.w, h: (r.y + r.h - by).max(0.0) };
            (a, b)
        }
    }
}

/// Picks the pane nearest to `from` in the given direction, comparing centres.
/// `dx`/`dy` is the unit direction, e.g. `(-1, 0)` for "focus left".
pub fn neighbor(
    rects: &[(PaneId, Rect)],
    from: PaneId,
    dx: f32,
    dy: f32,
) -> Option<PaneId> {
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
            (a.2 * 4.0 + a.1)
                .partial_cmp(&(b.2 * 4.0 + b.1))
                .unwrap_or(std::cmp::Ordering::Equal)
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
        // The two panes plus the gutter must span the full width exactly.
        assert!((b.x + b.w - FULL.w).abs() < 0.01);
        assert!(b.x > a.x + a.w, "there must be a gutter between panes");
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
    }

    #[test]
    fn nested_splits_produce_one_rect_per_leaf() {
        let mut t = Node::leaf(1);
        t.split(1, Axis::Horizontal, 2);
        t.split(2, Axis::Vertical, 3);
        let out = layout_of(&t);
        assert_eq!(out.len(), 3);
        let mut leaves = Vec::new();
        t.leaves(&mut leaves);
        assert_eq!(leaves, vec![1, 2, 3]);
        // Nothing may overlap.
        for i in 0..out.len() {
            for j in (i + 1)..out.len() {
                let (a, b) = (out[i].1, out[j].1);
                let overlaps = a.x < b.x + b.w
                    && b.x < a.x + a.w
                    && a.y < b.y + b.h
                    && b.y < a.y + a.h;
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
        let before = layout_of(&t)[0].1.w;
        assert!(t.resize(1, Axis::Horizontal, 0.1));
        let after = layout_of(&t)[0].1.w;
        assert!(after > before, "growing pane 1 should widen it");

        // The ratio is clamped, so repeated shrinks never invert the layout.
        for _ in 0..100 {
            t.resize(1, Axis::Horizontal, -0.1);
        }
        let out = layout_of(&t);
        assert!(out[0].1.w > 0.0 && out[1].1.w > 0.0);
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
        let mut d = Vec::new();
        t.dividers(FULL, &mut d);
        assert_eq!(d.len(), 1);
        let rects = layout_of(&t);
        let (a, b) = (rects[0].1, rects[1].1);
        assert!(d[0].x >= a.x + a.w - 0.01 && d[0].x + d[0].w <= b.x + 0.01);
        assert_eq!(d[0].h, FULL.h);
    }
}
