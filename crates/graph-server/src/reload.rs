//! One generation of everything the server derives from a load, and how one
//! generation carries over to the next. Ids are arena indices, so a rebuild
//! renumbers everything; `id_map` matches entities across adjacent generations
//! on `(kind, path)` and `migrate_cursor` re-applies an expansion through that map.
//! See `ui/CONTRACT.md`, "Live updates".

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Mutex;

use coalesce::Cursor;
use entity_graph::{EntityGraph, EntityId, EntityKind};
use graph_diff::{FileDiff, GraphDiff, Status};

use crate::dto::{self, DiffView, RemapDto};
use crate::handlers::build_text_index;
use crate::text_index::TextIndex;

/// What a load produced. `Diff` is what `graph_diff::diff` returns plus the
/// labels the payloads show for the base side.
pub enum Loaded {
    Graph(EntityGraph),
    Diff { diff: GraphDiff, base_label: String, base_commit: String },
}

pub type Loader = Box<dyn Fn() -> anyhow::Result<Loaded> + Send + Sync>;

/// Everything diff mode adds: the change tags for the union graph and both
/// texts of every changed file.
pub struct DiffState {
    pub base_label: String,
    pub base_commit: String,
    pub entity_status: Vec<Status>,
    pub reference_status: Vec<Status>,
    pub churn: Vec<(usize, usize)>,
    pub files: HashMap<EntityId, FileDiff>,
}

impl DiffState {
    pub fn view(&self) -> DiffView<'_> {
        DiffView {
            base: &self.base_label,
            base_commit: &self.base_commit,
            entity_status: &self.entity_status,
            reference_status: &self.reference_status,
            churn: &self.churn,
        }
    }
}

/// One generation. Immutable except the cursor, which is the only piece of
/// server state a request mutates.
pub struct Snapshot {
    pub generation: u64,
    pub graph: EntityGraph,
    // Rendered once per generation; on a real repo it is by far the largest
    // payload.
    pub graph_json: String,
    pub cursor: Mutex<Cursor>,
    pub diff: Option<DiffState>,
    pub index: TextIndex,
}

impl Snapshot {
    pub fn build(
        generation: u64,
        loaded: Loaded,
        root: &Path,
        remap: Option<&[Option<EntityId>]>,
    ) -> Snapshot {
        let (graph, diff) = match loaded {
            Loaded::Graph(graph) => (graph, None),
            Loaded::Diff { diff, base_label, base_commit } => {
                let GraphDiff { graph, entity_status, reference_status, churn, files } = diff;
                let state =
                    DiffState { base_label, base_commit, entity_status, reference_status, churn, files };
                (graph, Some(state))
            }
        };
        let index = build_text_index(&graph, root, diff.as_ref().map(|d| &d.files));
        let cursor = Mutex::new(Cursor::new(&graph));
        let mut snap = Snapshot { generation, graph, graph_json: String::new(), cursor, diff, index };
        snap.graph_json = snap.render_graph_json(remap.map(|ids| remap_dto(generation - 1, ids)));
        snap
    }

    /// `/graph.json` with the given remap; `graph_json` caches the one from
    /// the previous generation, the common case.
    pub fn render_graph_json(&self, remap: Option<RemapDto>) -> String {
        let dto = match &self.diff {
            None => dto::graph_dto(&self.graph, self.generation, remap),
            Some(d) => dto::graph_dto_with_diff(&self.graph, &d.view(), self.generation, remap),
        };
        serde_json::to_string(&dto).expect("GraphDto serialization is infallible")
    }
}

pub fn remap_dto(from: u64, ids: &[Option<EntityId>]) -> RemapDto {
    RemapDto { from, ids: ids.iter().map(|id| id.map(|id| id.0)).collect() }
}

/// The last few adjacent-generation maps. A client polls for the generation
/// and a hidden tab is polled about once a minute, so sleeping through several
/// rebuilds is routine; composing the steps lets it translate its state
/// instead of resetting.
pub struct RemapHistory {
    // steps[i] maps generation first_from + i to the next one.
    first_from: u64,
    steps: VecDeque<Vec<Option<EntityId>>>,
}

impl RemapHistory {
    // Bounded because one step is an entry per entity of the old generation.
    pub const KEEP: usize = 16;

    pub fn new() -> RemapHistory {
        RemapHistory { first_from: 0, steps: VecDeque::new() }
    }

    /// `from` must be the generation the newest recorded step leads to (or
    /// anything, when empty): a gap would make composition silently wrong.
    pub fn push(&mut self, from: u64, ids: Vec<Option<EntityId>>) {
        if self.steps.is_empty() {
            self.first_from = from;
        } else {
            assert_eq!(from, self.first_from + self.steps.len() as u64, "remap history has a gap");
        }
        self.steps.push_back(ids);
        if self.steps.len() > Self::KEEP {
            self.steps.pop_front();
            self.first_from += 1;
        }
    }

    /// The map from generation `from` to `to`, or None when `from` is not
    /// (or no longer) remembered.
    pub fn compose(&self, from: u64, to: u64) -> Option<Vec<Option<EntityId>>> {
        let last_to = self.first_from + self.steps.len() as u64;
        if from < self.first_from || from >= to || to > last_to {
            return None;
        }
        let mut steps = self.steps.iter().skip((from - self.first_from) as usize).take((to - from) as usize);
        let mut ids = steps.next()?.clone();
        for step in steps {
            for id in ids.iter_mut() {
                *id = id.and_then(|EntityId(i)| step.get(i).copied().flatten());
            }
        }
        Some(ids)
    }
}

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

    /// Composition across steps: an entity that shifts twice lands where the
    /// second step puts it, one that vanishes midway stays gone, ids beyond
    /// a later step's range are gone too; the window is bounded and anything
    /// before it, or not a strictly forward span, is unanswerable.
    #[test]
    fn remap_history_composes_steps_within_its_window() {
        let id = |n: usize| Some(EntityId(n));
        let mut h = RemapHistory::new();
        h.push(1, vec![id(1), id(0), id(2), None]);
        h.push(2, vec![None, id(2), id(0)]);
        assert_eq!(h.compose(1, 2), Some(vec![id(1), id(0), id(2), None]));
        assert_eq!(h.compose(1, 3), Some(vec![id(2), None, id(0), None]));
        assert_eq!(h.compose(2, 3), Some(vec![None, id(2), id(0)]));
        assert_eq!(h.compose(0, 3), None, "generation 0 was never recorded");
        assert_eq!(h.compose(2, 2), None);
        assert_eq!(h.compose(1, 4), None, "generation 4 does not exist yet");

        for from in 3..3 + RemapHistory::KEEP as u64 {
            h.push(from, vec![id(0)]);
        }
        assert_eq!(h.compose(1, 4), None, "the oldest steps fell out of the window");
        assert_eq!(h.compose(3, 3 + RemapHistory::KEEP as u64), Some(vec![id(0)]));
    }
}
