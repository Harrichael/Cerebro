use std::collections::{HashMap, HashSet};

use entity_graph::{EntityGraph, EntityId, ReferenceId, ReferenceKind};

/// A reference tracked within the cursor's context.
///
/// Represents a symbolic reference in the entity graph, along with the lineages
/// (paths from root to entity) and which cursor leaves are involved.
#[derive(Debug, Clone)]
pub struct CursorReference {
    /// ID of the reference in the entity graph.
    pub reference_id: ReferenceId,
    pub kind: ReferenceKind,
    /// Lineage from root to the source entity.
    pub from_lineage: Vec<EntityId>,
    /// Lineage from root to the target entity.
    pub to_lineage: Vec<EntityId>,
    /// Which cursor leaf the source entity is under (by EntityId).
    pub from_leaf: EntityId,
    /// Which cursor leaf the target entity is under (by EntityId).
    pub to_leaf: EntityId,
}

/// One reference edge as seen at the current expansion.
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
    pub leaves: Vec<EntityId>,
    pub edges: Vec<CoalescedEdge>,
}

/// Navigation cursor through the entity graph's containment hierarchy.
///
/// Tracks a set of active leaf entities (the current focus points). Navigation
/// operations query the EntityGraph to move individual cursors through the contains topology:
/// - Moving down: expand a specific leaf to its children
/// - Moving up: collapse a specific leaf to its parent, aggregating siblings
///
/// The active set maintains the invariant that no entity is a descendant of another.
///
/// Additionally, the cursor tracks all symbolic references in the entity graph
/// and maintains which cursor leaves are involved in each reference. As leaves
/// move up and down, references are updated, split, or joined accordingly.
pub struct Cursor {
    /// Current active leaves in the containment hierarchy.
    /// Invariant: no entity is a descendant of another.
    pub leaves: Vec<EntityId>,
    /// References tracked in the cursor's context.
    pub references: Vec<CursorReference>,
}

impl Cursor {
    /// Create a new cursor starting at root entities and initialize references.
    ///
    /// Populates the cursor with all symbolic references from the entity graph,
    /// computing the lineage (ancestor path) for each reference endpoint.
    pub fn new(graph: &EntityGraph) -> Self {
        let roots: Vec<EntityId> = graph
            .entities
            .iter()
            .filter(|e| e.parent.is_none())
            .map(|e| e.id)
            .collect();
        let mut references = Vec::new();

        for (idx, reference) in graph.references.iter().enumerate() {
            let from_lineage = Self::compute_lineage(reference.from, graph);
            let to_lineage = Self::compute_lineage(reference.to, graph);

            let from_leaf = roots
                .iter()
                .find(|&&root_id| from_lineage.contains(&root_id))
                .copied()
                .unwrap_or(reference.from);

            let to_leaf = roots
                .iter()
                .find(|&&root_id| to_lineage.contains(&root_id))
                .copied()
                .unwrap_or(reference.to);

            references.push(CursorReference {
                reference_id: ReferenceId(idx),
                kind: reference.kind,
                from_lineage,
                to_lineage,
                from_leaf,
                to_leaf,
            });
        }

        Cursor { leaves: roots, references }
    }

    /// The ancestry order: [root, ..., entity].
    fn compute_lineage(mut entity_id: EntityId, graph: &EntityGraph) -> Vec<EntityId> {
        let mut lineage = Vec::new();
        loop {
            if let Some(entity) = graph.get(entity_id) {
                lineage.push(entity_id);
                if let Some(parent) = entity.parent {
                    entity_id = parent;
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        lineage.reverse();
        lineage
    }

    /// Move a specific leaf down one level in the containment hierarchy.
    ///
    /// Expands the given leaf to show its direct children. The leaf is replaced
    /// with all its children in the active set. If the leaf has no children,
    /// it remains unchanged.
    ///
    /// Updates references as the leaf structure changes.
    pub fn move_down(&mut self, entity_id: EntityId, graph: &EntityGraph) -> bool {
        if let Some(_pos) = self.leaves.iter().position(|&id| id == entity_id) {
            if let Some(entity) = graph.get(entity_id) {
                if entity.children.is_empty() {
                    // No children, no change
                    return false;
                }

                let children = entity.children.clone();

                // Remove the leaf and add all its children
                self.leaves.retain(|&id| id != entity_id);
                self.leaves.extend(children.iter().copied());

                // Split references: references that pointed to entity_id as a leaf
                // need to be re-assigned to the appropriate child
                self.split_references_down(entity_id, &children);

                return true;
            }
        }
        false
    }

    /// Move a specific leaf up one level in the containment hierarchy.
    ///
    /// Collapses the given leaf to its parent. The leaf is replaced with its parent,
    /// and any siblings (other children of the parent) are removed from leaves to
    /// maintain the non-overlapping invariant.
    /// If the leaf has no parent (it's the root), it remains unchanged.
    ///
    /// Updates references as the leaf structure changes.
    pub fn move_up(&mut self, entity_id: EntityId, graph: &EntityGraph) -> bool {
        if let Some(_pos) = self.leaves.iter().position(|&id| id == entity_id) {
            if let Some(entity) = graph.get(entity_id) {
                if let Some(parent_id) = entity.parent {
                    let leaves_affected = self
                        .leaves
                        .iter()
                        .filter(|&&leaf_id| {
                            // Check if leaf_id is a descendant of parent_id
                            let lineage = Self::compute_lineage(leaf_id, graph);
                            lineage.contains(&parent_id)
                        })
                        .copied()
                        .collect::<Vec<_>>();

                    self.leaves.retain(|&leaf_id| !leaves_affected.contains(&leaf_id));

                    self.join_references_up(&leaves_affected, parent_id);

                    self.leaves.push(parent_id);

                    return true;
                }
            }
        }
        false
    }

    /// Get the current active leaves.
    pub fn active(&self) -> &[EntityId] {
        &self.leaves
    }

    /// The view at the current expansion: every active leaf, plus every reference
    /// projected onto the leaves that contain its endpoints.
    ///
    /// References whose endpoints fall under the same leaf collapse into
    /// self-loops and are dropped; the rest are deduplicated on
    /// `(from, to, kind)`, each surviving edge keeping every reference that
    /// projected onto it in `refs`. Needs no `&EntityGraph`
    /// because the projection was maintained incrementally by `move_down` /
    /// `move_up`.
    ///
    /// Gotcha: a reference whose endpoint *is* an expanded entity (e.g. a
    /// file-level import once that file is expanded) has no child to land
    /// on, so its leaf pointer stays on the now-inactive entity. Such an edge
    /// can name an endpoint that is not in `leaves`.
    pub fn coalesced(&self) -> Coalesced {
        // A reference whose endpoint *is* an expanded entity (a file-level
        // import after expanding that file) has no leaf to live on: the
        // lineage ends at the old leaf, so split_references_down leaves it
        // pointing at an inactive entity. Such edges are not drawable and are
        // dropped here so `edges` is always within `leaves x leaves`.
        let active: HashSet<EntityId> = self.leaves.iter().copied().collect();
        let mut slot_of: HashMap<(EntityId, EntityId, ReferenceKind), usize> = HashMap::new();
        let mut edges: Vec<CoalescedEdge> = Vec::new();
        for r in self
            .references
            .iter()
            .filter(|r| r.from_leaf != r.to_leaf)
            .filter(|r| active.contains(&r.from_leaf) && active.contains(&r.to_leaf))
        {
            let slot = *slot_of.entry((r.from_leaf, r.to_leaf, r.kind)).or_insert_with(|| {
                let edge = CoalescedEdge {
                    from: r.from_leaf,
                    to: r.to_leaf,
                    kind: r.kind,
                    refs: Vec::new(),
                };
                edges.push(edge);
                edges.len() - 1
            });
            edges[slot].refs.push(r.reference_id);
        }
        Coalesced { leaves: self.leaves.clone(), edges }
    }

    // Reference update helpers

    /// Split references when a leaf expands to its children.
    /// References that had the old leaf as source/target are updated to point to children.
    fn split_references_down(
        &mut self,
        old_leaf: EntityId,
        children: &[EntityId],
    ) {
        for cursor_ref in &mut self.references {
            // Check if reference points from old_leaf to something
            if cursor_ref.from_leaf == old_leaf {
                if let Some(new_leaf) = Self::find_containing_child(&cursor_ref.from_lineage, children) {
                    cursor_ref.from_leaf = new_leaf;
                }
            }

            // Check if reference points to old_leaf
            if cursor_ref.to_leaf == old_leaf {
                if let Some(new_leaf) = Self::find_containing_child(&cursor_ref.to_lineage, children) {
                    cursor_ref.to_leaf = new_leaf;
                }
            }
        }
    }

    /// Join references when siblings collapse to their parent.
    fn join_references_up(&mut self, leaflets: &[EntityId], parent_id: EntityId) {
        for cursor_ref in &mut self.references {
            if leaflets.contains(&cursor_ref.from_leaf) {
                cursor_ref.from_leaf = parent_id;
            }
            if leaflets.contains(&cursor_ref.to_leaf) {
                cursor_ref.to_leaf = parent_id;
            }
        }
    }

    /// Find which child entity is in the given lineage.
    fn find_containing_child(lineage: &[EntityId], children: &[EntityId]) -> Option<EntityId> {
        // Check which child appears in the lineage
        for child in children {
            if lineage.contains(child) {
                return Some(*child);
            }
        }
        None
    }
}
