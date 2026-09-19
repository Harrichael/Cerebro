//! Converts the parsed [`CodeTree`] into an [`EntityGraph`].
//!
//! The [`CodeTree`] produced by the parser contains all granularity levels
//! (Folder → Module → File → Class → Function → Block → Line).  The
//! [`EntityGraph`] only models the coarser levels (Folder → Module → File →
//! Class → Function); Block and Line nodes are filtered out and their
//! reference edges are remapped to the nearest function-or-coarser ancestor.
//!
//! Additionally, Rust `mod.rs` files that contain only `pub mod` / `mod`
//! declarations (no functions or classes) are treated as **glue modules** and
//! omitted from the entity graph.  Their bare stub `Module` children (the
//! `pub mod foo;` declaration nodes, which have no body and therefore no
//! CodeTree children) are also omitted because they duplicate the real
//! `Folder` / `File` nodes that represent each sub-module.

use std::collections::{HashMap, HashSet};

use crate::tree::{CodeTree, NodeKind, ReferenceKind};
use entity_graph::{
    Entity, EntityGraph, EntityId, EntityKind,
    Reference as GraphReference, ReferenceKind as GraphReferenceKind, Site,
};

/// Build an [`EntityGraph`] from a fully-parsed [`CodeTree`].
///
/// Only structural nodes up to [`NodeKind::Function`] are translated;
/// Block and Line nodes are skipped.  References that originate
/// from or target finer-grained nodes are remapped upward to the nearest
/// entity-level ancestor so no symbolic edges are lost.
pub fn code_tree_to_entity_graph(tree: &CodeTree) -> EntityGraph {
    let is_entity_kind = |k: &NodeKind| {
        matches!(
            k,
            NodeKind::Folder
                | NodeKind::Module
                | NodeKind::File
                | NodeKind::Class
                | NodeKind::Function
        )
    };

    // ------------------------------------------------------------------
    // Pre-pass: Identify Rust "glue" mod.rs files.
    //
    // A glue mod.rs contains only `pub mod` / `mod` declarations and
    // `use` re-exports — no classes or functions.  In the CodeTree these
    // files have no Class or Function children.  We skip:
    //
    //   1. The File node for the glue mod.rs itself.
    //   2. Its direct Module children that are declaration-only stubs
    //      (i.e., `pub mod foo;` with no body → no CodeTree children).
    //
    // This keeps the entity graph uncluttered: the real sub-module
    // entities are the Folder / File nodes added by the directory walker.
    // ------------------------------------------------------------------
    let mut glue_ids: HashSet<usize> = HashSet::new();

    for code_id in 0..tree.node_count() {
        if !is_glue_mod_rs(code_id, tree) {
            continue;
        }
        glue_ids.insert(code_id);
        // Also skip stub Module children (pub mod foo; with no body).
        let children = tree.get(code_id).map(|n| n.children.clone()).unwrap_or_default();
        for child_id in children {
            let Some(child) = tree.get(child_id) else { continue };
            if child.kind == NodeKind::Module && child.children.is_empty() {
                glue_ids.insert(child_id);
            }
        }
    }

    // ------------------------------------------------------------------
    // Pass 1: Create entity stubs for every qualifying structural node.
    // We keep a mapping code_node_id → EntityId for use in later passes.
    // ------------------------------------------------------------------
    let mut id_map: HashMap<usize, EntityId> = HashMap::new();
    let mut entities: Vec<Entity> = Vec::new();

    for code_id in 0..tree.node_count() {
        if let Some(node) = tree.get(code_id) {
            if !is_entity_kind(&node.kind) {
                continue;
            }
            // Skip glue mod.rs files and their stub Module children.
            if glue_ids.contains(&code_id) {
                continue;
            }
            let entity_id = EntityId(entities.len());
            id_map.insert(code_id, entity_id);

            let kind = match node.kind {
                NodeKind::Folder => EntityKind::Folder,
                NodeKind::Module => EntityKind::Module,
                NodeKind::File => EntityKind::File,
                NodeKind::Class => EntityKind::Class,
                NodeKind::Function => EntityKind::Function,
                _ => unreachable!(),
            };

            // Build the full display path by walking up the ancestor chain so
            // the UI can show e.g. "src/graph/entity.rs" or
            // "src/graph/cursor.rs/Cursor" instead of bare symbol names.
            let path = build_node_path(code_id, tree);

            entities.push(Entity {
                id: entity_id,
                kind,
                noun: node.noun,
                name: node.name.clone(),
                parent: None,       // filled in pass 2
                children: Vec::new(), // filled in pass 2
                path,
                byte_range: node.byte_range.0..node.byte_range.1,
                line_range: node.line_range.0..node.line_range.1,
                is_test: false,
            });
        }
    }

    // ------------------------------------------------------------------
    // Pass 2: Wire parent / children relationships.
    // For a node whose direct parent is not in the entity graph (e.g. a
    // Function whose parent is a Block), we walk up until we find an
    // entity-level ancestor.
    // ------------------------------------------------------------------
    for code_id in 0..tree.node_count() {
        if let Some(&entity_id) = id_map.get(&code_id) {
            if let Some(node) = tree.get(code_id) {
                // Parent: walk up to find nearest entity ancestor.
                let parent_entity_id =
                    node.parent.and_then(|p| find_entity_ancestor(p, tree, &id_map));
                entities[entity_id.0].parent = parent_entity_id;

                // Children: direct entity children, or nearest entity descendants
                // when an intermediate Block/Line node is present.
                let children = collect_entity_children(code_id, tree, &id_map);
                entities[entity_id.0].children = children;
            }
        }
    }

    // ------------------------------------------------------------------
    // Pass 3: Convert reference edges.
    // Remap both endpoints to their nearest entity-level ancestor so that
    // references inside Block/Line nodes still appear at the Function level.
    // Self-loops produced by remapping are discarded, and the remaining edges
    // are deduplicated on (from, to, kind): a Call and an Import between the
    // same pair are distinct facts a consumer may want to tell apart. Repeats
    // of one triple accumulate their occurrence lines onto the single edge.
    //
    // References where the raw `from` or `to` code node is itself a glue
    // node (a glue mod.rs File or one of its declaration-only Module stubs)
    // are dropped entirely.  These are structural plumbing edges (pub mod,
    // pub use re-exports) that, if kept, remap to the parent Folder entity
    // and leave stale cursor leaf pointers after the user expands that
    // folder — which causes the folder node to reappear in the view tree
    // even though it has been expanded past.
    // ------------------------------------------------------------------
    let mut index: HashMap<(EntityId, EntityId, GraphReferenceKind), usize> = HashMap::new();
    let mut references: Vec<GraphReference> = Vec::new();

    for r in tree.references.references() {
        // Drop references that touch glue mod.rs nodes or their stubs.
        if glue_ids.contains(&r.from) || glue_ids.contains(&r.to) {
            continue;
        }
        let from = match find_entity_ancestor_or_self(r.from, tree, &id_map) {
            Some(id) => id,
            None => continue,
        };
        let to = match find_entity_ancestor_or_self(r.to, tree, &id_map) {
            Some(id) => id,
            None => continue,
        };
        if from == to {
            continue;
        }
        let kind = match r.kind {
            ReferenceKind::Call => GraphReferenceKind::Call,
            ReferenceKind::Import => GraphReferenceKind::Import,
            ReferenceKind::TypeRef => GraphReferenceKind::TypeRef,
            ReferenceKind::VarRef => GraphReferenceKind::VarRef,
            ReferenceKind::Generic => GraphReferenceKind::Generic,
        };
        let site = Site { line: r.line };
        match index.get(&(from, to, kind)) {
            Some(&i) => references[i].sites.push(site),
            None => {
                index.insert((from, to, kind), references.len());
                references.push(GraphReference { from, to, kind, sites: vec![site] });
            }
        }
    }
    for r in &mut references {
        r.sites.sort();
        r.sites.dedup();
    }

    EntityGraph { entities, references }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Return `true` if `code_id` is a Rust "glue" `mod.rs` file — a `File` node
/// named `"mod.rs"` whose only entity-kind children are Module declaration
/// stubs (`pub mod foo;` / `mod foo;`) with no functions or classes.
///
/// Files that contain real implementations (functions, structs, etc.) return
/// `false` and are kept in the entity graph as normal.
fn is_glue_mod_rs(code_id: usize, tree: &CodeTree) -> bool {
    let Some(node) = tree.get(code_id) else { return false };
    if node.kind != NodeKind::File || node.name != "mod.rs" {
        return false;
    }
    // Not glue if any direct child is a Class or Function.
    node.children.iter()
        .all(|&c| {
            !matches!(
                tree.get(c).map(|n| &n.kind),
                Some(NodeKind::Class) | Some(NodeKind::Function)
            )
        })
}

/// Build a display path for `code_id` by walking up the containment hierarchy
/// and joining entity-level ancestor names with `/`.
///
/// Only Folder, Module, File, Class, and Function nodes contribute a path
/// segment; Block and Line nodes are skipped.  Examples:
///
/// - `src/` folder         → `src`
/// - `src/graph/entity.rs` → `src/graph/entity.rs`
/// - `Cursor` struct       → `src/graph/cursor.rs/Cursor`
fn build_node_path(code_id: usize, tree: &CodeTree) -> std::path::PathBuf {
    let is_entity = |k: &NodeKind| {
        matches!(
            k,
            NodeKind::Folder
                | NodeKind::Module
                | NodeKind::File
                | NodeKind::Class
                | NodeKind::Function
        )
    };
    let mut parts: Vec<String> = Vec::new();
    let mut current = Some(code_id);
    while let Some(id) = current {
        match tree.get(id) {
            Some(node) => {
                if is_entity(&node.kind) {
                    parts.push(node.name.clone());
                }
                current = node.parent;
            }
            None => break,
        }
    }
    parts.reverse();
    std::path::PathBuf::from(parts.join("/"))
}

/// Walk up the containment hierarchy from `code_id` (exclusive) until we
/// reach a node that has an [`EntityId`] mapping.
fn find_entity_ancestor(
    mut code_id: usize,
    tree: &CodeTree,
    id_map: &HashMap<usize, EntityId>,
) -> Option<EntityId> {
    loop {
        if let Some(&eid) = id_map.get(&code_id) {
            return Some(eid);
        }
        match tree.get(code_id).and_then(|n| n.parent) {
            Some(parent) => code_id = parent,
            None => return None,
        }
    }
}

/// Same as [`find_entity_ancestor`] but also accepts `code_id` itself.
fn find_entity_ancestor_or_self(
    code_id: usize,
    tree: &CodeTree,
    id_map: &HashMap<usize, EntityId>,
) -> Option<EntityId> {
    if let Some(&eid) = id_map.get(&code_id) {
        return Some(eid);
    }
    find_entity_ancestor(code_id, tree, id_map)
}

/// Collect the entity-level children of `code_id`.
///
/// For children that are not entity nodes themselves (Block, Line), descend
/// recursively until entity-level descendants are found.
fn collect_entity_children(
    code_id: usize,
    tree: &CodeTree,
    id_map: &HashMap<usize, EntityId>,
) -> Vec<EntityId> {
    let Some(node) = tree.get(code_id) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for &child_code_id in &node.children {
        if let Some(&child_entity_id) = id_map.get(&child_code_id) {
            result.push(child_entity_id);
        } else {
            // Non-entity child (Block/Line): recurse into its descendants.
            result.extend(collect_entity_children(child_code_id, tree, id_map));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_file_tree() -> CodeTree {
        let mut tree = CodeTree::new();
        let root = tree.add_node(NodeKind::Folder, "root", (0, 200), (0, 40), 0, None);
        let fa = tree.add_node(NodeKind::File, "a.rs", (0, 100), (0, 20), 1, Some(root));
        let _fn_a = tree.add_node(NodeKind::Function, "fn_a", (0, 50), (0, 10), 2, Some(fa));
        let fb = tree.add_node(NodeKind::File, "b.rs", (101, 200), (21, 40), 1, Some(root));
        let _fn_b = tree.add_node(NodeKind::Function, "fn_b", (101, 150), (21, 30), 2, Some(fb));
        tree
    }

    #[test]
    fn test_entity_count_matches_entity_kinds() {
        let tree = two_file_tree();
        let graph = code_tree_to_entity_graph(&tree);
        // root(Folder) + a.rs(File) + fn_a(Function) + b.rs(File) + fn_b(Function) = 5
        assert_eq!(graph.entities.len(), 5);
    }

    #[test]
    fn test_block_and_line_nodes_excluded() {
        let mut tree = CodeTree::new();
        let root = tree.add_node(NodeKind::File, "f.rs", (0, 100), (0, 10), 0, None);
        let func = tree.add_node(NodeKind::Function, "fn_x", (0, 50), (0, 5), 1, Some(root));
        let _blk = tree.add_node(NodeKind::Block, "{}", (0, 30), (0, 3), 2, Some(func));
        let _ln = tree.add_node(NodeKind::Line, "let x = 1;", (0, 20), (0, 2), 3, Some(func));
        let graph = code_tree_to_entity_graph(&tree);
        // Only File + Function should appear
        assert_eq!(graph.entities.len(), 2);
        assert!(graph.entities.iter().all(|e| matches!(
            e.kind,
            EntityKind::File | EntityKind::Function
        )));
    }

    #[test]
    fn test_parent_child_wiring() {
        let tree = two_file_tree();
        let graph = code_tree_to_entity_graph(&tree);
        // Find the folder (root)
        let folder = graph.entities.iter().find(|e| e.kind == EntityKind::Folder).unwrap();
        assert_eq!(folder.children.len(), 2, "folder should have 2 file children");
        assert!(folder.parent.is_none());
    }

    #[test]
    fn test_references_converted() {
        let mut tree = two_file_tree();
        // fn_a (id=2) calls fn_b (id=4)
        tree.add_reference(2, 4, ReferenceKind::Call);
        let graph = code_tree_to_entity_graph(&tree);
        assert_eq!(graph.references.len(), 1);
        assert!(graph.references[0].from != graph.references[0].to);
    }

    /// Edges are deduplicated on the full (from, to, kind) triple: two kinds
    /// between the same pair both survive, while a repeated identical edge
    /// collapses to one whose sites list every distinct occurrence line.
    #[test]
    fn test_references_deduplicated_per_kind() {
        let mut tree = two_file_tree();
        tree.add_reference_at(2, 4, ReferenceKind::Call, 7);
        tree.add_reference_at(2, 4, ReferenceKind::Import, 1);
        tree.add_reference_at(2, 4, ReferenceKind::Call, 3);
        tree.add_reference_at(2, 4, ReferenceKind::Call, 7);
        let graph = code_tree_to_entity_graph(&tree);
        let kinds: Vec<_> = graph.references.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, vec![GraphReferenceKind::Call, GraphReferenceKind::Import]);
        assert_eq!(graph.references[0].sites, vec![Site { line: 3 }, Site { line: 7 }]);
        assert_eq!(graph.references[1].sites, vec![Site { line: 1 }]);
        assert!(graph.references.iter().all(|r| r.from != r.to));
    }

    #[test]
    fn test_empty_tree_gives_empty_graph() {
        let tree = CodeTree::new();
        let graph = code_tree_to_entity_graph(&tree);
        assert!(graph.entities.is_empty());
        assert!(graph.references.is_empty());
    }

    /// A `mod.rs` whose only children are Module declaration stubs (`pub mod foo;`)
    /// should be omitted from the entity graph, along with its stub Module children.
    /// References to the folder that owned the mod.rs should still be reachable.
    #[test]
    fn test_glue_mod_rs_excluded() {
        let mut tree = CodeTree::new();
        // src/ (Folder)
        let src = tree.add_node(NodeKind::Folder, "src", (0, 500), (0, 50), 0, None);
        // src/mod.rs (File) — contains only `pub mod app;` and `pub mod graph;`
        let modrs = tree.add_node(NodeKind::File, "mod.rs", (0, 50), (0, 5), 1, Some(src));
        // `pub mod app;` — Module stub with no body (no children)
        let _stub_app = tree.add_node(NodeKind::Module, "app", (0, 20), (0, 1), 2, Some(modrs));
        // `pub mod graph;` — Module stub with no body (no children)
        let _stub_graph = tree.add_node(NodeKind::Module, "graph", (21, 50), (2, 5), 2, Some(modrs));
        // src/app/ (Folder) — real sub-module
        let app_folder = tree.add_node(NodeKind::Folder, "app", (100, 300), (10, 30), 1, Some(src));
        // src/app/state.rs (File)
        let _state_rs = tree.add_node(NodeKind::File, "state.rs", (100, 300), (10, 30), 2, Some(app_folder));

        let graph = code_tree_to_entity_graph(&tree);

        // mod.rs and its two stub Module children must NOT appear
        assert!(
            graph.entities.iter().all(|e| e.name != "mod.rs"),
            "glue mod.rs should not appear as an entity"
        );
        // The bare stub Module nodes (app, graph declared in mod.rs) must NOT appear
        // as separate entities — the real Folder "app" node is still present.
        let module_entities: Vec<_> = graph.entities.iter()
            .filter(|e| e.kind == EntityKind::Module)
            .collect();
        assert!(
            module_entities.is_empty(),
            "stub Module declarations from glue mod.rs should not appear"
        );
        // The Folder "src", Folder "app", and File "state.rs" should still appear
        let names: Vec<&str> = graph.entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"src"), "Folder 'src' must be in graph");
        assert!(names.contains(&"app"), "Folder 'app' must be in graph");
        assert!(names.contains(&"state.rs"), "File 'state.rs' must be in graph");
    }

    /// A `mod.rs` that contains a function is NOT glue and should appear normally.
    #[test]
    fn test_non_glue_mod_rs_included() {
        let mut tree = CodeTree::new();
        let src = tree.add_node(NodeKind::Folder, "src", (0, 500), (0, 50), 0, None);
        let modrs = tree.add_node(NodeKind::File, "mod.rs", (0, 200), (0, 20), 1, Some(src));
        // This mod.rs has a real function — not pure glue
        let _fn_main = tree.add_node(NodeKind::Function, "main", (0, 100), (0, 10), 2, Some(modrs));

        let graph = code_tree_to_entity_graph(&tree);

        let names: Vec<&str> = graph.entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"mod.rs"), "non-glue mod.rs must appear as entity");
        assert!(names.contains(&"main"), "function inside non-glue mod.rs must appear");
    }

    /// References originating from a glue `mod.rs` (e.g. `pub use mod_impl::render;`)
    /// must be dropped from the entity graph so they do not produce stale cursor
    /// leaf pointers that cause the parent folder to reappear after an expand.
    #[test]
    fn test_glue_mod_rs_references_dropped() {
        let mut tree = CodeTree::new();
        // src/ (Folder)
        let src = tree.add_node(NodeKind::Folder, "src", (0, 500), (0, 50), 0, None);
        // src/ui/ (Folder)
        let ui = tree.add_node(NodeKind::Folder, "ui", (0, 400), (0, 40), 1, Some(src));
        // src/ui/mod.rs — glue file
        let modrs = tree.add_node(NodeKind::File, "mod.rs", (0, 50), (0, 5), 2, Some(ui));
        let _stub = tree.add_node(NodeKind::Module, "mod_impl", (0, 20), (0, 1), 3, Some(modrs));
        // src/ui/mod_impl.rs — real file
        let mod_impl = tree.add_node(NodeKind::File, "mod_impl.rs", (50, 200), (5, 20), 2, Some(ui));
        let render = tree.add_node(NodeKind::Function, "render", (50, 100), (5, 10), 3, Some(mod_impl));

        // Reference from glue mod.rs (file-level) to render function:
        // simulates `pub use mod_impl::render;`
        tree.add_reference(modrs, render, ReferenceKind::Import);

        let graph = code_tree_to_entity_graph(&tree);

        // The reference from the glue mod.rs must NOT appear in the entity graph.
        assert!(
            graph.references.is_empty(),
            "references from glue mod.rs must be dropped (got {:?})", graph.references
        );
    }

    /// References whose raw `to` endpoint is a glue stub Module node must be
    /// dropped, preventing the parent folder from appearing after an expand.
    #[test]
    fn test_references_to_glue_stub_dropped() {
        let mut tree = CodeTree::new();
        let src = tree.add_node(NodeKind::Folder, "src", (0, 500), (0, 50), 0, None);
        let ui = tree.add_node(NodeKind::Folder, "ui", (0, 400), (0, 40), 1, Some(src));
        let modrs = tree.add_node(NodeKind::File, "mod.rs", (0, 50), (0, 5), 2, Some(ui));
        let stub_events = tree.add_node(NodeKind::Module, "events", (0, 20), (0, 1), 3, Some(modrs));
        // Another real file with a function that "calls" the events module stub.
        let main_rs = tree.add_node(NodeKind::File, "main.rs", (100, 300), (10, 30), 1, Some(src));
        let fn_main = tree.add_node(NodeKind::Function, "main", (100, 200), (10, 20), 2, Some(main_rs));

        // Reference from fn_main to the stub Module "events" — this simulates a
        // reference resolved to a glue stub, e.g. `events::something()` where
        // "events" maps to the stub Module node in build_name_to_id_map.
        tree.add_reference(fn_main, stub_events, ReferenceKind::Call);

        let graph = code_tree_to_entity_graph(&tree);

        assert!(
            graph.references.is_empty(),
            "references to glue stub modules must be dropped (got {:?})", graph.references
        );
    }
}
