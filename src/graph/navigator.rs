use crate::graph::cursor::Cursor;
use crate::graph::entity::{Entity, EntityGraph, EntityId};
use crate::graph::tree::{GraphTree, GraphTreeNodeId};

/// The central controller that wraps the static entity graph and the dynamic
/// cursor, projecting the cursor's current leaf set into a renderable
/// [`GraphTree`].
///
/// The Navigator owns three things:
/// - `graph`     — the immutable entity/reference database.
/// - `cursor`    — the mutable navigation state (which leaves are active).
/// - `view_tree` — the derived, renderable tree; always in sync with `cursor`.
pub struct Navigator {
    graph: EntityGraph,
    cursor: Cursor,
    view_tree: GraphTree,
}

impl Navigator {
    /// Build a navigator starting at the root level of `graph`.
    pub fn new(graph: EntityGraph) -> Self {
        let cursor = Cursor::new(&graph);
        let mut navigator = Self {
            graph,
            cursor,
            view_tree: GraphTree::new(),
        };
        navigator.sync_tree();
        navigator
    }

    // --- PUBLIC API FOR THE UI ---

    pub fn tree(&self) -> &GraphTree {
        &self.view_tree
    }

    /// Look up an entity from the graph by its ID.
    ///
    /// Used by the UI layer to retrieve entity metadata (name, kind, etc.)
    /// for rendering tree nodes without exposing the graph directly.
    pub fn entity(&self, id: EntityId) -> Option<&Entity> {
        self.graph.get(id)
    }

    /// Expand the entity represented by `focused_tree_node` one level down.
    ///
    /// Returns the [`EntityId`] of the first revealed child so the UI can
    /// move its highlight there, or `None` if the entity has no children.
    pub fn zoom_in(&mut self, focused_tree_node: GraphTreeNodeId) -> Option<EntityId> {
        let entity_id = self.view_tree.get(focused_tree_node)?.entity_id;
        if self.cursor.move_down(entity_id, &self.graph) {
            self.sync_tree();
            self.graph.get(entity_id)?.children.first().copied()
        } else {
            None
        }
    }

    /// Collapse the entity represented by `focused_tree_node` one level up.
    ///
    /// Returns the [`EntityId`] of the parent so the UI can move its
    /// highlight there, or `None` if the entity has no parent (already root).
    pub fn zoom_out(&mut self, focused_tree_node: GraphTreeNodeId) -> Option<EntityId> {
        let entity_id = self.view_tree.get(focused_tree_node)?.entity_id;
        if self.cursor.move_up(entity_id, &self.graph) {
            self.sync_tree();
            self.graph.get(entity_id)?.parent
        } else {
            None
        }
    }

    // --- PRIVATE SYNCHRONIZATION LOGIC ---

    /// Rebuild `view_tree` from the cursor's current state.
    ///
    /// Algorithm:
    /// 1. Register every active leaf as a `GraphTree` node.
    /// 2. Build an adjacency list from the coalesced edges, deduplicated by
    ///    `(from, to)` since the tree does not distinguish reference kinds.
    /// 3. Walk the reference graph with a DFS spanning forest: only the first
    ///    time a node is reached do we emit an `insert_edge` call.  Back-edges
    ///    and cross-edges are silently dropped.
    ///
    /// Producing a spanning forest rather than inserting every edge has two
    /// important benefits:
    ///
    /// - **No cycles in the view tree.**  Mutual references (A→B and B→A) no
    ///   longer create `NodeKind::Cycle` / `NodeKind::Ref` stubs; the user
    ///   just sees A→B without the confusing ↺ marker.
    ///
    /// - **No exponential node growth.**  Previously, inserting hundreds of
    ///   deduplicated edges between the same small set of folder-level leaves
    ///   triggered `deep_copy_subtree` for each duplicate parent, growing the
    ///   tree exponentially.  The DFS guarantees each entity is placed in the
    ///   tree exactly once.
    fn sync_tree(&mut self) {
        self.view_tree = GraphTree::new();
        let coalesced = self.cursor.coalesced(&self.graph);

        // All active leaves must exist before any edges are wired.
        for &leaf in &coalesced.leaves {
            self.view_tree.insert_entity(leaf, vec![]);
        }

        let mut adj: std::collections::HashMap<EntityId, Vec<EntityId>> =
            std::collections::HashMap::new();
        {
            let mut seen = std::collections::HashSet::new();
            for edge in &coalesced.edges {
                if seen.insert((edge.from, edge.to)) {
                    adj.entry(edge.from).or_default().push(edge.to);
                }
            }
        }

        // DFS spanning forest: only tree edges are forwarded to insert_edge.
        let leaves = coalesced.leaves;
        let mut visited = std::collections::HashSet::new();
        for &root in &leaves {
            if visited.contains(&root) {
                continue;
            }
            let mut stack: Vec<(EntityId, Option<EntityId>)> = vec![(root, None)];
            while let Some((node, parent)) = stack.pop() {
                if !visited.insert(node) {
                    continue;
                }
                if let Some(p) = parent {
                    self.view_tree.insert_edge(p, node);
                }
                if let Some(children) = adj.get(&node) {
                    // Push in reverse so the first adjacency-list child is
                    // processed first (LIFO stack, so last-pushed = first-popped).
                    for &child in children.iter().rev() {
                        if !visited.contains(&child) {
                            stack.push((child, Some(node)));
                        }
                    }
                }
            }
        }
    }
}
