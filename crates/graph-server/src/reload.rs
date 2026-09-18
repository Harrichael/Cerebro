//! One generation of everything the server derives from a load, and how one
//! generation carries over to the next. Ids are arena indices, so a rebuild
//! renumbers everything; `coalesce::migrate` matches entities across adjacent
//! generations and re-applies an expansion through that map, and `RemapHistory`
//! keeps the last few of those maps so a client a generation or two behind can
//! still be told where its ids went.
//! See `ui/CONTRACT.md`, "Live updates".

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Mutex;

use coalesce::Cursor;
use entity_graph::{EntityGraph, EntityId};
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

#[cfg(test)]
mod tests {
    use super::*;

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
