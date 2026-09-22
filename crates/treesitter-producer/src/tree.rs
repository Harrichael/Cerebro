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
        }
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

    /// Set detail text for a node.
    #[allow(dead_code)]
    pub fn set_detail(&mut self, id: usize, detail: impl Into<String>) {
        if let Some(n) = self.nodes.get_mut(id) {
            n.detail = Some(detail.into());
        }
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

    pub fn node_count(&self) -> usize {
        self.nodes.len()
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
    fn test_nodekind_level_ordering() {
        assert!(NodeKind::Folder.level() < NodeKind::Line.level());
        assert!(NodeKind::Function.level() < NodeKind::Block.level());
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

}
