//! Where the boxes go. Layout is bottom-up, one container at a time: a
//! container's direct children are ranked only by the edges among *them*, and
//! the finished container becomes a fixed-size node one level up.
//!
//! Ranking every leaf together instead is the obvious shortcut and it does not
//! work. Measured on this repo, a global rank of two expansions in — seventeen
//! nodes — needs ten ranks and 106x57 cells; laid out per container the
//! deepest box is 21x33. The browser learned the same thing the same way
//! (`graph-server/ui/layout.js`, header comment).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use entity_graph::{EntityGraph, EntityId};

use crate::label::Labels;
use crate::view::Picture;
use crate::zoom::{Metrics, Zoom};
use ratatui::layout::Rect;

/// A box spends a row on its own name, plus a border on each side. Unlike the
/// rest of the measurements this does not vary with zoom: a box with no frame
/// is not a box.
const BOX_PAD: u16 = 1;
const LABEL_H: u16 = 1;

#[derive(Debug, Clone)]
pub struct Placed {
    pub id: EntityId,
    pub rect: Rect,
    /// A container drawn around other nodes, as opposed to a cursor leaf.
    pub is_box: bool,
}

/// One edge as it should be drawn: endpoints are the outermost nodes that
/// actually appear, which for an edge into a collapsed box is the box.
#[derive(Debug, Clone, Copy)]
pub struct DrawnEdge {
    pub from: EntityId,
    pub to: EntityId,
}

pub struct Diagram {
    pub nodes: Vec<Placed>,
    pub edges: Vec<DrawnEdge>,
    pub width: u16,
    pub height: u16,
    /// What the nodes were sized for. Carried so the renderer cannot fill a
    /// box that was measured for one zoom using the text of another.
    pub zoom: Zoom,
}

impl Diagram {
    pub fn extent(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// The leaf drawn at a diagram cell. Boxes are deliberately not hit: a box
    /// is an ancestor of whatever is being pointed at, and the selection has
    /// to stay a leaf for expanding to have something to act on.
    pub fn leaf_at(&self, point: (u16, u16)) -> Option<EntityId> {
        self.nodes
            .iter()
            .find(|n| !n.is_box && n.rect.contains(point.into()))
            .map(|n| n.id)
    }

    pub fn rect_of(&self, id: EntityId) -> Option<Rect> {
        self.nodes.iter().find(|n| n.id == id).map(|n| n.rect)
    }
}

/// Bounded on both sides: nothing is narrower than a few cells, and no label
/// sets the width of the thing holding it. Minified sources really do produce
/// identifiers tens of thousands of characters long, and one of them would
/// otherwise size a box past what the arithmetic below can hold.
fn width_of(text: &str, m: &Metrics) -> u16 {
    unicode_width::UnicodeWidthStr::width(text).clamp(6, m.label_max) as u16
}

/// Display width of what a node has to fit, bounded so one long name cannot
/// set the width of a whole rank. A leaf is sized by whichever of its two
/// lines is wider -- sizing it by the name alone truncates `function · 10–30`
/// on every short-named function there is.
fn label_width(labels: &Labels, id: EntityId, is_box: bool, m: &Metrics) -> u16 {
    if is_box {
        width_of(&labels.head(id, m.box_size), m)
    } else if m.detail {
        width_of(labels.name(id), m).max(width_of(&labels.detail(id), m))
    } else {
        width_of(labels.name(id), m)
    }
}

/// Wide enough for its label and for one port per edge touching it. Ports are
/// spread along the border, so a node narrower than its own degree has to reuse
/// a coordinate -- and a reused port is an exclusive cell, so the second edge
/// to claim it simply never routes.
///
/// The `+ 2` is a border allowance that an unframed leaf does not need, but it
/// is also exactly what `render`'s port slots divide by, so dropping it for
/// the coarsest zoom would quietly cost high-degree nodes their edges.
fn leaf_size(labels: &Labels, id: EntityId, degree: u16, m: &Metrics) -> (u16, u16) {
    (label_width(labels, id, false, m).max(degree) + 2, m.leaf_h)
}

/// Ancestor chain from the outermost container down to `id` itself.
fn lineage(graph: &EntityGraph, id: EntityId) -> Vec<EntityId> {
    let mut chain = vec![id];
    let mut cur = id;
    while let Some(p) = graph.get(cur).and_then(|e| e.parent) {
        chain.push(p);
        cur = p;
    }
    chain.reverse();
    chain
}

/// Back edges by DFS colouring. A code graph is always cyclic, so ranking has
/// to be told which edges to ignore rather than assuming a DAG.
fn back_edges(n: usize, edges: &[(usize, usize)]) -> HashSet<usize> {
    let mut out: HashMap<usize, Vec<(usize, usize)>> = HashMap::new();
    for (i, &(a, b)) in edges.iter().enumerate() {
        out.entry(a).or_default().push((b, i));
    }
    let (mut colour, mut back) = (vec![0u8; n], HashSet::new());
    for start in 0..n {
        if colour[start] != 0 {
            continue;
        }
        colour[start] = 1;
        let mut stack = vec![(start, 0usize)];
        while let Some(&mut (v, ref mut i)) = stack.last_mut() {
            let empty = Vec::new();
            let adj = out.get(&v).unwrap_or(&empty);
            if *i < adj.len() {
                let (w, ei) = adj[*i];
                *i += 1;
                match colour[w] {
                    1 => {
                        back.insert(ei);
                    }
                    0 => {
                        colour[w] = 1;
                        stack.push((w, 0));
                    }
                    _ => {}
                }
            } else {
                colour[v] = 2;
                stack.pop();
            }
        }
    }
    back
}

/// Longest-path layering, then a median-heuristic sweep to cut crossings.
fn layers(n: usize, edges: &[(usize, usize)]) -> Vec<Vec<usize>> {
    let back = back_edges(n, edges);
    let forward: Vec<(usize, usize)> = edges
        .iter()
        .enumerate()
        .filter(|(i, (a, b))| !back.contains(i) && a != b)
        .map(|(_, &e)| e)
        .collect();

    let mut indeg = vec![0usize; n];
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(a, b) in &forward {
        out[a].push(b);
        indeg[b] += 1;
    }
    let mut queue: Vec<usize> = (0..n).filter(|&v| indeg[v] == 0).collect();
    let mut rank = vec![0usize; n];
    while let Some(v) = queue.pop() {
        for &w in &out[v] {
            rank[w] = rank[w].max(rank[v] + 1);
            indeg[w] -= 1;
            if indeg[w] == 0 {
                queue.push(w);
            }
        }
    }

    let depth = rank.iter().copied().max().map_or(0, |m| m + 1);
    let mut levels: Vec<Vec<usize>> = vec![Vec::new(); depth];
    for v in 0..n {
        levels[rank[v]].push(v);
    }
    let mut up: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(a, b) in &forward {
        up[b].push(a);
    }
    for _ in 0..4 {
        for li in 1..depth {
            let prev: HashMap<usize, usize> =
                levels[li - 1].iter().enumerate().map(|(i, &v)| (v, i)).collect();
            let mut keyed: Vec<(f64, usize)> = levels[li]
                .iter()
                .map(|&v| {
                    let mut ps: Vec<usize> =
                        up[v].iter().filter_map(|u| prev.get(u).copied()).collect();
                    ps.sort_unstable();
                    let m = if ps.is_empty() { f64::MAX } else { ps[ps.len() / 2] as f64 };
                    (m, v)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
            levels[li] = keyed.into_iter().map(|(_, v)| v).collect();
        }
    }
    levels
}

/// Children of one container, already sized, packed into ranks. Ranks wrap
/// when they would exceed `max_w`: layering puts every edge between different
/// ranks, so nodes inside one rank never link to each other and wrapping one
/// cannot cross an edge.
fn pack(
    sizes: &[(u16, u16)],
    edges: &[(usize, usize)],
    max_w: u16,
    m: &Metrics,
) -> (Vec<(u16, u16)>, u16, u16) {
    let mut pos = vec![(0u16, 0u16); sizes.len()];
    let (mut y, mut widest) = (0u16, 0u16);
    for level in layers(sizes.len(), edges) {
        let (mut x, mut tallest) = (0u16, 0u16);
        for &v in &level {
            let (w, h) = sizes[v];
            if x > 0 && x.saturating_add(w) > max_w {
                y = y.saturating_add(tallest).saturating_add(1);
                x = 0;
                tallest = 0;
            }
            pos[v] = (x, y);
            x = x.saturating_add(w).saturating_add(m.gap_x);
            tallest = tallest.max(h);
            widest = widest.max(x.saturating_sub(m.gap_x));
        }
        y = y.saturating_add(tallest).saturating_add(m.gap_y);
    }
    (pos, widest, y.saturating_sub(m.gap_y))
}

struct Ctx<'a, 'g> {
    labels: &'a Labels<'g>,
    m: Metrics,
    /// Direct children of each container that are actually in view.
    kids: BTreeMap<EntityId, Vec<EntityId>>,
    /// Edge ends touching each drawn node, which sets how many ports it needs.
    degree: BTreeMap<EntityId, u16>,
    /// Lifted edges among the direct children of each container.
    inner: BTreeMap<EntityId, Vec<(EntityId, EntityId)>>,
}

impl Ctx<'_, '_> {
    /// Size a node and everything under it, returning its own extent. Children
    /// are positioned relative to the node; `emit` shifts them absolute.
    fn size(
        &self,
        id: EntityId,
        avail: u16,
        out: &mut HashMap<EntityId, (u16, u16)>,
    ) -> (u16, u16) {
        let deg = self.degree.get(&id).copied().unwrap_or(0);
        let Some(kids) = self.kids.get(&id) else {
            let s = leaf_size(self.labels, id, deg, &self.m);
            out.insert(id, s);
            return s;
        };
        // Each nesting level spends a border and a pad on each side, so the
        // room left for children shrinks with depth. Passing the *same* bound
        // down at every level instead lets a box hand back its own width plus
        // four, and the total grows past the viewport without ever tripping
        // the wrap test.
        let inner_avail = avail.saturating_sub(2 * (BOX_PAD + 1)).max(self.m.min_inner_w);
        for &k in kids {
            self.size(k, inner_avail, out);
        }
        let sizes: Vec<(u16, u16)> = kids.iter().map(|k| out[k]).collect();
        let idx: HashMap<EntityId, usize> =
            kids.iter().enumerate().map(|(i, &k)| (k, i)).collect();
        let edges: Vec<(usize, usize)> = self
            .inner
            .get(&id)
            .map(|es| es.iter().filter_map(|(a, b)| Some((*idx.get(a)?, *idx.get(b)?))).collect())
            .unwrap_or_default();
        let (_, w, h) = pack(&sizes, &edges, inner_avail, &self.m);
        let label_w = label_width(self.labels, id, true, &self.m).max(deg) + 2;
        // Never wider than the room its parent had to give. Without this the
        // `MIN_INNER_W` floor leaks: once the available width bottoms out,
        // every further level still wraps its child in four more columns, and
        // deep nesting walks straight past the viewport -- 40 levels reached
        // 181 columns against a bound of 100. A box that cannot fit its
        // contents is the honest outcome; growing the diagram is not.
        let size = (
            w.max(label_w).saturating_add(2 * (BOX_PAD + 1)).min(avail.max(self.m.min_inner_w)),
            h.saturating_add(2 * BOX_PAD + LABEL_H + 1),
        );
        out.insert(id, size);
        size
    }

    fn emit(
        &self,
        id: EntityId,
        origin: (u16, u16),
        avail: u16,
        sizes: &HashMap<EntityId, (u16, u16)>,
        out: &mut Vec<Placed>,
    ) {
        let (w, h) = sizes[&id];
        let rect = Rect { x: origin.0, y: origin.1, width: w, height: h };
        let Some(kids) = self.kids.get(&id) else {
            out.push(Placed { id, rect, is_box: false });
            return;
        };
        out.push(Placed { id, rect, is_box: true });
        let child_sizes: Vec<(u16, u16)> = kids.iter().map(|k| sizes[k]).collect();
        let idx: HashMap<EntityId, usize> =
            kids.iter().enumerate().map(|(i, &k)| (k, i)).collect();
        let edges: Vec<(usize, usize)> = self
            .inner
            .get(&id)
            .map(|es| es.iter().filter_map(|(a, b)| Some((*idx.get(a)?, *idx.get(b)?))).collect())
            .unwrap_or_default();
        let inner_avail = avail.saturating_sub(2 * (BOX_PAD + 1)).max(self.m.min_inner_w);
        let (pos, _, _) = pack(&child_sizes, &edges, inner_avail, &self.m);
        let base = (origin.0 + BOX_PAD + 1, origin.1 + BOX_PAD + LABEL_H);
        for (i, &k) in kids.iter().enumerate() {
            self.emit(k, (base.0 + pos[i].0, base.1 + pos[i].1), inner_avail, sizes, out);
        }
    }
}

/// Lay the picture out at one zoom level. `max_w` bounds a rank before it
/// wraps.
pub fn place(labels: &Labels, picture: &Picture, max_w: u16, zoom: Zoom) -> Diagram {
    let graph = labels.graph;
    let leaves: BTreeSet<EntityId> = picture.nodes.iter().copied().collect();
    let chains: BTreeMap<EntityId, Vec<EntityId>> =
        leaves.iter().map(|&l| (l, lineage(graph, l))).collect();

    // Containers worth drawing: every ancestor of a visible leaf. A chain of
    // single-child containers is kept, not collapsed -- the folder spine is
    // how a reader locates a box, and hiding it was what made the tree view
    // hard to navigate.
    let mut kids: BTreeMap<EntityId, Vec<EntityId>> = BTreeMap::new();
    for chain in chains.values() {
        for pair in chain.windows(2) {
            let entry = kids.entry(pair[0]).or_default();
            if !entry.contains(&pair[1]) {
                entry.push(pair[1]);
            }
        }
    }
    for l in &leaves {
        kids.remove(l);
    }

    // An edge is drawn between the two outermost nodes that differ: inside a
    // box it belongs to that box's own level, and is ranked there.
    let mut inner: BTreeMap<EntityId, Vec<(EntityId, EntityId)>> = BTreeMap::new();
    let mut drawn: Vec<DrawnEdge> = Vec::new();
    let mut seen: BTreeSet<(EntityId, EntityId)> = BTreeSet::new();
    for e in &picture.edges {
        let (Some(fa), Some(ta)) = (chains.get(&e.from), chains.get(&e.to)) else { continue };
        let split = fa.iter().zip(ta).take_while(|(a, b)| a == b).count();
        if split >= fa.len() || split >= ta.len() {
            continue; // one endpoint contains the other; there is no pair to draw
        }
        if split == 0 {
            // Different root trees. They sit side by side, so the edge is
            // drawable between the roots even though no container holds both.
            let (a, b) = (fa[0], ta[0]);
            if a != b && seen.insert((a, b)) {
                drawn.push(DrawnEdge { from: a, to: b });
            }
            continue;
        }
        let (a, b) = (fa[split], ta[split]);
        if a == b || !seen.insert((a, b)) {
            continue;
        }
        inner.entry(fa[split - 1]).or_default().push((a, b));
        drawn.push(DrawnEdge { from: a, to: b });
    }

    let roots: Vec<EntityId> =
        chains.values().map(|c| c[0]).collect::<BTreeSet<_>>().into_iter().collect();
    let mut degree: BTreeMap<EntityId, u16> = BTreeMap::new();
    for e in &drawn {
        *degree.entry(e.from).or_default() += 1;
        *degree.entry(e.to).or_default() += 1;
    }
    let ctx = Ctx { labels, m: zoom.metrics(), kids, inner, degree };
    let mut sizes = HashMap::new();
    for &r in &roots {
        ctx.size(r, max_w, &mut sizes);
    }
    let mut nodes = Vec::new();
    let (mut x, mut h) = (0u16, 0u16);
    for &r in &roots {
        ctx.emit(r, (x, 0), max_w, &sizes, &mut nodes);
        x += sizes[&r].0 + ctx.m.gap_x;
        h = h.max(sizes[&r].1);
    }
    Diagram { nodes, edges: drawn, width: x.saturating_sub(ctx.m.gap_x), height: h, zoom }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zoom::Zoom;
    use entity_graph::EntityKind::{File, Folder};
    use entity_graph::ReferenceKind::Call;
    use entity_graph::test_support::graph_from_parents;

    /// Expand all the way, which for these fixtures puts every file in view.
    fn fully_expanded(graph: &EntityGraph) -> Picture {
        let mut cursor = coalesce::Cursor::new(graph);
        loop {
            let mut moved = false;
            for leaf in cursor.coalesced().leaves {
                moved |= cursor.move_down(leaf, graph);
            }
            if !moved {
                return crate::view::apply(graph, &cursor.coalesced(), &Default::default());
            }
        }
    }

    fn rect_of(d: &Diagram, graph: &EntityGraph, name: &str) -> Rect {
        let id = graph.entities.iter().find(|e| e.name == name).expect("no such entity").id;
        d.nodes.iter().find(|n| n.id == id).expect("entity not placed").rect
    }

    fn contains(outer: Rect, inner: Rect) -> bool {
        inner.x > outer.x
            && inner.y > outer.y
            && inner.right() < outer.right()
            && inner.bottom() < outer.bottom()
    }

    #[test]
    fn every_file_is_drawn_strictly_inside_the_folder_that_holds_it() {
        let graph = graph_from_parents(
            &[
                ("root", Folder, None),
                ("a.rs", File, Some(0)),
                ("b.rs", File, Some(0)),
                ("c.rs", File, Some(0)),
            ],
            &[],
        );
        let d = place(&Labels::new(&graph), &fully_expanded(&graph), 200, Zoom::Close);
        let folder = rect_of(&d, &graph, "root");
        assert!(d.nodes.iter().find(|n| n.id.0 == 0).unwrap().is_box);
        for file in ["a.rs", "b.rs", "c.rs"] {
            assert!(contains(folder, rect_of(&d, &graph, file)), "{file} escaped its folder");
        }
    }

    #[test]
    fn a_reference_puts_its_target_below_its_source() {
        let graph = graph_from_parents(
            &[("root", Folder, None), ("caller.rs", File, Some(0)), ("callee.rs", File, Some(0))],
            &[(1, 2, Call)],
        );
        let d = place(&Labels::new(&graph), &fully_expanded(&graph), 200, Zoom::Close);
        let caller = rect_of(&d, &graph, "caller.rs");
        let callee = rect_of(&d, &graph, "callee.rs");
        assert!(caller.y < callee.y, "caller {caller:?} should rank above callee {callee:?}");
        assert_eq!(d.edges.len(), 1);
    }

    /// Ranking assumes a DAG and a code graph is never one, so the interesting
    /// case is that mutual references still terminate and still place both
    /// nodes rather than dropping one.
    #[test]
    fn mutually_referencing_files_are_both_placed() {
        let graph = graph_from_parents(
            &[("root", Folder, None), ("one.rs", File, Some(0)), ("two.rs", File, Some(0))],
            &[(1, 2, Call), (2, 1, Call)],
        );
        let d = place(&Labels::new(&graph), &fully_expanded(&graph), 200, Zoom::Close);
        assert_ne!(rect_of(&d, &graph, "one.rs"), rect_of(&d, &graph, "two.rs"));
    }

    /// Sibling order is the tie-break in ranking, which drives wrapping and so
    /// every box size. Iterating a `HashMap` to establish it made the whole
    /// layout vary run to run -- and between two calls in one process, which
    /// is what this pins.
    #[test]
    fn the_same_graph_lays_out_identically_every_time() {
        let names: Vec<String> = (0..10).map(|i| format!("mod_{i:02}.rs")).collect();
        let mut rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> =
            vec![("root", Folder, None), ("inner", Folder, Some(0))];
        rows.extend(names.iter().map(|n| (n.as_str(), File, Some(1))));
        let refs: Vec<(usize, usize, entity_graph::ReferenceKind)> =
            (2..11).map(|i| (i, i + 1, Call)).collect();
        let graph = graph_from_parents(&rows, &refs);
        let coalesced = fully_expanded(&graph);

        let first = place(&Labels::new(&graph), &coalesced, 80, Zoom::Close);
        for _ in 0..5 {
            let again = place(&Labels::new(&graph), &coalesced, 80, Zoom::Close);
            assert_eq!(first.width, again.width);
            assert_eq!(first.height, again.height);
            let a: Vec<_> = first.nodes.iter().map(|n| (n.id, n.rect)).collect();
            let b: Vec<_> = again.nodes.iter().map(|n| (n.id, n.rect)).collect();
            assert_eq!(a, b, "layout differs between calls");
        }
    }

    /// Every nesting level spends four columns on borders and padding. Handing
    /// the same bound down at each level let a box return its own width plus
    /// four, so the total crept past the viewport however deep it went.
    #[test]
    fn nesting_does_not_grow_the_diagram_past_the_viewport() {
        for levels in [3usize, 10, 20, 40] {
            let names: Vec<String> = (0..levels).map(|i| format!("lvl{i}")).collect();
            let mut rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> =
                vec![("root", Folder, None)];
            for (i, n) in names.iter().enumerate() {
                rows.push((n.as_str(), Folder, Some(i)));
            }
            let files: Vec<String> = (0..6).map(|i| format!("some_file_{i:02}.rs")).collect();
            rows.extend(files.iter().map(|n| (n.as_str(), File, Some(levels))));
            let graph = graph_from_parents(&rows, &[]);
            let d = place(&Labels::new(&graph), &fully_expanded(&graph), 100, Zoom::Close);
            assert!(d.width <= 100, "{levels} levels of nesting reached {} columns", d.width);
        }
    }

    #[test]
    fn a_wide_subtree_still_fits_the_viewport() {
        let mut rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> =
            vec![("root", Folder, None)];
        let deep: Vec<String> = (0..6).map(|i| format!("lvl{i}")).collect();
        for (i, name) in deep.iter().enumerate() {
            rows.push((name.as_str(), Folder, Some(i)));
        }
        let files: Vec<String> = (0..8).map(|i| format!("wide_file_name_{i:02}.rs")).collect();
        rows.extend(files.iter().map(|n| (n.as_str(), File, Some(deep.len()))));
        let graph = graph_from_parents(&rows, &[]);
        let d = place(&Labels::new(&graph), &fully_expanded(&graph), 100, Zoom::Close);
        assert!(d.width <= 100, "six levels of nesting reached {} columns", d.width);
    }

    /// Minified vendor sources really do carry identifiers tens of thousands
    /// of characters long. Before the clamp, one of them sized a box to 30346
    /// columns and the next zoom overflowed the layout arithmetic outright.
    #[test]
    fn an_absurdly_long_name_does_not_size_the_thing_that_holds_it() {
        let huge = "x".repeat(30_324);
        let graph = graph_from_parents(
            &[
                ("root", Folder, None),
                ("inner", Folder, Some(0)),
                (huge.as_str(), File, Some(1)),
                ("ordinary.rs", File, Some(1)),
            ],
            &[],
        );
        let d = place(&Labels::new(&graph), &fully_expanded(&graph), 120, Zoom::Close);
        assert!(d.width <= 120, "one long label widened the diagram to {}", d.width);
    }

    #[test]
    fn a_rank_too_wide_for_the_viewport_wraps_onto_another_row() {
        let names: Vec<String> = (0..12).map(|i| format!("file_{i:02}.rs")).collect();
        let mut rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> =
            vec![("root", Folder, None)];
        rows.extend(names.iter().map(|n| (n.as_str(), File, Some(0))));
        let graph = graph_from_parents(&rows, &[]);

        let narrow = place(&Labels::new(&graph), &fully_expanded(&graph), 60, Zoom::Close);
        assert!(narrow.width <= 60, "wrapped layout is {} wide", narrow.width);
        let rows_used: std::collections::HashSet<u16> =
            narrow.nodes.iter().filter(|n| !n.is_box).map(|n| n.rect.y).collect();
        assert!(rows_used.len() > 1, "a 12-node rank should not fit one 60-column row");

        // The same graph with room to spare keeps them on one row.
        let wide = place(&Labels::new(&graph), &fully_expanded(&graph), 400, Zoom::Close);
        let wide_rows: std::collections::HashSet<u16> =
            wide.nodes.iter().filter(|n| !n.is_box).map(|n| n.rect.y).collect();
        assert_eq!(wide_rows.len(), 1);
    }
}
