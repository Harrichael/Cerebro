//! What is in the picture, before anything decides where it goes.
//!
//! The seed of the diagram model. It holds the rules that change *which* nodes
//! and edges exist — not how they are laid out (`layout`) or drawn (`render`).
//! Today that is the test filter, edge collapse, hiding and scoping; bundling
//! joins them here when it arrives.
//!
//! `nodes` is the cut -- what is drawn with nothing of its own inside it.
//! An edge may end above the cut, on a box, because a box is a thing you can
//! point at: `use crate::x;` names a file and does not stop naming it when
//! the file is opened. The boxes themselves are not listed, since they are
//! exactly the ancestors of `nodes` and `scene` derives them anyway.

use std::collections::BTreeSet;

use coalesce::Coalesced;
use entity_graph::{EntityGraph, EntityId, ReferenceId, ReferenceKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: EntityId,
    pub to: EntityId,
    pub kind: ReferenceKind,
    /// Every reference this edge stands for. Collapsing a pair keeps them all,
    /// so an inspector can still list what the single line was made of.
    pub refs: Vec<ReferenceId>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Picture {
    pub nodes: Vec<EntityId>,
    pub edges: Vec<Edge>,
}

#[derive(Debug, Clone)]
pub struct Settings {
    /// Matches the browser, which starts with tests shown: leaving them out by
    /// default would quietly disagree with the view this replaces.
    pub show_tests: bool,
    /// Draw one edge per ordered pair rather than one per reference kind. Two
    /// files usually relate several ways at once, and drawing each separately
    /// multiplies the lines without adding information.
    pub one_per_pair: bool,
    /// Entities the user took out of the picture. Hiding one hides everything
    /// under it: an entity is a subtree, and leaving the children of a hidden
    /// folder on screen would be showing what was just dismissed.
    pub hidden: BTreeSet<EntityId>,
    /// Show only this entity and what is under it. Where hiding says "not
    /// that", scoping says "only this"; a graph gets large enough that picking
    /// out the one subtree is the faster of the two.
    pub scope: Option<EntityId>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            show_tests: true,
            one_per_pair: true,
            hidden: BTreeSet::new(),
            scope: None,
        }
    }
}

/// Lower is stronger. A pair that both calls and imports is a call: the import
/// is how the call was made possible, not a separate relationship worth its own
/// line. Matches `PRECEDENCE` in the browser's `ui/common.js`.
fn rank(kind: ReferenceKind) -> u8 {
    match kind {
        ReferenceKind::Call => 0,
        ReferenceKind::TypeRef => 1,
        ReferenceKind::VarRef => 2,
        ReferenceKind::Import => 3,
        ReferenceKind::Generic => 4,
    }
}

/// Does `id`, or anything holding it, satisfy `wanted`?
fn lineage_has(graph: &EntityGraph, id: EntityId, wanted: impl Fn(EntityId) -> bool) -> bool {
    let mut cur = Some(id);
    while let Some(c) = cur {
        if wanted(c) {
            return true;
        }
        cur = graph.get(c).and_then(|e| e.parent);
    }
    false
}

/// Narrow one expansion of the graph to what should actually be drawn.
pub fn apply(graph: &EntityGraph, coalesced: &Coalesced, settings: &Settings) -> Picture {
    // Hiding and scoping are asked of an *ancestor*, but the cursor hands back
    // leaves, so both questions are answered by walking up from each leaf.
    let nodes: Vec<EntityId> = coalesced
        .leaves
        .iter()
        .copied()
        .filter(|&l| settings.show_tests || !graph.get(l).is_some_and(|e| e.is_test))
        .filter(|&l| {
            settings.hidden.is_empty() || !lineage_has(graph, l, |c| settings.hidden.contains(&c))
        })
        .filter(|&l| match settings.scope {
            None => true,
            Some(s) => lineage_has(graph, l, |c| c == s),
        })
        .collect();

    // An edge may end on a container rather than a leaf, because a container
    // is what `use crate::x;` names. It stays in the picture for as long as
    // anything under it does.
    let mut shown: std::collections::BTreeSet<EntityId> = nodes.iter().copied().collect();
    for &leaf in &nodes {
        let mut cur = graph.get(leaf).and_then(|e| e.parent);
        while let Some(c) = cur {
            shown.insert(c);
            cur = graph.get(c).and_then(|e| e.parent);
        }
    }
    let mut edges: Vec<Edge> = coalesced
        .edges
        .iter()
        .filter(|e| shown.contains(&e.from) && shown.contains(&e.to))
        .map(|e| Edge { from: e.from, to: e.to, kind: e.kind, refs: e.refs.clone() })
        .collect();

    if settings.one_per_pair {
        let mut by_pair: std::collections::BTreeMap<(EntityId, EntityId), Edge> = Default::default();
        for edge in edges {
            by_pair
                .entry((edge.from, edge.to))
                .and_modify(|kept| {
                    kept.refs.extend(edge.refs.iter().copied());
                    if rank(edge.kind) < rank(kept.kind) {
                        kept.kind = edge.kind;
                    }
                })
                .or_insert(edge);
        }
        edges = by_pair.into_values().collect();
    }

    Picture { nodes, edges }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entity_graph::EntityKind::{File, Folder};
    use entity_graph::ReferenceKind::{Call, Import};
    use entity_graph::test_support::graph_from_parents;

    fn expanded(graph: &EntityGraph) -> Coalesced {
        let mut cursor = coalesce::Cursor::new(graph);
        loop {
            let mut moved = false;
            for leaf in cursor.coalesced(graph).leaves {
                moved |= cursor.move_down(leaf, graph);
            }
            if !moved {
                return cursor.coalesced(graph);
            }
        }
    }

    #[test]
    fn a_pair_related_several_ways_draws_once_as_its_strongest_kind() {
        let graph = graph_from_parents(
            &[("root", Folder, None), ("a.rs", File, Some(0)), ("b.rs", File, Some(0))],
            &[(1, 2, Import), (1, 2, Call)],
        );
        let every =
            apply(&graph, &expanded(&graph), &Settings { one_per_pair: false, ..Default::default() });
        assert_eq!(every.edges.len(), 2);

        let collapsed = apply(&graph, &expanded(&graph), &Settings::default());
        assert_eq!(collapsed.edges.len(), 1);
        assert_eq!(collapsed.edges[0].kind, Call, "a call outranks the import that enabled it");
        assert_eq!(collapsed.edges[0].refs.len(), 2, "both references are kept on the one edge");
    }

    /// Hiding is asked of one entity but means its whole subtree, and scoping
    /// is the same walk read the other way round. Both are checked here
    /// against a leaf two levels below the entity named, which is the case
    /// that a naive `nodes.contains(id)` test would get wrong.
    #[test]
    fn hiding_a_folder_hides_what_is_under_it_and_scoping_keeps_only_that() {
        let graph = graph_from_parents(
            &[
                ("root", Folder, None),
                ("src", Folder, Some(0)),
                ("lib.rs", File, Some(1)),
                ("docs", Folder, Some(0)),
                ("readme.md", File, Some(3)),
            ],
            &[(2, 4, Call)],
        );
        let (src, lib, docs) = (EntityId(1), EntityId(2), EntityId(4));
        let named = |p: &Picture| p.nodes.clone();

        let all = apply(&graph, &expanded(&graph), &Settings::default());
        assert!(named(&all).contains(&lib) && named(&all).contains(&docs));

        let hidden = apply(
            &graph,
            &expanded(&graph),
            &Settings { hidden: [src].into_iter().collect(), ..Default::default() },
        );
        assert!(!named(&hidden).contains(&lib), "hiding a folder left its file behind");
        assert!(named(&hidden).contains(&docs), "hiding one folder took out another");
        assert!(hidden.edges.is_empty(), "an edge survived the node it came from");

        let scoped =
            apply(&graph, &expanded(&graph), &Settings { scope: Some(src), ..Default::default() });
        assert_eq!(named(&scoped), vec![lib], "scoping should keep only what is under it");
    }

    /// Dropping a test leaf has to drop its edges too, or the layout is handed
    /// an edge whose endpoint is not in the picture.
    #[test]
    fn hiding_tests_drops_their_edges_with_them() {
        let mut graph = graph_from_parents(
            &[("root", Folder, None), ("lib.rs", File, Some(0)), ("tests.rs", File, Some(0))],
            &[(2, 1, Call)],
        );
        graph.entities[2].is_test = true;

        let shown = apply(&graph, &expanded(&graph), &Settings::default());
        assert_eq!(shown.nodes.len(), 2);
        assert_eq!(shown.edges.len(), 1);

        let hidden =
            apply(&graph, &expanded(&graph), &Settings { show_tests: false, ..Default::default() });
        assert_eq!(hidden.nodes.len(), 1);
        assert!(hidden.edges.is_empty(), "an edge survived the node it came from");
    }
}
