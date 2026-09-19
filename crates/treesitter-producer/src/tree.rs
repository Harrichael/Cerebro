/// Hierarchical code node kinds, ordered from coarsest to finest granularity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)]
pub enum NodeKind {
    Folder,
    Module,
    File,
    Class,
    Function,
    Block,
    Line,
}

impl NodeKind {
    /// Granularity level: lower number = coarser detail.
    pub fn level(&self) -> u8 {
        match self {
            NodeKind::Folder => 0,
            NodeKind::Module => 1,
            NodeKind::File => 2,
            NodeKind::Class => 3,
            NodeKind::Function => 4,
            NodeKind::Block => 5,
            NodeKind::Line => 6,
        }
    }

    /// One step coarser. Returns `None` if already at the coarsest level.
    pub fn coarser(&self) -> Option<NodeKind> {
        match self {
            NodeKind::Module => Some(NodeKind::Folder),
            NodeKind::File => Some(NodeKind::Module),
            NodeKind::Class => Some(NodeKind::File),
            NodeKind::Function => Some(NodeKind::Class),
            NodeKind::Block => Some(NodeKind::Function),
            NodeKind::Line => Some(NodeKind::Block),
            NodeKind::Folder => None,
        }
    }

    /// One step finer. Returns `None` if already at the finest level.
    pub fn finer(&self) -> Option<NodeKind> {
        match self {
            NodeKind::Folder => Some(NodeKind::Module),
            NodeKind::Module => Some(NodeKind::File),
            NodeKind::File => Some(NodeKind::Class),
            NodeKind::Class => Some(NodeKind::Function),
            NodeKind::Function => Some(NodeKind::Block),
            NodeKind::Block => Some(NodeKind::Line),
            NodeKind::Line => None,
        }
    }

    /// True if this kind is at a finer granularity than `other`.
    pub fn is_finer_than(&self, other: &NodeKind) -> bool {
        self.level() > other.level()
    }
}

impl std::fmt::Display for NodeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            NodeKind::Folder => "folder",
            NodeKind::Module => "module",
            NodeKind::File => "file",
            NodeKind::Class => "class/struct",
            NodeKind::Function => "fn/method",
            NodeKind::Block => "block",
            NodeKind::Line => "line",
        };
        write!(f, "{s}")
    }
}

/// The kind of symbolic relationship between two nodes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[allow(dead_code)]
pub enum ReferenceKind {
    /// A function or method call.
    Call,
    /// An import or `use` statement.
    Import,
    /// A type annotation or type usage.
    TypeRef,
    /// A variable read or write.
    VarRef,
    /// Any other symbolic reference.
    Generic,
}

impl std::fmt::Display for ReferenceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ReferenceKind::Call => "call",
            ReferenceKind::Import => "import",
            ReferenceKind::TypeRef => "type_ref",
            ReferenceKind::VarRef => "var_ref",
            ReferenceKind::Generic => "generic",
        };
        write!(f, "{s}")
    }
}

/// A directed symbolic reference edge from one code node to another.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Reference {
    /// ID of the source node (where the reference originates).
    pub from: usize,
    /// ID of the target node (the symbol being referenced).
    pub to: usize,
    /// The kind of relationship this reference represents.
    pub kind: ReferenceKind,
    /// 0-indexed source line of the occurrence, in `from`'s file.
    pub line: usize,
}

impl Reference {
    pub fn new(from: usize, to: usize, kind: ReferenceKind, line: usize) -> Self {
        Reference { from, to, kind, line }
    }
}

/// A graph of directed symbolic references between code nodes.
///
/// This forms the second structural layer of the entity model alongside the
/// hierarchical contains topology stored in [`CodeTree`].  Each edge records
/// which node *uses* which other node (calls, imports, type references, …).
#[derive(Debug, Default)]
pub struct ReferenceGraph {
    edges: Vec<Reference>,
}

impl ReferenceGraph {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a directed reference edge from `from` to `to`, occurring at `line`.
    pub fn add_reference_at(&mut self, from: usize, to: usize, kind: ReferenceKind, line: usize) {
        self.edges.push(Reference::new(from, to, kind, line));
    }

    /// Return all reference edges.
    pub fn references(&self) -> &[Reference] {
        &self.edges
    }

    /// Return all edges that originate from `node_id`.
    #[allow(dead_code)]
    pub fn refs_from(&self, node_id: usize) -> Vec<&Reference> {
        self.edges.iter().filter(|r| r.from == node_id).collect()
    }

    /// Return all edges that point to `node_id`.
    #[allow(dead_code)]
    pub fn refs_to(&self, node_id: usize) -> Vec<&Reference> {
        self.edges.iter().filter(|r| r.to == node_id).collect()
    }
}

/// A single node in the code tree.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CodeNode {
    /// Unique identifier within the tree (index into the flat arena).
    pub id: usize,
    pub kind: NodeKind,
    /// The grammar's word for the construct, when it is one: `trait`,
    /// `interface`, `method`. Shown, never branched on.
    pub noun: Option<&'static str>,
    /// Display name (symbol name, file name, line content, …)
    pub name: String,
    /// Optional detail text shown in a secondary column.
    pub detail: Option<String>,
    /// Source byte range in the original file: (start_byte, end_byte).
    pub byte_range: (usize, usize),
    /// Source line range: (first_line, last_line), 0-indexed.
    pub line_range: (usize, usize),
    /// Depth in the tree (0 = root).
    pub depth: usize,
    /// Index of parent node, None for the root.
    pub parent: Option<usize>,
    /// Indices of direct children (in insertion order).
    pub children: Vec<usize>,
    /// Whether this node is currently collapsed (all children hidden).
    pub collapsed: bool,
    /// Granularity limit: only show children whose kind is at most this
    /// level of detail. `None` means "show all children".
    pub granularity_limit: Option<NodeKind>,
}

impl CodeNode {
    pub fn new(
        id: usize,
        kind: NodeKind,
        name: impl Into<String>,
        byte_range: (usize, usize),
        line_range: (usize, usize),
        depth: usize,
        parent: Option<usize>,
    ) -> Self {
        CodeNode {
            id,
            kind,
            noun: None,
            name: name.into(),
            detail: None,
            byte_range,
            line_range,
            depth,
            parent,
            children: Vec::new(),
            collapsed: false,
            granularity_limit: None,
        }
    }

    /// True when this node has no children.
    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }

    pub fn full_display_name(&self) -> String {
        self.name.clone()
    }
}

/// Arena-allocated tree of code nodes for a single file or directory workspace.
///
/// The model has two structural layers:
///
/// 1. **Contains topology** — the parent/child tree stored in `nodes`.  Each
///    node's [`CodeNode::parent`] and [`CodeNode::children`] fields record the
///    hierarchical containment relationships (folder → file → class →
///    function → …).
///
/// 2. **Reference graph** — the [`ReferenceGraph`] stored in `references`.
///    Each edge records a directed symbolic relationship between two nodes
///    (calls, imports, type usage, …).  Use
///    [`CodeTree::aggregate_refs_at_granularity`] to collapse fine-grained
///    edges to any desired resolution level, or
///    [`CodeTree::project_refs_onto_visible`] to dynamically project edges
///    onto whichever nodes are currently expanded in the viewer.
#[derive(Debug, Default)]
pub struct CodeTree {
    /// Flat storage; index == node.id.
    pub(crate) nodes: Vec<CodeNode>,
    /// Root node index (always 0 when the tree is non-empty).
    pub root: Option<usize>,
    /// Directed symbolic references between nodes (the reference graph).
    pub references: ReferenceGraph,
}

impl CodeTree {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a new node and return its id.
    pub fn add_node(
        &mut self,
        kind: NodeKind,
        name: impl Into<String>,
        byte_range: (usize, usize),
        line_range: (usize, usize),
        depth: usize,
        parent: Option<usize>,
    ) -> usize {
        let id = self.nodes.len();
        let node = CodeNode::new(id, kind, name, byte_range, line_range, depth, parent);
        self.nodes.push(node);
        if let Some(pid) = parent {
            self.nodes[pid].children.push(id);
        }
        if self.root.is_none() {
            self.root = Some(id);
        }
        id
    }

    pub fn get(&self, id: usize) -> Option<&CodeNode> {
        self.nodes.get(id)
    }

    pub fn get_mut(&mut self, id: usize) -> Option<&mut CodeNode> {
        self.nodes.get_mut(id)
    }

    /// Toggle the collapsed state of a node.
    pub fn toggle_collapse(&mut self, id: usize) {
        if let Some(n) = self.nodes.get_mut(id) {
            n.collapsed = !n.collapsed;
        }
    }

    /// Expand the granularity of node `id`: show one finer level of children.
    ///
    /// If currently showing all children (no limit), this is a no-op.
    /// If there is a limit, it moves one step finer; reaching the finest
    /// level removes the limit entirely (show everything).
    pub fn expand_granularity(&mut self, id: usize) {
        if let Some(node) = self.nodes.get_mut(id) {
            match &node.granularity_limit {
                None => {} // Already at maximum detail.
                Some(current) => {
                    // Move one step finer; None means "remove limit" (show all).
                    node.granularity_limit = current.finer();
                    node.collapsed = false;
                }
            }
        }
    }

    /// Shrink the granularity of node `id`: hide one finer level of children.
    ///
    /// Steps from "show all" → Block → Function → Class → File → Module →
    /// Folder → collapsed.
    pub fn shrink_granularity(&mut self, id: usize) {
        if let Some(node) = self.nodes.get_mut(id) {
            // Clone the current limit to avoid a simultaneous borrow conflict.
            let current_limit = node.granularity_limit.clone();
            match current_limit {
                None => {
                    // Currently showing all — hide Lines.
                    node.granularity_limit = Some(NodeKind::Block);
                }
                Some(current) => {
                    match current.coarser() {
                        Some(coarser) => {
                            node.granularity_limit = Some(coarser);
                        }
                        None => {
                            // Already at Folder level — collapse completely.
                            node.collapsed = true;
                            node.granularity_limit = None;
                        }
                    }
                }
            }
        }
    }

    /// Set detail text for a node.
    #[allow(dead_code)]
    pub fn set_detail(&mut self, id: usize, detail: impl Into<String>) {
        if let Some(n) = self.nodes.get_mut(id) {
            n.detail = Some(detail.into());
        }
    }

    /// Return the visible nodes in DFS pre-order, respecting `collapsed` and
    /// `granularity_limit` flags.
    pub fn visible_nodes(&self) -> Vec<&CodeNode> {
        let Some(root) = self.root else {
            return Vec::new();
        };
        let mut result = Vec::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let node = &self.nodes[id];
            result.push(node);
            if !node.collapsed {
                for &child in node.children.iter().rev() {
                    let child_node = &self.nodes[child];
                    // Respect the per-node granularity limit.
                    if let Some(ref limit) = node.granularity_limit {
                        if child_node.kind.is_finer_than(limit) {
                            continue;
                        }
                    }
                    stack.push(child);
                }
            }
        }
        result
    }

    /// Return all nodes (visible or not) in DFS pre-order.
    pub fn all_nodes_dfs(&self) -> Vec<&CodeNode> {
        let Some(root) = self.root else {
            return Vec::new();
        };
        let mut result = Vec::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let node = &self.nodes[id];
            result.push(node);
            for &child in node.children.iter().rev() {
                stack.push(child);
            }
        }
        result
    }

    /// Filter visible nodes by a text pattern (case-insensitive substring match
    /// on name and detail).
    pub fn filter_visible<'a>(&'a self, pattern: &str) -> Vec<&'a CodeNode> {
        let pat = pattern.to_ascii_lowercase();
        self.visible_nodes()
            .into_iter()
            .filter(|n| {
                n.name.to_ascii_lowercase().contains(&pat)
                    || n.detail
                        .as_deref()
                        .map(|d| d.to_ascii_lowercase().contains(&pat))
                        .unwrap_or(false)
            })
            .collect()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    // -----------------------------------------------------------------------
    // Reference graph helpers
    // -----------------------------------------------------------------------

    /// Add a directed symbolic reference from `from` to `to` occurring at
    /// 0-indexed `line` of `from`'s file.
    pub fn add_reference_at(&mut self, from: usize, to: usize, kind: ReferenceKind, line: usize) {
        self.references.add_reference_at(from, to, kind, line);
    }

    /// [`add_reference_at`](Self::add_reference_at) with no known line; for
    /// hand-built trees in tests.
    #[cfg(test)]
    pub fn add_reference(&mut self, from: usize, to: usize, kind: ReferenceKind) {
        self.add_reference_at(from, to, kind, 0);
    }

    /// Walk the parent chain of `node_id` and return the nearest ancestor
    /// (inclusive) whose [`NodeKind`] is at or coarser than `granularity`.
    ///
    /// Returns `None` only when `node_id` is invalid.
    #[allow(dead_code)]
    pub fn ancestor_at_granularity(
        &self,
        node_id: usize,
        granularity: &NodeKind,
    ) -> Option<usize> {
        let gran_level = granularity.level();
        let mut current_id = node_id;
        loop {
            let node = self.nodes.get(current_id)?;
            if node.kind.level() <= gran_level {
                return Some(current_id);
            }
            match node.parent {
                Some(pid) => current_id = pid,
                None => return Some(current_id), // root — use as best effort
            }
        }
    }

    /// Aggregate all reference edges at the requested `granularity` level.
    ///
    /// For every edge `(from, to)` in the [`ReferenceGraph`], this method
    /// walks up the contains hierarchy for *both* endpoints to find the
    /// nearest ancestor at or coarser than `granularity`.  The resulting
    /// `(ancestor_from, ancestor_to)` pairs are deduplicated so that — for
    /// example — many function-level calls between two files are collapsed
    /// into a single file-level edge.
    ///
    /// Self-loops (where both endpoints resolve to the same ancestor) are
    /// dropped.  Edges whose endpoints cannot be resolved (invalid IDs) are
    /// also dropped.
    ///
    /// # Example
    ///
    /// At [`NodeKind::File`] granularity, every call from any function inside
    /// `file_a` to any symbol inside `file_b` is reported as the single pair
    /// `(file_a_id, file_b_id)`.
    #[allow(dead_code)]
    pub fn aggregate_refs_at_granularity(
        &self,
        granularity: &NodeKind,
    ) -> Vec<(usize, usize)> {
        use std::collections::HashSet;
        let mut seen: HashSet<(usize, usize)> = HashSet::new();
        let mut result = Vec::new();
        for edge in self.references.references() {
            let Some(from_anc) = self.ancestor_at_granularity(edge.from, granularity) else {
                continue;
            };
            let Some(to_anc) = self.ancestor_at_granularity(edge.to, granularity) else {
                continue;
            };
            if from_anc == to_anc {
                continue; // drop self-loops
            }
            if seen.insert((from_anc, to_anc)) {
                result.push((from_anc, to_anc));
            }
        }
        result
    }

    /// Walk the parent chain of `node_id` upward and return the first ancestor
    /// (inclusive) that appears in `visible_set`.
    ///
    /// Returns `None` if neither `node_id` nor any ancestor is visible (e.g.
    /// the whole subtree is collapsed above the root).
    fn first_visible_ancestor(
        &self,
        node_id: usize,
        visible_set: &std::collections::HashSet<usize>,
    ) -> Option<usize> {
        let mut current = node_id;
        loop {
            if visible_set.contains(&current) {
                return Some(current);
            }
            let node = self.nodes.get(current)?;
            match node.parent {
                Some(pid) => current = pid,
                None => return None,
            }
        }
    }

    /// Project every reference edge onto the set of currently visible nodes.
    ///
    /// For each edge `(from, to)` in the [`ReferenceGraph`], both endpoints
    /// are walked up the contains hierarchy until a visible node is reached.
    /// This handles **mixed granularity**: if one file is expanded (its
    /// functions are visible) while the other is collapsed (only the file node
    /// is visible), the edge appears as `(fn_a, file_b)` rather than
    /// `(file_a, file_b)`.
    ///
    /// Self-loops and edges where either endpoint has no visible ancestor are
    /// dropped.  The result is deduplicated.
    ///
    /// Visible ancestors are cached per unique endpoint ID so that nodes
    /// referenced by many edges (e.g. a widely-used function) only pay the
    /// parent-chain traversal cost once.
    pub fn project_refs_onto_visible(&self, visible_ids: &[usize]) -> Vec<(usize, usize)> {
        use std::collections::{HashMap, HashSet};
        let visible_set: HashSet<usize> = visible_ids.iter().copied().collect();

        // Pre-compute the visible ancestor for each unique referenced node so that
        // nodes appearing in many edges are traversed only once.
        let mut ancestor_cache: HashMap<usize, Option<usize>> = HashMap::new();
        for edge in self.references.references() {
            for &node_id in &[edge.from, edge.to] {
                ancestor_cache
                    .entry(node_id)
                    .or_insert_with(|| self.first_visible_ancestor(node_id, &visible_set));
            }
        }

        let mut seen: HashSet<(usize, usize)> = HashSet::new();
        let mut result = Vec::new();
        for edge in self.references.references() {
            let (Some(from_vis), Some(to_vis)) = (
                ancestor_cache.get(&edge.from).copied().flatten(),
                ancestor_cache.get(&edge.to).copied().flatten(),
            ) else {
                continue;
            };
            if from_vis != to_vis && seen.insert((from_vis, to_vis)) {
                result.push((from_vis, to_vis));
            }
        }
        result
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn nodes_iter(&self) -> impl Iterator<Item = &CodeNode> {
        self.nodes.iter()
    }

    /// Collect all node IDs in the subtree rooted at root_id.
    pub fn subtree_ids(&self, root_id: usize) -> std::collections::HashSet<usize> {
        let mut result = std::collections::HashSet::new();
        let mut stack = vec![root_id];
        while let Some(id) = stack.pop() {
            result.insert(id);
            if let Some(node) = self.nodes.get(id) {
                for &child in &node.children {
                    stack.push(child);
                }
            }
        }
        result
    }

    /// Count reference edges from the subtree of `from_root` to the subtree of `to_root`.
    pub fn cross_ref_count(&self, from_root: usize, to_root: usize) -> usize {
        let from_set = self.subtree_ids(from_root);
        let to_set = self.subtree_ids(to_root);
        self.references
            .references()
            .iter()
            .filter(|r| from_set.contains(&r.from) && to_set.contains(&r.to))
            .count()
    }

    /// Compute the full path of a node from the tree root (e.g. "src/app/state.rs").
    pub fn full_path(&self, node_id: usize) -> String {
        let mut parts = Vec::new();
        let mut current = node_id;
        loop {
            if let Some(node) = self.nodes.get(current) {
                parts.push(node.name.clone());
                match node.parent {
                    Some(pid) => current = pid,
                    None => break,
                }
            } else {
                break;
            }
        }
        parts.reverse();
        parts.join("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tree() -> CodeTree {
        let mut tree = CodeTree::new();
        let root = tree.add_node(NodeKind::File, "main.rs", (0, 100), (0, 20), 0, None);
        let fn1 = tree.add_node(NodeKind::Function, "fn_foo", (0, 50), (0, 10), 1, Some(root));
        let _ln1 = tree.add_node(NodeKind::Line, "let x = 1;", (0, 20), (0, 0), 2, Some(fn1));
        let _fn2 = tree.add_node(NodeKind::Function, "fn_bar", (51, 100), (11, 20), 1, Some(root));
        tree
    }

    #[test]
    fn test_tree_structure() {
        let tree = sample_tree();
        assert_eq!(tree.len(), 4);
        assert_eq!(tree.root, Some(0));

        let root = tree.get(0).unwrap();
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.kind, NodeKind::File);
    }

    #[test]
    fn test_visible_nodes_full() {
        let tree = sample_tree();
        let vis = tree.visible_nodes();
        assert_eq!(vis.len(), 4);
    }

    #[test]
    fn test_collapse_hides_subtree() {
        let mut tree = sample_tree();
        tree.toggle_collapse(1);
        let vis = tree.visible_nodes();
        assert_eq!(vis.len(), 3); // root, fn_foo, fn_bar (line hidden)
    }

    #[test]
    fn test_collapse_root_hides_all_children() {
        let mut tree = sample_tree();
        tree.toggle_collapse(0);
        let vis = tree.visible_nodes();
        assert_eq!(vis.len(), 1); // only root
    }

    #[test]
    fn test_double_toggle_restores() {
        let mut tree = sample_tree();
        tree.toggle_collapse(1);
        tree.toggle_collapse(1);
        assert_eq!(tree.visible_nodes().len(), 4);
    }

    #[test]
    fn test_granularity_limit_hides_fine_children() {
        let mut tree = sample_tree();
        // Set fn_foo (id=1) to only show children up to Function level.
        // fn_foo's direct children are Lines (level 6 > Function level 4) → hidden.
        tree.get_mut(1).unwrap().granularity_limit = Some(NodeKind::Function);
        let vis = tree.visible_nodes();
        // root + fn_foo (its Line child hidden) + fn_bar = 3
        assert_eq!(vis.len(), 3);
        assert!(vis.iter().all(|n| n.kind != NodeKind::Line));
    }

    #[test]
    fn test_granularity_limit_none_shows_all() {
        let mut tree = sample_tree();
        tree.get_mut(0).unwrap().granularity_limit = None;
        assert_eq!(tree.visible_nodes().len(), 4);
    }

    #[test]
    fn test_shrink_granularity_from_none() {
        let mut tree = sample_tree();
        // Shrink from None → Block limit (hide Lines)
        tree.shrink_granularity(0);
        let node = tree.get(0).unwrap();
        assert_eq!(node.granularity_limit, Some(NodeKind::Block));
    }

    #[test]
    fn test_shrink_granularity_from_block_to_function() {
        let mut tree = sample_tree();
        tree.get_mut(0).unwrap().granularity_limit = Some(NodeKind::Block);
        tree.shrink_granularity(0);
        let node = tree.get(0).unwrap();
        assert_eq!(node.granularity_limit, Some(NodeKind::Function));
    }

    #[test]
    fn test_expand_granularity_from_function_to_block() {
        let mut tree = sample_tree();
        tree.get_mut(0).unwrap().granularity_limit = Some(NodeKind::Function);
        tree.expand_granularity(0);
        let node = tree.get(0).unwrap();
        assert_eq!(node.granularity_limit, Some(NodeKind::Block));
    }

    #[test]
    fn test_expand_granularity_from_block_to_line() {
        // Block.finer() = Some(Line), so expanding from Block gives Line limit.
        let mut tree = sample_tree();
        tree.get_mut(0).unwrap().granularity_limit = Some(NodeKind::Block);
        tree.expand_granularity(0);
        let node = tree.get(0).unwrap();
        assert_eq!(node.granularity_limit, Some(NodeKind::Line));
    }

    #[test]
    fn test_expand_granularity_from_line_to_none() {
        // Line.finer() = None, so expanding from Line removes the limit.
        let mut tree = sample_tree();
        tree.get_mut(0).unwrap().granularity_limit = Some(NodeKind::Line);
        tree.expand_granularity(0);
        let node = tree.get(0).unwrap();
        assert_eq!(node.granularity_limit, None);
    }

    #[test]
    fn test_expand_granularity_from_none_noop() {
        let mut tree = sample_tree();
        tree.expand_granularity(0);
        assert_eq!(tree.get(0).unwrap().granularity_limit, None);
    }

    #[test]
    fn test_filter_visible() {
        let tree = sample_tree();
        let res = tree.filter_visible("foo");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "fn_foo");
    }

    #[test]
    fn test_filter_visible_empty_pattern() {
        let tree = sample_tree();
        assert_eq!(tree.filter_visible("").len(), 4);
    }

    #[test]
    fn test_filter_case_insensitive() {
        let tree = sample_tree();
        assert_eq!(tree.filter_visible("FN_FOO").len(), 1);
    }

    #[test]
    fn test_node_is_leaf() {
        let tree = sample_tree();
        assert!(!tree.get(0).unwrap().is_leaf());
        assert!(tree.get(2).unwrap().is_leaf()); // Line node
    }

    #[test]
    fn test_nodekind_level_ordering() {
        assert!(NodeKind::Folder.level() < NodeKind::Line.level());
        assert!(NodeKind::Function.level() < NodeKind::Block.level());
    }

    #[test]
    fn test_nodekind_coarser_finer() {
        assert_eq!(NodeKind::Function.coarser(), Some(NodeKind::Class));
        assert_eq!(NodeKind::Function.finer(), Some(NodeKind::Block));
        assert_eq!(NodeKind::Folder.coarser(), None);
        assert_eq!(NodeKind::Line.finer(), None);
    }

    #[test]
    fn test_is_finer_than() {
        assert!(NodeKind::Line.is_finer_than(&NodeKind::Function));
        assert!(!NodeKind::File.is_finer_than(&NodeKind::Function));
    }

    // -----------------------------------------------------------------------
    // Reference graph tests
    // -----------------------------------------------------------------------

    /// Build a two-file tree for reference-graph tests:
    ///
    /// ```text
    /// Folder "root"            (id 0)
    ///   File "a.rs"            (id 1)
    ///     Function "fn_a1"     (id 2)
    ///     Function "fn_a2"     (id 3)
    ///   File "b.rs"            (id 4)
    ///     Function "fn_b1"     (id 5)
    ///     Function "fn_b2"     (id 6)
    /// ```
    fn two_file_tree() -> CodeTree {
        let mut tree = CodeTree::new();
        let root = tree.add_node(NodeKind::Folder, "root", (0, 200), (0, 40), 0, None);
        let file_a = tree.add_node(NodeKind::File, "a.rs", (0, 100), (0, 20), 1, Some(root));
        let _fn_a1 = tree.add_node(NodeKind::Function, "fn_a1", (0, 50), (0, 10), 2, Some(file_a));
        let _fn_a2 = tree.add_node(NodeKind::Function, "fn_a2", (51, 100), (11, 20), 2, Some(file_a));
        let file_b = tree.add_node(NodeKind::File, "b.rs", (101, 200), (21, 40), 1, Some(root));
        let _fn_b1 = tree.add_node(NodeKind::Function, "fn_b1", (101, 150), (21, 30), 2, Some(file_b));
        let _fn_b2 = tree.add_node(NodeKind::Function, "fn_b2", (151, 200), (31, 40), 2, Some(file_b));
        tree
    }

    #[test]
    fn test_add_reference_and_query() {
        let mut tree = two_file_tree();
        // fn_a1 (2) calls fn_b1 (5)
        tree.add_reference(2, 5, ReferenceKind::Call);
        let refs = tree.references.references();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].from, 2);
        assert_eq!(refs[0].to, 5);
        assert_eq!(refs[0].kind, ReferenceKind::Call);
    }

    #[test]
    fn test_refs_from_and_to() {
        let mut tree = two_file_tree();
        tree.add_reference(2, 5, ReferenceKind::Call);
        tree.add_reference(3, 5, ReferenceKind::Import);

        let from_2 = tree.references.refs_from(2);
        assert_eq!(from_2.len(), 1);
        assert_eq!(from_2[0].to, 5);

        let to_5 = tree.references.refs_to(5);
        assert_eq!(to_5.len(), 2);

        assert!(tree.references.refs_from(5).is_empty());
    }

    #[test]
    fn test_ancestor_at_granularity_same_level() {
        let tree = two_file_tree();
        // fn_a1 is already at Function level — querying at Function returns itself.
        assert_eq!(tree.ancestor_at_granularity(2, &NodeKind::Function), Some(2));
    }

    #[test]
    fn test_ancestor_at_granularity_walks_up() {
        let tree = two_file_tree();
        // fn_a1 (id=2, Function) → at File granularity should give file_a (id=1).
        assert_eq!(tree.ancestor_at_granularity(2, &NodeKind::File), Some(1));
        // fn_a1 (id=2) → at Folder granularity should give root (id=0).
        assert_eq!(tree.ancestor_at_granularity(2, &NodeKind::Folder), Some(0));
    }

    #[test]
    fn test_ancestor_at_granularity_coarser_node() {
        let tree = two_file_tree();
        // file_a is at File level; querying at Folder should give root (id=0).
        assert_eq!(tree.ancestor_at_granularity(1, &NodeKind::Folder), Some(0));
        // root is a Folder; querying at Folder should return root itself (id=0).
        assert_eq!(tree.ancestor_at_granularity(0, &NodeKind::Folder), Some(0));
    }

    #[test]
    fn test_aggregate_refs_at_file_granularity_deduplicates() {
        let mut tree = two_file_tree();
        // Two function-level calls from a.rs to b.rs.
        tree.add_reference(2, 5, ReferenceKind::Call); // fn_a1 → fn_b1
        tree.add_reference(3, 6, ReferenceKind::Call); // fn_a2 → fn_b2

        // At File granularity both collapse to (file_a=1, file_b=4).
        let agg = tree.aggregate_refs_at_granularity(&NodeKind::File);
        assert_eq!(agg.len(), 1);
        assert!(agg.contains(&(1, 4)));
    }

    #[test]
    fn test_aggregate_refs_at_function_granularity_keeps_distinct() {
        let mut tree = two_file_tree();
        tree.add_reference(2, 5, ReferenceKind::Call); // fn_a1 → fn_b1
        tree.add_reference(3, 6, ReferenceKind::Call); // fn_a2 → fn_b2

        // At Function granularity the edges stay separate.
        let agg = tree.aggregate_refs_at_granularity(&NodeKind::Function);
        assert_eq!(agg.len(), 2);
        assert!(agg.contains(&(2, 5)));
        assert!(agg.contains(&(3, 6)));
    }

    #[test]
    fn test_aggregate_refs_drops_self_loops() {
        let mut tree = two_file_tree();
        // Both functions are inside a.rs (id=1); at File level this is a self-loop.
        tree.add_reference(2, 3, ReferenceKind::Call); // fn_a1 → fn_a2

        let agg = tree.aggregate_refs_at_granularity(&NodeKind::File);
        assert!(agg.is_empty());

        // At Function granularity it remains a real edge (2 != 3).
        let agg_fn = tree.aggregate_refs_at_granularity(&NodeKind::Function);
        assert_eq!(agg_fn.len(), 1);
        assert!(agg_fn.contains(&(2, 3)));
    }

    #[test]
    fn test_aggregate_refs_mixed_granularity() {
        let mut tree = two_file_tree();
        // One cross-file call and one intra-file call.
        tree.add_reference(2, 5, ReferenceKind::Call); // fn_a1 → fn_b1 (cross-file)
        tree.add_reference(2, 3, ReferenceKind::Call); // fn_a1 → fn_a2 (intra-file)

        let agg = tree.aggregate_refs_at_granularity(&NodeKind::File);
        // Only the cross-file edge survives; intra-file becomes self-loop and is dropped.
        assert_eq!(agg.len(), 1);
        assert!(agg.contains(&(1, 4)));
    }

    #[test]
    fn test_reference_kind_display() {
        assert_eq!(ReferenceKind::Call.to_string(), "call");
        assert_eq!(ReferenceKind::Import.to_string(), "import");
        assert_eq!(ReferenceKind::TypeRef.to_string(), "type_ref");
        assert_eq!(ReferenceKind::VarRef.to_string(), "var_ref");
        assert_eq!(ReferenceKind::Generic.to_string(), "generic");
    }

    // -----------------------------------------------------------------------
    // project_refs_onto_visible tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_project_refs_both_collapsed_gives_file_edge() {
        let mut tree = two_file_tree();
        // fn_a1 (2) → fn_b1 (5); both files collapsed → only folder(0), file_a(1), file_b(4) visible.
        tree.add_reference(2, 5, ReferenceKind::Call);
        let visible = vec![0usize, 1, 4]; // folder, file_a, file_b (functions collapsed)
        let proj = tree.project_refs_onto_visible(&visible);
        assert_eq!(proj.len(), 1);
        assert!(proj.contains(&(1, 4)));
    }

    #[test]
    fn test_project_refs_one_expanded_gives_mixed_edge() {
        let mut tree = two_file_tree();
        // fn_a1 (2) → fn_b1 (5); file_a expanded, file_b collapsed.
        tree.add_reference(2, 5, ReferenceKind::Call);
        let visible = vec![0usize, 1, 2, 3, 4]; // folder, file_a, fn_a1, fn_a2, file_b
        let proj = tree.project_refs_onto_visible(&visible);
        assert_eq!(proj.len(), 1);
        // fn_a1 is visible, fn_b1 is not → walks up to file_b.
        assert!(proj.contains(&(2, 4)));
    }

    #[test]
    fn test_project_refs_both_expanded_gives_function_edge() {
        let mut tree = two_file_tree();
        tree.add_reference(2, 5, ReferenceKind::Call);
        let visible = vec![0usize, 1, 2, 3, 4, 5, 6]; // all nodes
        let proj = tree.project_refs_onto_visible(&visible);
        assert_eq!(proj.len(), 1);
        assert!(proj.contains(&(2, 5)));
    }

    #[test]
    fn test_project_refs_intra_file_drops_self_loop_when_collapsed() {
        let mut tree = two_file_tree();
        // fn_a1 (2) → fn_a2 (3); both in file_a; when file_a is collapsed both map to file_a.
        tree.add_reference(2, 3, ReferenceKind::Call);
        let visible = vec![0usize, 1, 4]; // file_a collapsed
        let proj = tree.project_refs_onto_visible(&visible);
        // Both endpoints project to file_a (1) → self-loop, dropped.
        assert!(proj.is_empty());
    }

    #[test]
    fn test_project_refs_deduplicates() {
        let mut tree = two_file_tree();
        // Multiple function-level edges from a→b; all should deduplicate.
        tree.add_reference(2, 5, ReferenceKind::Call);
        tree.add_reference(3, 6, ReferenceKind::Call);
        let visible = vec![0usize, 1, 4]; // both files collapsed
        let proj = tree.project_refs_onto_visible(&visible);
        assert_eq!(proj.len(), 1);
    }
}
