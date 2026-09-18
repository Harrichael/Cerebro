//! Carrying an expansion across a rebuild.
//!
//! Ids are arena indices, so re-reading a tree renumbers everything and a
//! [`Cursor`] from before the rebuild names entities that no longer exist.
//! [`id_map`] matches the two graphs on `(kind, path)` and [`migrate_cursor`]
//! re-applies the old expansion through it, so a viewer whose sources changed
//! under it keeps its place instead of snapping shut.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use entity_graph::{EntityGraph, EntityId, EntityKind};

use crate::Cursor;

/// Old id -> new id. Entities match on `(kind, path)`; when several share a
/// key, the i-th in id order matches the i-th on the other side.
pub fn id_map(old: &EntityGraph, new: &EntityGraph) -> Vec<Option<EntityId>> {
    let mut by_key: HashMap<(EntityKind, &Path), Vec<EntityId>> = HashMap::new();
    for e in &new.entities {
        by_key.entry((e.kind, e.path.as_path())).or_default().push(e.id);
    }
    let mut seen: HashMap<(EntityKind, &Path), usize> = HashMap::new();
    old.entities
        .iter()
        .map(|e| {
            let key = (e.kind, e.path.as_path());
            let ordinal = seen.entry(key).or_insert(0);
            let matched = by_key.get(&key).and_then(|ids| ids.get(*ordinal)).copied();
            *ordinal += 1;
            matched
        })
        .collect()
}

/// A new-generation cursor with the old expansion re-applied. Each old leaf lands
/// on its match or, when it is gone, its nearest matched ancestor; the strict
/// ancestors of every target are expanded root-first. A deleted leaf thus
/// coarsens to its surviving parent while its former siblings stay as they
/// were, and `Cursor` keeps its own antichain invariant, so no pruning is
/// needed.
pub fn migrate_cursor(
    old: &EntityGraph,
    old_leaves: &[EntityId],
    map: &[Option<EntityId>],
    new: &EntityGraph,
) -> Cursor {
    let mut expand: HashSet<EntityId> = HashSet::new();
    for &leaf in old_leaves {
        let mut cur = Some(leaf);
        let target = loop {
            let Some(id) = cur else { break None };
            match map.get(id.0).copied().flatten() {
                Some(t) => break Some(t),
                None => cur = old.get(id).and_then(|e| e.parent),
            }
        };
        let mut ancestor = target.and_then(|t| new.get(t)).and_then(|e| e.parent);
        while let Some(a) = ancestor {
            expand.insert(a);
            ancestor = new.get(a).and_then(|e| e.parent);
        }
    }
    // `move_down` only acts on a current leaf, so a node must be expanded
    // after every ancestor of its own: depth order guarantees that.
    let mut order: Vec<EntityId> = expand.into_iter().collect();
    order.sort_by_key(|&id| (depth(new, id), id));
    let mut cursor = Cursor::new(new);
    for id in order {
        cursor.move_down(id, new);
    }
    cursor
}

fn depth(graph: &EntityGraph, mut id: EntityId) -> usize {
    let mut d = 0;
    while let Some(p) = graph.get(id).and_then(|e| e.parent) {
        d += 1;
        id = p;
    }
    d
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use entity_graph::EntityKind::*;
    use entity_graph::test_support::graph_from_parents;

    use super::*;

    fn graph(rows: &[(&str, EntityKind, Option<usize>, &str)]) -> EntityGraph {
        let parents: Vec<_> = rows.iter().map(|r| (r.0, r.1, r.2)).collect();
        let mut g = graph_from_parents(&parents, &[]);
        for (e, row) in g.entities.iter_mut().zip(rows) {
            e.path = PathBuf::from(row.3);
        }
        g
    }

    fn leaves(c: &Cursor) -> HashSet<EntityId> {
        c.leaves.iter().copied().collect()
    }

    /// Old tree: proj/src/{a.rs, b.rs::{f, g, g}}; new tree drops `a.rs`, adds
    /// `c.rs` and keeps one `g`, all under fresh ids in a different order.
    /// The duplicate `g`s match by ordinal (the second one unmatched), the
    /// expansion `{a.rs, f, g, g}` survives with `a.rs` coarsened to `src` — so the
    /// new leaves are `src`'s files plus `f` and `g` inside `b.rs`.
    #[test]
    fn id_map_and_cursor_migration() {
        let old = graph(&[
            ("proj", Folder, None, "proj"),
            ("src", Folder, Some(0), "proj/src"),
            ("a.rs", File, Some(1), "proj/src/a.rs"),
            ("b.rs", File, Some(1), "proj/src/b.rs"),
            ("f", Function, Some(3), "proj/src/b.rs/f"),
            ("g", Function, Some(3), "proj/src/b.rs/g"),
            ("g", Function, Some(3), "proj/src/b.rs/g"),
        ]);
        let new = graph(&[
            ("proj", Folder, None, "proj"),
            ("src", Folder, Some(0), "proj/src"),
            ("c.rs", File, Some(1), "proj/src/c.rs"),
            ("b.rs", File, Some(1), "proj/src/b.rs"),
            ("g", Function, Some(3), "proj/src/b.rs/g"),
            ("f", Function, Some(3), "proj/src/b.rs/f"),
        ]);
        let map = id_map(&old, &new);
        let id = |n: usize| Some(EntityId(n));
        assert_eq!(map, vec![id(0), id(1), None, id(3), id(5), id(4), None]);

        let old_leaves = [EntityId(2), EntityId(4), EntityId(5), EntityId(6)];
        let migrated = migrate_cursor(&old, &old_leaves, &map, &new);
        assert_eq!(leaves(&migrated), HashSet::from([EntityId(2), EntityId(4), EntityId(5)]));

        // The file level: the vanished `a.rs` collapses to `src`, whose other
        // files are the leaves, and nothing below them is expanded.
        let migrated = migrate_cursor(&old, &[EntityId(2), EntityId(3)], &map, &new);
        assert_eq!(leaves(&migrated), HashSet::from([EntityId(2), EntityId(3)]));

        // A root-only expansion stays a root-only expansion.
        assert_eq!(leaves(&migrate_cursor(&old, &[EntityId(0)], &map, &new)), HashSet::from([EntityId(0)]));
    }
}
