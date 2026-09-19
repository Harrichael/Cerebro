//! Each fixture under `tests/fixtures/<lang>/` is a tiny project with its
//! committed `index.scip`, regenerated with
//! `cargo run -p scip-producer --example index -- tests/fixtures/<lang>`.
//! The tests pin the full entity set and the cross-file edges a consumer
//! would rely on, so any change in the mapping rules shows up here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use EntityKind::*;
use ReferenceKind::*;
use entity_graph::{EntityGraph, EntityId, EntityKind, ReferenceKind};

fn fixture(name: &str) -> (EntityGraph, PathBuf) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let graph = scip_producer::graph_from_index(&root.join("index.scip"), &root).unwrap();
    check_invariants(&graph, name);
    (graph, root)
}

fn check_invariants(graph: &EntityGraph, root_name: &str) {
    let roots: Vec<_> = graph
        .entities
        .iter()
        .filter(|e| e.parent.is_none())
        .collect();
    assert_eq!(roots.len(), 1);
    assert_eq!((roots[0].kind, roots[0].name.as_str()), (Folder, root_name));
    for (i, e) in graph.entities.iter().enumerate() {
        assert_eq!(e.id, EntityId(i));
        if let Some(p) = e.parent {
            assert!(
                graph.entities[p.0].children.contains(&e.id),
                "{} missing from parent's children",
                e.path.display()
            );
        }
    }
    let mut seen = HashSet::new();
    for r in &graph.references {
        assert_ne!(r.from, r.to);
        assert!(
            seen.insert((r.from, r.to, r.kind)),
            "duplicate reference {r:?}"
        );
        assert!(!r.sites.is_empty(), "reference without sites {r:?}");
        assert!(r.sites.windows(2).all(|w| w[0] < w[1]), "sites not sorted/deduped {r:?}");
    }
}

fn sites(graph: &EntityGraph, from: &str, to: &str, kind: ReferenceKind) -> Vec<usize> {
    let (from, to) = (id(graph, from), id(graph, to));
    graph
        .references
        .iter()
        .find(|r| r.from == from && r.to == to && r.kind == kind)
        .unwrap_or_else(|| panic!("missing edge {from:?} -> {to:?} [{kind}]"))
        .sites
        .iter()
        .map(|s| s.line)
        .collect()
}

fn entity_set(graph: &EntityGraph) -> HashSet<(String, EntityKind)> {
    graph
        .entities
        .iter()
        .map(|e| (e.path.display().to_string(), e.kind))
        .collect()
}

fn id(graph: &EntityGraph, path: &str) -> EntityId {
    graph
        .entities
        .iter()
        .find(|e| e.path == Path::new(path))
        .unwrap_or_else(|| panic!("no entity at {path}"))
        .id
}

fn assert_edge(graph: &EntityGraph, from: &str, to: &str, kind: ReferenceKind) {
    let (from, to) = (id(graph, from), id(graph, to));
    assert!(
        graph
            .references
            .iter()
            .any(|r| r.from == from && r.to == to && r.kind == kind),
        "missing edge {from:?} -> {to:?} [{kind}]"
    );
}

fn assert_parent(graph: &EntityGraph, child: &str, parent: &str) {
    assert_eq!(
        graph.entities[id(graph, child).0].parent,
        Some(id(graph, parent)),
        "parent of {child}"
    );
}

fn source_slice(root: &Path, graph: &EntityGraph, path: &str, file: &str) -> String {
    let e = &graph.entities[id(graph, path).0];
    let text = std::fs::read(root.join(file)).unwrap();
    String::from_utf8(text[e.byte_range.clone()].to_vec()).unwrap()
}

#[test]
fn rust_fixture_maps_methods_under_their_struct_and_links_across_files() {
    let (graph, root) = fixture("rust");

    let expected: HashSet<(String, EntityKind)> = [
        ("rust", Folder),
        ("rust/src", Folder),
        ("rust/src/geometry.rs", File),
        ("rust/src/main.rs", File),
        ("rust/src/util", Folder),
        ("rust/src/util/mod.rs", File),
        ("rust/src/geometry.rs/Point", Class),
        ("rust/src/geometry.rs/Point/new", Function),
        ("rust/src/geometry.rs/Point/magnitude", Function),
        ("rust/src/geometry.rs/Point/fmt", Function),
        ("rust/src/main.rs/main", Function),
        ("rust/src/main.rs/describe", Function),
        ("rust/src/util/mod.rs/greet", Function),
        ("rust/src/util/mod.rs/inner", Module),
        ("rust/src/util/mod.rs/inner/banner", Function),
    ]
    .into_iter()
    .map(|(p, k)| (p.to_string(), k))
    .collect();
    assert_eq!(entity_set(&graph), expected);

    // The indexer's own word for each entity comes along for showing.
    let noun = |p: &str| graph.entities[id(&graph, p).0].noun;
    assert_eq!(noun("rust/src/geometry.rs/Point"), Some("struct"));
    assert_eq!(noun("rust/src/geometry.rs/Point/new"), Some("static method"));
    assert_eq!(noun("rust/src/geometry.rs/Point/magnitude"), Some("method"));
    assert_eq!(noun("rust/src/main.rs/main"), Some("function"));

    // Inherent and trait impls both hang off the type, not the file.
    assert_parent(
        &graph,
        "rust/src/geometry.rs/Point/new",
        "rust/src/geometry.rs/Point",
    );
    assert_parent(
        &graph,
        "rust/src/geometry.rs/Point/fmt",
        "rust/src/geometry.rs/Point",
    );
    assert_parent(
        &graph,
        "rust/src/util/mod.rs/inner/banner",
        "rust/src/util/mod.rs/inner",
    );

    assert_edge(
        &graph,
        "rust/src/main.rs/main",
        "rust/src/geometry.rs/Point/new",
        Call,
    );
    assert_edge(
        &graph,
        "rust/src/main.rs/main",
        "rust/src/util/mod.rs/greet",
        Call,
    );
    assert_edge(
        &graph,
        "rust/src/main.rs/describe",
        "rust/src/geometry.rs/Point/magnitude",
        Call,
    );
    assert_edge(
        &graph,
        "rust/src/main.rs/describe",
        "rust/src/geometry.rs/Point",
        TypeRef,
    );
    assert_edge(
        &graph,
        "rust/src/main.rs",
        "rust/src/geometry.rs/Point",
        Import,
    );
    assert_edge(&graph, "rust/src/main.rs", "rust/src/geometry.rs", Import);
    assert_edge(
        &graph,
        "rust/src/util/mod.rs/greet",
        "rust/src/util/mod.rs/inner/banner",
        Call,
    );

    // Both `impl Point` headers land on the one file-level TypeRef edge.
    assert_eq!(
        sites(&graph, "rust/src/geometry.rs", "rust/src/geometry.rs/Point", TypeRef),
        vec![5, 15]
    );
    assert_eq!(
        sites(&graph, "rust/src/main.rs/main", "rust/src/geometry.rs/Point/new", Call),
        vec![6]
    );

    let point = &graph.entities[id(&graph, "rust/src/geometry.rs/Point").0];
    assert_eq!(point.line_range, 0..3);
    assert!(
        source_slice(
            &root,
            &graph,
            "rust/src/geometry.rs/Point",
            "src/geometry.rs"
        )
        .starts_with("pub struct Point {")
    );
    assert_eq!(
        source_slice(
            &root,
            &graph,
            "rust/src/util/mod.rs/inner/banner",
            "src/util/mod.rs"
        ),
        "pub fn banner() -> &'static str {\n        \"hello\"\n    }"
    );
}

#[test]
fn typescript_fixture_uses_utf16_columns_and_suffix_kinds() {
    let (graph, root) = fixture("ts");

    let expected: HashSet<(String, EntityKind)> = [
        ("ts", Folder),
        ("ts/src", Folder),
        ("ts/src/geometry.ts", File),
        ("ts/src/main.ts", File),
        ("ts/src/geometry.ts/Point", Class),
        ("ts/src/geometry.ts/Point/<constructor>", Function),
        ("ts/src/geometry.ts/Point/magnitude", Function),
        ("ts/src/geometry.ts/Point/describe", Function),
        ("ts/src/geometry.ts/origin", Function),
        ("ts/src/geometry.ts/shout", Function),
        ("ts/src/main.ts/report", Function),
        ("ts/src/main.ts/main", Function),
    ]
    .into_iter()
    .map(|(p, k)| (p.to_string(), k))
    .collect();
    assert_eq!(entity_set(&graph), expected);
    // scip-typescript classifies nothing, so the descriptor is all there is
    // to go on: it knows a method, and cannot tell a class from an interface.
    let noun = |p: &str| graph.entities[id(&graph, p).0].noun;
    assert_eq!(noun("ts/src/geometry.ts/Point"), None);
    assert_eq!(noun("ts/src/geometry.ts/Point/magnitude"), Some("method"));

    assert_parent(
        &graph,
        "ts/src/geometry.ts/Point/magnitude",
        "ts/src/geometry.ts/Point",
    );
    assert_edge(
        &graph,
        "ts/src/main.ts/report",
        "ts/src/geometry.ts/Point/describe",
        Call,
    );
    assert_edge(
        &graph,
        "ts/src/main.ts/report",
        "ts/src/geometry.ts/Point",
        TypeRef,
    );
    assert_edge(
        &graph,
        "ts/src/main.ts/main",
        "ts/src/geometry.ts/Point/<constructor>",
        Call,
    );
    assert_edge(
        &graph,
        "ts/src/main.ts/main",
        "ts/src/geometry.ts/origin",
        Call,
    );
    assert_edge(&graph, "ts/src/main.ts", "ts/src/geometry.ts/Point", Import);
    assert_edge(&graph, "ts/src/main.ts", "ts/src/geometry.ts", Import);

    // `shout` sits after an emoji on the same line: its UTF-16 column only
    // lands on the right byte if the encoding conversion happened.
    assert_eq!(
        source_slice(&root, &graph, "ts/src/geometry.ts/shout", "src/geometry.ts"),
        "export function shout(): string { return banner; }"
    );
}

#[test]
fn go_fixture_resolves_a_package_to_its_directory_not_to_one_of_its_files() {
    let (graph, _) = fixture("go");

    let expected: HashSet<(String, EntityKind)> = [
        ("go", Folder),
        ("go/geometry", Folder),
        ("go/geometry/point.go", File),
        ("go/geometry/scale.go", File),
        ("go/main.go", File),
        ("go/report.go", File),
        ("go/geometry/point.go/Point", Class),
        ("go/geometry/point.go/New", Function),
        ("go/geometry/point.go/Point/Magnitude", Function),
        ("go/geometry/scale.go/Scale", Function),
        ("go/main.go/describe", Function),
        ("go/main.go/main", Function),
        ("go/report.go/report", Function),
    ]
    .into_iter()
    .map(|(p, k)| (p.to_string(), k))
    .collect();
    assert_eq!(entity_set(&graph), expected);

    assert_parent(
        &graph,
        "go/geometry/point.go/Point/Magnitude",
        "go/geometry/point.go/Point",
    );
    // `Scale` is declared by scale.go even though `package geometry` names a
    // container that point.go declares too.
    assert_parent(&graph, "go/geometry/scale.go/Scale", "go/geometry/scale.go");

    // Importing a package, and qualifying a name with it, both mean the
    // directory. Naming a file instead would make whichever file sorts first
    // the target of every package-level reference in the project.
    assert_edge(&graph, "go/main.go", "go/geometry", Import);
    assert_edge(&graph, "go/main.go/main", "go/geometry", Generic);

    // A single-line `import "x"` is the form no other fixture line covers:
    // recognising only the `import ( .. )` block would leave this edge Generic.
    assert_eq!(sites(&graph, "go/report.go", "go/geometry", Import), vec![2]);
    assert!(
        !graph.references.iter().any(|r| r.from == id(&graph, "go/report.go")
            && r.to == id(&graph, "go/geometry")
            && r.kind == Generic),
        "the import line must not also read as a plain use"
    );

    assert_edge(&graph, "go/main.go/main", "go/geometry/point.go/New", Call);
    assert_edge(&graph, "go/main.go/main", "go/geometry/scale.go/Scale", Call);
    assert_edge(
        &graph,
        "go/main.go/describe",
        "go/geometry/point.go/Point/Magnitude",
        Call,
    );
    assert_edge(
        &graph,
        "go/main.go/describe",
        "go/geometry/point.go/Point",
        TypeRef,
    );

    // The `package geometry` line of every file after the first used to read
    // as a use of the package, giving each file an edge to its arbitrarily
    // chosen sibling.
    assert!(
        !graph.references.iter().any(|r| r.from == id(&graph, "go/geometry/scale.go")),
        "a bare `package` line is a declaration, not a reference"
    );
}

#[test]
fn missing_sources_keep_line_ranges_and_zero_byte_ranges() {
    let index = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust/index.scip");
    let graph = scip_producer::graph_from_index(&index, Path::new("/nonexistent/rust")).unwrap();
    let point = &graph.entities[id(&graph, "rust/src/geometry.rs/Point").0];
    assert_eq!(point.line_range, 0..3);
    assert_eq!(point.byte_range, 0..0);
    assert!(graph.references.iter().any(|r| r.kind == Call));
    // Without source text there is nothing to spot a `use` line with.
    assert!(graph.references.iter().all(|r| r.kind != Import));
}

/// An index from a tool with no adapter still yields the protocol graph: every
/// entity, Call and TypeRef edges, but no Import edges and no indexer-specific
/// parenting. Pins what the fallback is, so a dispatch regression or a renamed
/// tool shows up here instead of as a silently thinner graph.
#[test]
fn unknown_indexer_gets_the_protocol_only_graph() {
    use protobuf::Message;

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust");
    let mut index =
        scip::types::Index::parse_from_bytes(&std::fs::read(root.join("index.scip")).unwrap())
            .unwrap();
    index
        .metadata
        .mut_or_insert_default()
        .tool_info
        .mut_or_insert_default()
        .name = "scip-nobody".to_string();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.scip");
    std::fs::write(&path, index.write_to_bytes().unwrap()).unwrap();

    let graph = scip_producer::graph_from_index(&path, &root).unwrap();
    check_invariants(&graph, "rust");
    assert_eq!(graph.entities.len(), 15);
    // Without rust-analyzer's `impl#` reading a method hangs off its file.
    assert_parent(&graph, "rust/src/geometry.rs/new", "rust/src/geometry.rs");
    assert_edge(&graph, "rust/src/main.rs/main", "rust/src/geometry.rs/new", Call);
    // `use geometry::Point;` is still a reference, just not an Import.
    assert_edge(&graph, "rust/src/main.rs", "rust/src/geometry.rs/Point", TypeRef);
    assert!(graph.references.iter().all(|r| r.kind != Import));
}
