//! Tree-sitter producer for [`EntityGraph`].
//!
//! Parsing is name-based: a reference is resolved by looking the callee's or
//! import's leaf identifier up in a project-wide name table, so it is cheap
//! and needs no build, at the cost of precision (same-named symbols collapse
//! onto the first definition seen). That trade-off is the reason this
//! producer exists alongside compiler-backed ones.
//!
//! The producer contract this crate honors:
//!
//! - one root entity: a Folder named after the project directory, or a File
//!   when a single file was loaded;
//! - references deduplicated on `(from, to, kind)`, with self-loops dropped
//!   after Block/Line-level endpoints are lifted to their enclosing entity.
//!
//! Parsing lands in an intermediate [`tree::CodeTree`] before it becomes
//! entities, and that tree is nobody's business but this crate's:
//! `graph_from_path` is the only way in.

mod builder;
mod parser;
mod tree;

use std::path::Path;

use anyhow::{Context, Result};
use entity_graph::EntityGraph;

use parser::SourceLanguage;

pub fn graph_from_path(path: &Path) -> Result<EntityGraph> {
    // `.` or `..` would otherwise become the root entity's name; the root is
    // supposed to be named after the directory.
    let canonical = std::fs::canonicalize(path);
    let path = canonical.as_deref().unwrap_or(path);
    let tree = if path.is_file() {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("Cannot read file: {}", path.display()))?;
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
        parser::parse_source(&source, &SourceLanguage::from_path(path), file_name)?
    } else {
        parser::parse_directory(path)?
    };
    let mut graph = builder::code_tree_to_entity_graph(&tree);
    // A single-file load has the file as root, so its relative path is empty.
    let file_of = |rel: &Path| if path.is_file() { path.to_path_buf() } else { path.join(rel) };
    entity_graph::test_code::mark(&mut graph, |rel| std::fs::read_to_string(file_of(rel)).ok());
    Ok(graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use entity_graph::{EntityKind, ReferenceKind, Site};
    use tempfile::TempDir;

    /// The producer contract end to end: a directory load roots at a Folder
    /// named after the directory with one File per source file, File paths
    /// resolve back to relative filesystem paths, and a function called twice
    /// from one caller yields a single Function → Function edge carrying both
    /// occurrence lines. A single-file load roots at the File itself.
    #[test]
    fn graph_from_path_honors_root_and_reference_contract() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(project.join("sub")).unwrap();
        std::fs::write(project.join("sub/lib.rs"), "pub fn compute(x: i32) -> i32 { x * 2 }").unwrap();
        std::fs::write(
            project.join("main.rs"),
            "fn main() {\n    let v = compute(1);\n    let w = v;\n    let z = compute(w);\n}\n",
        )
        .unwrap();

        let graph = graph_from_path(&project).unwrap();
        let root = graph.entities.iter().find(|e| e.parent.is_none()).unwrap();
        assert_eq!((root.kind, root.name.as_str()), (EntityKind::Folder, "proj"));
        let by_name = |name: &str| graph.entities.iter().find(|e| e.name == name).unwrap().id;
        assert_eq!(graph.file_path(by_name("main.rs")), Some(PathBuf::from("main.rs")));
        assert_eq!(graph.file_path(by_name("lib.rs")), Some(PathBuf::from("sub/lib.rs")));
        assert_eq!(graph.file_path(by_name("compute")), Some(PathBuf::from("sub/lib.rs")));
        assert_eq!(graph.file_path(root.id), None);

        assert_eq!(graph.references.len(), 1);
        let r = &graph.references[0];
        assert_eq!((r.from, r.to, r.kind), (by_name("main"), by_name("compute"), ReferenceKind::Call));
        assert_eq!(r.sites, vec![Site { line: 1 }, Site { line: 3 }]);

        assert!(graph.entities.iter().all(|e| !e.is_test));

        let single = graph_from_path(&project.join("main.rs")).unwrap();
        let root = single.entities.iter().find(|e| e.parent.is_none()).unwrap();
        assert_eq!((root.kind, root.name.as_str()), (EntityKind::File, "main.rs"));
        assert_eq!(root.children.len(), 1);
        assert_eq!(single.file_path(root.id), Some(PathBuf::new()));
    }

    /// Test marking through the real parser: tree-sitter leaves attributes
    /// outside the item, so the classifier has to look above the start line;
    /// an integration-test folder marks everything beneath it.
    #[test]
    fn test_code_is_marked_from_attributes_and_paths() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(project.join("tests")).unwrap();
        std::fs::write(project.join("tests/it.rs"), "fn helper() {}\n").unwrap();
        std::fs::write(
            project.join("lib.rs"),
            "pub fn work() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn checks_work() { work(); }\n}\n",
        )
        .unwrap();

        let graph = graph_from_path(&project).unwrap();
        let mut flagged: Vec<&str> = graph.entities.iter().filter(|e| e.is_test).map(|e| e.name.as_str()).collect();
        flagged.sort();
        assert_eq!(flagged, ["checks_work", "helper", "it.rs", "tests", "tests"]);
        assert!(!graph.entities.iter().any(|e| e.name == "work" && e.is_test));

        let single = graph_from_path(&project.join("lib.rs")).unwrap();
        assert!(single.entities.iter().any(|e| e.name == "checks_work" && e.is_test));
    }

    /// A small project exercising every ordering rule the walker has: folders
    /// before files, siblings alphabetical, hidden and build directories
    /// skipped, several languages, nested constructs, a glue `mod.rs`, and
    /// cross-file references in both directions.
    fn ordering_fixture(dir: &Path) -> PathBuf {
        let project = dir.join("proj");
        for sub in [".hidden", "target", "db", "scripts", "src/shapes"] {
            std::fs::create_dir_all(project.join(sub)).unwrap();
        }
        let files: &[(&str, &str)] = &[
            (".hidden/secret.rs", "fn hidden() {}\n"),
            ("target/junk.rs", "fn junk() {}\n"),
            ("zeta.txt", "plain text\nno constructs\n"),
            ("alpha.rs", "use crate::util::compute;\n\nfn alpha() -> i32 {\n    compute(1) + run_all()\n}\n"),
            ("db/schema.sql", "CREATE TABLE users (id INT);\nCREATE VIEW active AS SELECT * FROM users;\n"),
            (
                "scripts/tool.py",
                "import os\nfrom util import compute\n\nclass Runner:\n    def run(self):\n        return compute(2)\n\ndef entry():\n    Runner().run()\n",
            ),
            (
                "scripts/web.ts",
                "import { greet } from './greet';\ninterface Animal { name: string; }\nclass Dog implements Animal {\n    name: string;\n    speak(): void { greet(this.name); }\n}\nfunction greet(who: string): string { return who; }\nnamespace Utils { export function helper(): void { greet('x'); } }\n",
            ),
            ("src/shapes/mod.rs", "pub mod circle;\npub mod square;\n"),
            ("src/shapes/circle.rs", "pub struct Circle;\nimpl Circle {\n    pub fn area(&self) -> f64 { compute(3) as f64 }\n}\n"),
            ("src/main.rs", "mod util;\nuse util::compute;\n\nfn main() {\n    let v = compute(1);\n    run_all();\n    let w = compute(v);\n}\n"),
            (
                "src/util.rs",
                "pub fn compute(x: i32) -> i32 { x * 2 }\n\npub struct Acc { total: i32 }\n\nimpl Acc {\n    pub fn add(&mut self, x: i32) { self.total += compute(x); }\n}\n\npub fn run_all() { alpha(); entry(); }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn checks() { compute(0); }\n}\n",
            ),
        ];
        for (rel, src) in files {
            std::fs::write(project.join(rel), src).unwrap();
        }
        project
    }

    fn render_graph(graph: &EntityGraph) -> String {
        let mut out = String::new();
        for e in &graph.entities {
            let parent = e.parent.map(|p| p.0.to_string()).unwrap_or_else(|| "-".into());
            out.push_str(&format!("{} {:?} {} <- {}\n", e.id.0, e.kind, e.name, parent));
        }
        for r in &graph.references {
            let lines: Vec<String> = r.sites.iter().map(|s| s.line.to_string()).collect();
            out.push_str(&format!("{} -> {} {:?} @{}\n", r.from.0, r.to.0, r.kind, lines.join(",")));
        }
        out
    }

    /// Entity ids are arena indices, so the exact interleaving of Folder,
    /// File and construct nodes IS the id assignment that reaches
    /// `/graph.json` and the server's rebuild id-map. Nothing else pins it:
    /// every other test looks entities up by name or path. This golden was
    /// captured from the original sequential walker (File node, then its
    /// constructs, then the next sibling) and any change to it is a wire
    /// format change, not a refactor.
    ///
    /// Note that it also pins two pre-existing quirks worth knowing about
    /// before "fixing" them: keyword tokens (`class`, `function`, `namespace`)
    /// are classified as constructs, and a struct plus its `impl` yield two
    /// same-named Class entities.
    #[test]
    fn entity_order_matches_golden() {
        const GOLDEN: &str = "\
0 Folder proj <- -
1 Folder db <- 0
2 File schema.sql <- 1
3 Class users <- 2
4 Class active <- 2
5 Folder scripts <- 0
6 File tool.py <- 5
7 Class Runner <- 6
8 Class class Runner: <- 7
9 Function run <- 7
10 Function entry <- 6
11 File web.ts <- 5
12 Class Animal <- 11
13 Class Dog <- 11
14 Class class Dog implements Animal { <- 13
15 Function speak <- 13
16 Function greet <- 11
17 Function function greet(who: string): string { return who; } <- 16
18 Module Utils <- 11
19 Function helper <- 18
20 Function namespace Utils { export function helper(): void { greet('x'); } } <- 19
21 Folder src <- 0
22 Folder shapes <- 21
23 File circle.rs <- 22
24 Class Circle <- 23
25 Class Circle <- 23
26 Function area <- 25
27 File main.rs <- 21
28 Module util <- 27
29 Function main <- 27
30 File util.rs <- 21
31 Function compute <- 30
32 Class Acc <- 30
33 Class Acc <- 30
34 Function add <- 33
35 Function run_all <- 30
36 Module tests <- 30
37 Function checks <- 36
38 File alpha.rs <- 0
39 Function alpha <- 38
40 File zeta.txt <- 0
6 -> 28 Import @1
6 -> 31 Import @1
9 -> 31 Call @5
10 -> 9 Call @8
10 -> 7 Call @8
11 -> 16 Import @0
15 -> 16 Call @4
19 -> 16 Call @7
26 -> 31 Call @2
27 -> 28 Import @1
27 -> 31 Import @1
29 -> 31 Call @4,6
29 -> 35 Call @5
34 -> 31 Call @5
35 -> 39 Call @8
35 -> 10 Call @8
37 -> 31 Call @13
38 -> 28 Import @0
38 -> 31 Import @0
39 -> 31 Call @3
39 -> 35 Call @3
";
        let dir = TempDir::new().unwrap();
        let project = ordering_fixture(dir.path());
        let graph = graph_from_path(&project).unwrap();
        assert_eq!(render_graph(&graph), GOLDEN);
    }

    /// Files are parsed on a thread pool; the result must not depend on
    /// scheduling. Ids and reference order both feed the server's rebuild
    /// migration, so the whole graph has to come out identical every time.
    #[test]
    fn repeated_parses_are_identical() {
        let dir = TempDir::new().unwrap();
        let project = ordering_fixture(dir.path());
        // `Entity` has no `PartialEq`; its Debug rendering covers every field.
        let snapshot = |g: &EntityGraph| (format!("{:?}", g.entities), g.references.clone());
        let first = snapshot(&graph_from_path(&project).unwrap());
        for _ in 0..5 {
            assert_eq!(snapshot(&graph_from_path(&project).unwrap()), first);
        }
    }
}
