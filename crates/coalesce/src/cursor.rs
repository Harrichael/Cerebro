use std::collections::{HashMap, HashSet};

use entity_graph::{EntityGraph, EntityId, ReferenceId, ReferenceKind};

/// One edge as seen at the current expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoalescedEdge {
    pub from: EntityId,
    pub to: EntityId,
    pub kind: ReferenceKind,
    /// Every raw reference this edge stands for, in graph order. Consumers
    /// that need per-reference detail (sites, diff status) join these back
    /// against `graph.references` instead of re-deriving the projection.
    pub refs: Vec<ReferenceId>,
}

/// Snapshot of what is in view: see [`Cursor::coalesced`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coalesced {
    /// The cut: nodes shown with nothing of theirs inside them.
    pub leaves: Vec<EntityId>,
    /// An endpoint is a leaf, or a container holding some of them. A
    /// container is a thing in its own right -- a file is what `use crate::x`
    /// points at -- so opening one does not take away the lines that named it.
    pub edges: Vec<CoalescedEdge>,
}

/// How far the containment hierarchy is opened up, and nothing else.
///
/// The cut is an antichain: no leaf is an ancestor of another. Everything
/// above it is shown as a container, everything below it is folded into the
/// leaf that holds it. Laying that out as a tree, a diagram or anything else
/// is a consumer's concern.
pub struct Cursor {
    pub leaves: Vec<EntityId>,
}

/// The deepest node at or above the cut that is `id`, or holds it. `None`
/// only for an id the graph does not know.
fn site(graph: &EntityGraph, shown: &HashSet<EntityId>, id: EntityId) -> Option<EntityId> {
    let mut cur = Some(id);
    while let Some(c) = cur {
        if shown.contains(&c) {
            return Some(c);
        }
        cur = graph.get(c)?.parent;
    }
    None
}

impl Cursor {
    /// A cursor showing the outermost entities, with nothing expanded.
    pub fn new(graph: &EntityGraph) -> Self {
        let leaves = graph.entities.iter().filter(|e| e.parent.is_none()).map(|e| e.id).collect();
        Cursor { leaves }
    }

    /// Open a leaf, replacing it with its children. A leaf with no children,
    /// or an entity that is not on the cut, cannot be opened.
    pub fn move_down(&mut self, entity_id: EntityId, graph: &EntityGraph) -> bool {
        let Some(entity) = graph.get(entity_id) else { return false };
        if entity.children.is_empty() || !self.leaves.contains(&entity_id) {
            return false;
        }
        self.leaves.retain(|&id| id != entity_id);
        self.leaves.extend(entity.children.iter().copied());
        true
    }

    /// Shut the container around a leaf, taking every leaf under it with it.
    pub fn move_up(&mut self, entity_id: EntityId, graph: &EntityGraph) -> bool {
        if !self.leaves.contains(&entity_id) {
            return false;
        }
        let Some(parent) = graph.get(entity_id).and_then(|e| e.parent) else { return false };
        self.leaves.retain(|&id| !holds(graph, parent, id));
        self.leaves.push(parent);
        true
    }

    /// The current cut.
    pub fn active(&self) -> &[EntityId] {
        &self.leaves
    }

    /// What this expansion shows, and where each reference lands on it.
    ///
    /// An endpoint is shown by the deepest node at or above the cut that is
    /// it, or holds it. That is total: an endpoint inside a shut leaf lands
    /// on the leaf, and an endpoint that *is* an open container lands on the
    /// container, because a container is a thing you can point at and does
    /// not stop being one when you open it. Only a reference whose two ends
    /// land on the same node is dropped, as a self-loop.
    ///
    /// Derived from the cut every time rather than carried alongside it: an
    /// expansion *is* a cut through the tree, so the same cut has to give the
    /// same picture however the user arrived at it.
    pub fn coalesced(&self, graph: &EntityGraph) -> Coalesced {
        let mut shown: HashSet<EntityId> = self.leaves.iter().copied().collect();
        for &leaf in &self.leaves {
            let mut cur = graph.get(leaf).and_then(|e| e.parent);
            while let Some(c) = cur {
                shown.insert(c);
                cur = graph.get(c).and_then(|e| e.parent);
            }
        }

        let mut slot_of: HashMap<(EntityId, EntityId, ReferenceKind), usize> = HashMap::new();
        let mut edges: Vec<CoalescedEdge> = Vec::new();
        for (i, r) in graph.references.iter().enumerate() {
            let (Some(from), Some(to)) = (site(graph, &shown, r.from), site(graph, &shown, r.to))
            else {
                continue;
            };
            if from == to {
                continue;
            }
            let slot = *slot_of.entry((from, to, r.kind)).or_insert_with(|| {
                edges.push(CoalescedEdge { from, to, kind: r.kind, refs: Vec::new() });
                edges.len() - 1
            });
            edges[slot].refs.push(ReferenceId(i));
        }
        Coalesced { leaves: self.leaves.clone(), edges }
    }
}

/// Is `id` `ancestor`, or under it?
fn holds(graph: &EntityGraph, ancestor: EntityId, id: EntityId) -> bool {
    let mut cur = Some(id);
    while let Some(c) = cur {
        if c == ancestor {
            return true;
        }
        cur = graph.get(c).and_then(|e| e.parent);
    }
    false
}
