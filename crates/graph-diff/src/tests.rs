use std::path::Path;

use entity_graph::{EntityGraph, EntityId};
use tempfile::TempDir;

use crate::{GraphDiff, LineOp, Status, Tag};

fn write_tree(root: &Path, files: &[(&str, &str)]) {
    for (rel, body) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
}

/// Both sides live under a directory called `proj`: the producer names the
/// root after it and `diff` requires the two names to agree. `diff` reads
/// everything it needs up front, so the trees need not outlive the call.
fn diff_of(old: &[(&str, &str)], new: &[(&str, &str)]) -> GraphDiff {
    let tmp = TempDir::new().unwrap();
    let old_root = tmp.path().join("base/proj");
    let new_root = tmp.path().join("work/proj");
    write_tree(&old_root, old);
    write_tree(&new_root, new);
    let old_graph = treesitter_producer::graph_from_path(&old_root).unwrap();
    let new_graph = treesitter_producer::graph_from_path(&new_root).unwrap();
    crate::diff(&old_graph, &old_root, &new_graph, &new_root).unwrap()
}

fn id_of(graph: &EntityGraph, path: &str) -> EntityId {
    graph
        .entities
        .iter()
        .find(|e| e.path.to_string_lossy() == path)
        .unwrap_or_else(|| panic!("no entity at {path}"))
        .id
}

fn status_of(d: &GraphDiff, path: &str) -> Status {
    d.entity_status[id_of(&d.graph, path).0]
}

fn churn_of(d: &GraphDiff, path: &str) -> (usize, usize) {
    d.churn[id_of(&d.graph, path).0]
}

fn op(tag: Tag, old_start: usize, old_len: usize, new_start: usize, new_len: usize) -> LineOp {
    LineOp { tag, old_start, old_len, new_start, new_len }
}

const LIB_BASE: &str = "\
/// Doc.
fn alpha() {
    let x = 1;
}

fn beta() {
    alpha();
}
";

const UTIL: &str = "fn util() {\n    1\n}\n";

/// One line added inside `alpha`. Only `alpha` and the containers above it
/// change: `beta` shifted down a line but its text is untouched, and the
/// untouched sibling file stays `Same` even though its folder does not.
/// The file's ops must describe the insertion in place, not a wholesale
/// replace.
#[test]
fn an_edit_lands_on_its_function_and_its_containers() {
    let edited = "\
/// Doc.
fn alpha() {
    let x = 1;
    let y = 2;
}

fn beta() {
    alpha();
}
";
    let d = diff_of(
        &[("src/lib.rs", LIB_BASE), ("src/util.rs", UTIL)],
        &[("src/lib.rs", edited), ("src/util.rs", UTIL)],
    );

    assert_eq!(status_of(&d, "proj/src/lib.rs/alpha"), Status::Modified);
    assert_eq!(churn_of(&d, "proj/src/lib.rs/alpha"), (1, 0));
    assert_eq!(status_of(&d, "proj/src/lib.rs/beta"), Status::Same);
    assert_eq!(churn_of(&d, "proj/src/lib.rs/beta"), (0, 0));
    assert_eq!(status_of(&d, "proj/src/lib.rs"), Status::Modified);
    assert_eq!(status_of(&d, "proj/src"), Status::Modified);
    assert_eq!(status_of(&d, "proj"), Status::Modified);
    assert_eq!(churn_of(&d, "proj"), (1, 0));

    assert_eq!(status_of(&d, "proj/src/util.rs"), Status::Same);
    assert_eq!(status_of(&d, "proj/src/util.rs/util"), Status::Same);

    let lib = d.files.get(&id_of(&d.graph, "proj/src/lib.rs")).expect("edited file has ops");
    assert_eq!(lib.old_text.as_deref(), Some(LIB_BASE));
    assert_eq!(lib.new_text.as_deref(), Some(edited));
    assert_eq!(
        lib.ops,
        vec![op(Tag::Equal, 0, 3, 0, 3), op(Tag::Insert, 3, 0, 3, 1), op(Tag::Equal, 3, 5, 4, 5),]
    );
    assert!(!d.files.contains_key(&id_of(&d.graph, "proj/src/util.rs")));
}

/// A doc comment sits outside the function's own line range, so the range is
/// widened upward over it: editing the comment is editing the function.
#[test]
fn a_doc_comment_edit_modifies_its_function() {
    let d = diff_of(
        &[("src/lib.rs", LIB_BASE)],
        &[("src/lib.rs", &LIB_BASE.replace("/// Doc.", "/// Doc, revised."))],
    );

    assert_eq!(status_of(&d, "proj/src/lib.rs/alpha"), Status::Modified);
    assert_eq!(churn_of(&d, "proj/src/lib.rs/alpha"), (1, 1));
    assert_eq!(status_of(&d, "proj/src/lib.rs/beta"), Status::Same);
}

/// A file on one side only: its whole content is churn, its entities inherit
/// the file's fate, and a reference out of a removed file survives as
/// `Removed` pointing at the surviving definition. The removed subtree hangs
/// off the matched folder, so the union is still one well-formed tree.
#[test]
fn one_sided_files_and_references() {
    let d = diff_of(
        &[("src/lib.rs", LIB_BASE), ("src/gone.rs", "fn gone() {\n    alpha();\n}\n"), ("tests/old.rs", "fn t() {}\n")],
        &[("src/lib.rs", LIB_BASE), ("src/added.rs", "fn fresh() {\n    beta();\n}\n")],
    );

    // Producer-side facts survive the union for entities from either side.
    assert!(d.graph.entities[id_of(&d.graph, "proj/tests/old.rs/t").0].is_test);
    assert!(!d.graph.entities[id_of(&d.graph, "proj/src/added.rs/fresh").0].is_test);

    assert_eq!(status_of(&d, "proj/src/added.rs"), Status::Added);
    assert_eq!(churn_of(&d, "proj/src/added.rs"), (3, 0));
    assert_eq!(status_of(&d, "proj/src/added.rs/fresh"), Status::Added);

    assert_eq!(status_of(&d, "proj/src/gone.rs"), Status::Removed);
    assert_eq!(churn_of(&d, "proj/src/gone.rs"), (0, 3));
    assert_eq!(status_of(&d, "proj/src/gone.rs/gone"), Status::Removed);

    // lib.rs itself is byte-identical; only its neighbours moved.
    assert_eq!(status_of(&d, "proj/src/lib.rs"), Status::Same);
    assert_eq!(status_of(&d, "proj/src"), Status::Modified);

    let removed_file = id_of(&d.graph, "proj/src/gone.rs");
    assert_eq!(d.graph.entities[removed_file.0].parent, Some(id_of(&d.graph, "proj/src")));
    assert_well_formed(&d.graph);

    let refs: Vec<_> = d
        .graph
        .references
        .iter()
        .enumerate()
        .map(|(i, r)| {
            (
                d.graph.entities[r.from.0].name.as_str(),
                d.graph.entities[r.to.0].name.as_str(),
                d.reference_status[i],
            )
        })
        .collect();
    assert!(refs.contains(&("gone", "alpha", Status::Removed)), "{refs:?}");
    assert!(refs.contains(&("fresh", "beta", Status::Added)), "{refs:?}");
    assert!(refs.contains(&("beta", "alpha", Status::Same)), "{refs:?}");
}

/// The union is an ordinary graph, so every consumer keeps working on it.
#[test]
fn the_union_graph_drives_a_cursor() {
    let d = diff_of(
        &[("src/lib.rs", LIB_BASE), ("src/gone.rs", "fn gone() {\n    alpha();\n}\n")],
        &[("src/lib.rs", LIB_BASE), ("src/added.rs", "fn fresh() {\n    beta();\n}\n")],
    );

    let mut cursor = coalesce::Cursor::new(&d.graph);
    let root = d.graph.entities.iter().find(|e| e.parent.is_none()).unwrap().id;
    assert!(cursor.move_down(root, &d.graph));
    assert!(cursor.move_down(id_of(&d.graph, "proj/src"), &d.graph));
    let view = cursor.coalesced(&d.graph);
    assert!(view.leaves.contains(&id_of(&d.graph, "proj/src/gone.rs")));
    assert!(view.edges.iter().all(|e| !e.refs.is_empty()));
}

fn assert_well_formed(graph: &EntityGraph) {
    let roots = graph.entities.iter().filter(|e| e.parent.is_none()).count();
    assert_eq!(roots, 1, "the union must have exactly one root");
    for (i, e) in graph.entities.iter().enumerate() {
        assert_eq!(e.id.0, i, "ids must be dense and match arena order");
        for &c in &e.children {
            assert_eq!(graph.entities[c.0].parent, Some(e.id));
        }
        if let Some(p) = e.parent {
            assert!(graph.entities[p.0].children.contains(&e.id));
        }
    }
    for r in &graph.references {
        assert!(r.from.0 < graph.entities.len() && r.to.0 < graph.entities.len());
    }
}
