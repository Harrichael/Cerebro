//! The containment tree of what is drawn, and the edges lifted onto it.
//!
//! A [`crate::view::Picture`] is a flat set of leaves and the edges among
//! them. Everything downstream -- laying out, routing, painting, hit-testing
//! -- needs the same derived facts: which containers hold which drawn
//! children, and between which two *siblings* each edge is actually drawn.
//! Deriving them once here keeps the layout and the renderer from each
//! answering "what is the parent of this box" differently.

use std::collections::{BTreeMap, BTreeSet};

use entity_graph::{EntityGraph, EntityId, ReferenceKind};

use crate::view::Picture;

/// One edge as it should be drawn: between the two outermost nodes that
/// differ, which for an edge into a collapsed box is the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawnEdge {
    pub from: EntityId,
    pub to: EntityId,
    /// The container both ends are direct children of; `None` between roots.
    pub level: Option<EntityId>,
    /// The strongest kind among the references lifted onto this pair.
    pub kind: ReferenceKind,
}

#[derive(Debug, Clone, Default)]
pub struct Scene {
    /// Drawn nodes with no container, in graph order.
    pub roots: Vec<EntityId>,
    /// Direct drawn children of each container, in graph order.
    pub kids: BTreeMap<EntityId, Vec<EntityId>>,
    /// The container each drawn node sits in; roots are absent.
    pub parent: BTreeMap<EntityId, EntityId>,
    /// The one box that held everything, not drawn: the edge of the canvas
    /// is its frame. A frame around the whole picture spends a border and a
    /// title row on saying what the status line can say, and the space it
    /// takes is space the picture cannot have.
    pub canvas: Option<EntityId>,
    pub edges: Vec<DrawnEdge>,
    /// Edge ends touching each drawn node, which sets how many ports it needs.
    pub degree: BTreeMap<EntityId, u16>,
}

/// Lower is stronger; mirrors `view::rank` and the browser's `PRECEDENCE`.
fn strength(kind: ReferenceKind) -> u8 {
    match kind {
        ReferenceKind::Call => 0,
        ReferenceKind::TypeRef => 1,
        ReferenceKind::VarRef => 2,
        ReferenceKind::Import => 3,
        ReferenceKind::Generic => 4,
    }
}

/// Ancestor chain from the outermost container down to `id` itself.
pub fn lineage(graph: &EntityGraph, id: EntityId) -> Vec<EntityId> {
    let mut chain = vec![id];
    let mut cur = id;
    while let Some(p) = graph.get(cur).and_then(|e| e.parent) {
        chain.push(p);
        cur = p;
    }
    chain.reverse();
    chain
}

impl Scene {
    pub fn new(graph: &EntityGraph, picture: &Picture) -> Scene {
        let leaves: BTreeSet<EntityId> = picture.nodes.iter().copied().collect();
        let chains: BTreeMap<EntityId, Vec<EntityId>> =
            leaves.iter().map(|&l| (l, lineage(graph, l))).collect();

        // Containers worth drawing: every ancestor of a visible leaf. A chain
        // of single-child containers is kept, not collapsed -- the folder
        // spine is how a reader locates a box.
        let mut kids: BTreeMap<EntityId, Vec<EntityId>> = BTreeMap::new();
        let mut parent: BTreeMap<EntityId, EntityId> = BTreeMap::new();
        let mut roots: Vec<EntityId> = Vec::new();
        for chain in chains.values() {
            if !roots.contains(&chain[0]) {
                roots.push(chain[0]);
            }
            for pair in chain.windows(2) {
                let entry = kids.entry(pair[0]).or_default();
                if !entry.contains(&pair[1]) {
                    entry.push(pair[1]);
                }
                parent.insert(pair[1], pair[0]);
            }
        }
        for l in &leaves {
            kids.remove(l);
        }
        // Graph order, not discovery order: the order siblings are ranked in
        // is a tie-break that shapes every layout, and it must not depend on
        // which leaf happened to be walked first.
        roots.sort();
        for v in kids.values_mut() {
            v.sort();
        }
        // Only the outermost box goes, never a spine below it: a chain of
        // single-child folders is kept as the way a reader locates a box.
        let canvas = match roots.as_slice() {
            [only] if kids.contains_key(only) => Some(*only),
            _ => None,
        };
        if let Some(c) = canvas {
            roots = kids.remove(&c).unwrap_or_default();
            for r in &roots {
                parent.remove(r);
            }
        }

        // An edge is drawn between the two outermost nodes that differ. Two
        // references lifting onto the same pair are one line, of the kind
        // that says most.
        let mut lifted: BTreeMap<(EntityId, EntityId), DrawnEdge> = BTreeMap::new();
        for e in &picture.edges {
            let (Some(fa), Some(ta)) = (chains.get(&e.from), chains.get(&e.to)) else { continue };
            let split = fa.iter().zip(ta).take_while(|(a, b)| a == b).count();
            if split >= fa.len() || split >= ta.len() {
                continue; // one endpoint contains the other; there is no pair to draw
            }
            let (a, b) = (fa[split], ta[split]);
            let level = (split > 0).then(|| fa[split - 1]).filter(|&l| Some(l) != canvas);
            lifted
                .entry((a, b))
                .and_modify(|d| {
                    if strength(e.kind) < strength(d.kind) {
                        d.kind = e.kind;
                    }
                })
                .or_insert(DrawnEdge { from: a, to: b, level, kind: e.kind });
        }
        let edges: Vec<DrawnEdge> = lifted.into_values().collect();
        let mut degree: BTreeMap<EntityId, u16> = BTreeMap::new();
        for e in &edges {
            *degree.entry(e.from).or_default() += 1;
            *degree.entry(e.to).or_default() += 1;
        }
        Scene { roots, kids, parent, canvas, edges, degree }
    }

    /// The direct children of a level: a container's, or the roots.
    pub fn children(&self, level: Option<EntityId>) -> &[EntityId] {
        match level {
            None => &self.roots,
            Some(c) => self.kids.get(&c).map_or(&[], Vec::as_slice),
        }
    }

    pub fn is_box(&self, id: EntityId) -> bool {
        self.kids.contains_key(&id)
    }

    /// Roots are depth 1, their children 2, and so on.
    pub fn depth(&self, id: EntityId) -> u8 {
        let mut d = 1u8;
        let mut cur = id;
        while let Some(&p) = self.parent.get(&cur) {
            d = d.saturating_add(1);
            cur = p;
        }
        d
    }

    /// Edges drawn among the children of one level.
    pub fn edges_at(&self, level: Option<EntityId>) -> impl Iterator<Item = &DrawnEdge> {
        self.edges.iter().filter(move |e| e.level == level)
    }

    /// Every level there is, deepest first, so a pass that needs a box's
    /// children finished before the box can just walk the list.
    pub fn levels_bottom_up(&self) -> Vec<Option<EntityId>> {
        let mut levels: Vec<(u8, Option<EntityId>)> =
            self.kids.keys().map(|&c| (self.depth(c), Some(c))).collect();
        levels.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        levels.into_iter().map(|(_, l)| l).chain(std::iter::once(None)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entity_graph::EntityKind::{File, Folder};
    use entity_graph::ReferenceKind::{Call, Import};
    use entity_graph::test_support::graph_from_parents;

    fn expanded(graph: &EntityGraph) -> Picture {
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

    /// An edge between files in different folders is drawn between the
    /// folders, at the level that holds both -- and two references lifting
    /// onto the same pair are one edge of the stronger kind. The root that
    /// held both folders is the canvas itself, so the folders are the roots.
    #[test]
    fn edges_are_lifted_to_the_siblings_that_differ_and_merge_there() {
        let graph = graph_from_parents(
            &[
                ("root", Folder, None),
                ("a", Folder, Some(0)),
                ("b", Folder, Some(0)),
                ("a1.rs", File, Some(1)),
                ("a2.rs", File, Some(1)),
                ("b1.rs", File, Some(2)),
            ],
            &[(3, 5, Import), (4, 5, Call), (3, 4, Call)],
        );
        let scene = Scene::new(&graph, &expanded(&graph));
        let (root, a, b, a1, a2) =
            (EntityId(0), EntityId(1), EntityId(2), EntityId(3), EntityId(4));

        assert_eq!(scene.canvas, Some(root), "a lone outer box is not drawn");
        assert_eq!(scene.roots, vec![a, b]);
        assert_eq!(scene.children(None), &[a, b]);
        assert!(!scene.parent.contains_key(&a));
        assert_eq!(scene.parent[&a1], a);
        assert_eq!(scene.depth(a1), 2);

        let between_folders: Vec<_> = scene.edges_at(None).collect();
        assert_eq!(between_folders.len(), 1, "two references onto one pair should be one edge");
        assert_eq!((between_folders[0].from, between_folders[0].to), (a, b));
        assert_eq!(between_folders[0].kind, Call, "the call outranks the import");

        let inside_a: Vec<_> = scene.edges_at(Some(a)).collect();
        assert_eq!(inside_a.len(), 1);
        assert_eq!((inside_a[0].from, inside_a[0].to), (a1, a2));
        assert_eq!(scene.degree[&a], 1, "the lifted edge counts once against the box");
    }

    /// Only the outermost box becomes the canvas. The single-child folder
    /// under it is kept: the spine is how a reader locates a box.
    #[test]
    fn levels_come_deepest_first_and_end_with_the_roots() {
        let graph = graph_from_parents(
            &[
                ("root", Folder, None),
                ("mid", Folder, Some(0)),
                ("deep", Folder, Some(1)),
                ("f.rs", File, Some(2)),
            ],
            &[],
        );
        let scene = Scene::new(&graph, &expanded(&graph));
        assert_eq!(
            scene.levels_bottom_up(),
            vec![Some(EntityId(2)), Some(EntityId(1)), None]
        );
    }
}
