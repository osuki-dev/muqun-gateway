//! Split view's tiling, after Hyprland's "dwindle" layout: a binary tree
//! whose leaves are panes. A new pane splits the focused one along its
//! longer side and takes the second half, so panes spiral in as they are
//! added; closing a pane gives its room back to its sibling. Each split
//! keeps its own ratio, moved by dragging the gap between its halves.

use rune::Key;

use crate::config::TileNode;

/// The least share of a split either half keeps when dragged.
pub const MIN_RATIO: f32 = 0.15;

/// A rectangle: x, y, width, height.
pub type Rect = [f32; 4];

impl TileNode {
    /// The panes it holds, first to last.
    pub fn leaves(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<usize>) {
        match self {
            Self::Leaf(i) => out.push(*i),
            Self::Split { a, b, .. } => {
                a.collect(out);
                b.collect(out);
            }
        }
    }

    /// It tiles exactly panes `0..n`, once each.
    pub fn fits(&self, n: usize) -> bool {
        let mut leaves = self.leaves();
        leaves.sort_unstable();
        leaves.len() == n && leaves.iter().enumerate().all(|(i, &j)| i == j)
    }

    /// The split at `path` (false: first half, true: second).
    pub fn at_mut(&mut self, path: &[bool]) -> Option<&mut TileNode> {
        let Some((&first, rest)) = path.split_first() else {
            return Some(self);
        };
        match self {
            Self::Leaf(_) => None,
            Self::Split { a, b, .. } => if first { b } else { a }.at_mut(rest),
        }
    }
}

/// `r` cut in two: side by side (`across`) or stacked, the first `ratio`
/// of it.
pub fn cut(r: Rect, across: bool, ratio: f32) -> (Rect, Rect) {
    let [x, y, w, h] = r;
    if across {
        let wa = w * ratio;
        ([x, y, wa, h], [x + wa, y, w - wa, h])
    } else {
        let ha = h * ratio;
        ([x, y, w, ha], [x, y + ha, w, h - ha])
    }
}

/// Where each pane sits when the tree fills `r`.
pub fn rects(node: &TileNode, r: Rect) -> Vec<(usize, Rect)> {
    let mut out = Vec::new();
    place(node, r, &mut out);
    out
}

fn place(node: &TileNode, r: Rect, out: &mut Vec<(usize, Rect)>) {
    match node {
        TileNode::Leaf(i) => out.push((*i, r)),
        TileNode::Split {
            across,
            ratio,
            a,
            b,
        } => {
            let (ra, rb) = cut(r, *across, *ratio);
            place(a, ra, out);
            place(b, rb, out);
        }
    }
}

/// Make room for pane `new` by splitting pane `target` along its longer
/// side when the tree fills `size` (the new pane takes the second half).
pub fn insert(node: &mut TileNode, target: usize, new: usize, size: [f32; 2]) {
    let [w, h] = if size[0] > 0.0 && size[1] > 0.0 {
        size
    } else {
        // Not laid out yet: a landscape window.
        [16.0, 10.0]
    };
    let across = rects(node, [0.0, 0.0, w, h])
        .into_iter()
        .find(|(i, _)| *i == target)
        .is_none_or(|(_, r)| r[2] >= r[3]);
    replace_leaf(node, target, &mut |old| TileNode::Split {
        across,
        ratio: 0.5,
        a: Box::new(old),
        b: Box::new(TileNode::Leaf(new)),
    });
}

fn replace_leaf(node: &mut TileNode, target: usize, f: &mut dyn FnMut(TileNode) -> TileNode) {
    match node {
        TileNode::Leaf(i) if *i == target => {
            let old = std::mem::replace(node, TileNode::Leaf(target));
            *node = f(old);
        }
        TileNode::Leaf(_) => {}
        TileNode::Split { a, b, .. } => {
            replace_leaf(a, target, f);
            replace_leaf(b, target, f);
        }
    }
}

/// The tree with pane `gone` closed: its sibling takes the room, and the
/// panes after it move down one, as the tile list does.
pub fn remove(node: TileNode, gone: usize) -> Option<TileNode> {
    match node {
        TileNode::Leaf(i) if i == gone => None,
        TileNode::Leaf(i) => Some(TileNode::Leaf(if i > gone { i - 1 } else { i })),
        TileNode::Split {
            across,
            ratio,
            a,
            b,
        } => match (remove(*a, gone), remove(*b, gone)) {
            (Some(a), Some(b)) => Some(TileNode::Split {
                across,
                ratio,
                a: Box::new(a),
                b: Box::new(b),
            }),
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        },
    }
}

/// `n` panes tiled as if added one by one, each splitting the last.
pub fn build(n: usize, size: [f32; 2]) -> TileNode {
    let mut tree = TileNode::Leaf(0);
    for i in 1..n {
        insert(&mut tree, i - 1, i, size);
    }
    tree
}

/// The pane `key`-ward of pane `from` as the tree fills `size`: the nearest
/// one wholly past that edge, among those beside it first.
pub fn neighbour(node: &TileNode, size: [f32; 2], from: usize, key: &Key) -> Option<usize> {
    let size = if size[0] > 0.0 { size } else { [16.0, 10.0] };
    let all = rects(node, [0.0, 0.0, size[0], size[1]]);
    let &(_, c) = all.iter().find(|(i, _)| *i == from)?;
    const EPS: f32 = 0.5;
    let overlap = |a0: f32, a1: f32, b0: f32, b1: f32| a1.min(b1) - a0.max(b0);
    all.iter()
        .filter(|(i, _)| *i != from)
        .filter_map(|&(i, r)| {
            let (gap, beside) = match key {
                Key::Right => (
                    r[0] - (c[0] + c[2]),
                    overlap(c[1], c[1] + c[3], r[1], r[1] + r[3]),
                ),
                Key::Left => (
                    c[0] - (r[0] + r[2]),
                    overlap(c[1], c[1] + c[3], r[1], r[1] + r[3]),
                ),
                Key::Down => (
                    r[1] - (c[1] + c[3]),
                    overlap(c[0], c[0] + c[2], r[0], r[0] + r[2]),
                ),
                Key::Up => (
                    c[1] - (r[1] + r[3]),
                    overlap(c[0], c[0] + c[2], r[0], r[0] + r[2]),
                ),
                _ => return None,
            };
            (gap > -EPS).then_some((i, gap, beside))
        })
        // Beside it before diagonal, then nearest, then most shared edge.
        .min_by(|x, y| {
            (x.2 <= EPS)
                .cmp(&(y.2 <= EPS))
                .then(x.1.total_cmp(&y.1))
                .then(y.2.total_cmp(&x.2))
        })
        .map(|(i, ..)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Key::{Down, Left, Right, Up};

    const WIDE: [f32; 2] = [1600.0, 900.0];

    #[test]
    fn panes_spiral_in_along_the_longer_side() {
        // One tall pane on the left; on the right one above two side by side.
        let tree = build(4, WIDE);
        let placed = rects(&tree, [0.0, 0.0, WIDE[0], WIDE[1]]);
        let at = |i: usize| placed.iter().find(|(j, _)| *j == i).unwrap().1;
        assert_eq!(at(0), [0.0, 0.0, 800.0, 900.0]);
        assert_eq!(at(1), [800.0, 0.0, 800.0, 450.0]);
        assert_eq!(at(2), [800.0, 450.0, 400.0, 450.0]);
        assert_eq!(at(3), [1200.0, 450.0, 400.0, 450.0]);
        assert!(tree.fits(4));
    }

    #[test]
    fn a_closed_pane_gives_its_room_to_its_sibling() {
        let mut tree = build(4, WIDE);
        if let Some(TileNode::Split { ratio, .. }) = tree.at_mut(&[true]) {
            *ratio = 0.3;
        }
        // Closing the top-right pane: the bottom pair takes the right column,
        // and the panes after it renumber down.
        let tree = remove(tree, 1).unwrap();
        assert!(tree.fits(3));
        let placed = rects(&tree, [0.0, 0.0, WIDE[0], WIDE[1]]);
        assert_eq!(
            placed,
            vec![
                (0, [0.0, 0.0, 800.0, 900.0]),
                (1, [800.0, 0.0, 400.0, 900.0]),
                (2, [1200.0, 0.0, 400.0, 900.0]),
            ]
        );
        assert_eq!(remove(TileNode::Leaf(0), 0), None);
    }

    #[test]
    fn arrows_move_between_panes_as_they_sit() {
        let tree = build(4, WIDE);
        let cases = [
            (0, Right, Some(1)),
            (0, Left, None),
            (1, Left, Some(0)),
            (1, Down, Some(2)),
            (2, Up, Some(1)),
            (2, Right, Some(3)),
            (3, Left, Some(2)),
            (3, Up, Some(1)),
            (2, Left, Some(0)),
            (3, Down, None),
        ];
        for (from, key, want) in cases {
            assert_eq!(
                neighbour(&tree, WIDE, from, &key),
                want,
                "from {from}, {key:?}"
            );
        }
    }
}
