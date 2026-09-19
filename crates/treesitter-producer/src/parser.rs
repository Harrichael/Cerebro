use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rayon::prelude::*;
use tree_sitter::{Language, Node, Parser};

use crate::tree::{CodeTree, NodeKind, ReferenceKind};

/// Supported source languages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceLanguage {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Sql,
    PlainText,
}

impl SourceLanguage {
    /// Infer from a file extension.
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "rs" => SourceLanguage::Rust,
            "py" => SourceLanguage::Python,
            "js" | "mjs" | "cjs" => SourceLanguage::JavaScript,
            "ts" => SourceLanguage::TypeScript,
            "tsx" => SourceLanguage::Tsx,
            "sql" => SourceLanguage::Sql,
            _ => SourceLanguage::PlainText,
        }
    }

    /// Infer from a file path.
    pub fn from_path(path: &Path) -> Self {
        path.extension()
            .and_then(|e| e.to_str())
            .map(Self::from_extension)
            .unwrap_or(SourceLanguage::PlainText)
    }
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// Parse a single source file into a standalone `CodeTree`.
pub fn parse_source(source: &str, lang: &SourceLanguage, file_name: &str) -> Result<CodeTree> {
    let parsed = parse_file(source, lang)?;
    let mut tree = CodeTree::new();
    splice_file(&mut tree, file_name, &parsed, 0, None);
    Ok(tree)
}

/// Walk a directory and build a hierarchical `CodeTree` (Folder → File → constructs).
///
/// After building the contains topology the parser resolves the call and
/// import references extracted from each source file into the
/// [`ReferenceGraph`]. These edges represent the *symbolic* relationships
/// between constructs and are independent of the directory containment
/// structure.
///
/// Node ids are arena indices and downstream consumers treat them as the
/// identity of an entity across rebuilds, so the id assignment is part of
/// this producer's contract: a File node is immediately followed by its
/// constructs, then the next sibling, in directories-first alphabetical
/// order. Parsing runs on a thread pool, but only the pure per-file work
/// does; the tree itself is assembled sequentially in walk order.
pub fn parse_directory(dir: &Path) -> Result<CodeTree> {
    let mut tree = CodeTree::new();

    let dir_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(".")
        .to_string();
    let root_id = add_folder(&mut tree, dir_name, 0, None);

    // Phase 1: list the tree, parse every file in parallel, then splice the
    // parsed files into the tree in walk order.
    let listing = list_dir(dir)?;
    let mut paths = Vec::new();
    collect_file_paths(&listing, &mut paths);
    let parses = paths
        .par_iter()
        .map(|path| read_and_parse(path))
        .collect::<Result<Vec<ParsedFile>>>()?;
    let mut files = Vec::with_capacity(parses.len());
    splice_dir(&mut tree, &listing, root_id, 1, &mut parses.into_iter(), &mut files);

    // Phase 2: build a name → node_id map from the completed tree.
    let name_to_id = build_name_to_id_map(&tree);

    // Phase 3: resolve each file's raw references to node ids, in file order.
    for (file_id, parsed) in &files {
        for r in &parsed.refs {
            let from_id = construct_id(*file_id, r.from);
            if let Some(&to_id) = name_to_id.get(&r.name) {
                if from_id != to_id {
                    tree.add_reference_at(from_id, to_id, r.kind.clone(), r.line);
                }
            }
        }
    }

    Ok(tree)
}

// ─── Directory walk and splice ───────────────────────────────────────────────

/// One directory entry the walker decided to keep, in walk order.
enum Entry {
    Folder { name: String, children: Vec<Entry> },
    File { name: String, path: PathBuf },
}

/// Everything a single file contributes, expressed without tree ids so it
/// can be produced off-thread. Constructs are in `add_node` order with
/// parents as indices into the same list (`None` meaning the File itself),
/// which lets [`splice_file`] map them onto contiguous tree ids.
struct ParsedFile {
    byte_len: usize,
    last_line: usize,
    constructs: Vec<Construct>,
    refs: Vec<RawRef>,
}

struct Construct {
    kind: NodeKind,
    /// The grammar's own word for it; see [`noun_of`].
    noun: Option<&'static str>,
    name: String,
    byte_range: (usize, usize),
    line_range: (usize, usize),
    /// Nesting below the File: 0 for a top-level item.
    depth: usize,
    parent: Option<usize>,
}

/// A reference site whose target is still a bare name; resolution needs the
/// project-wide name table, which exists only once every file is in the tree.
struct RawRef {
    from: Option<usize>,
    name: String,
    kind: ReferenceKind,
    line: usize,
}

fn add_folder(tree: &mut CodeTree, name: impl Into<String>, depth: usize, parent: Option<usize>) -> usize {
    let id = tree.add_node(NodeKind::Folder, name, (0, 0), (0, 0), depth, parent);
    // Folders start at File-level granularity so only folders/files are shown
    // until the user explicitly drills down.
    if let Some(n) = tree.get_mut(id) {
        n.granularity_limit = Some(NodeKind::File);
    }
    id
}

/// List `dir` recursively: directories first, then files, both alphabetical,
/// skipping hidden entries and common build/dependency directories.
fn list_dir(dir: &Path) -> Result<Vec<Entry>> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("Cannot read directory: {}", dir.display()))?
        .filter_map(|e| e.ok())
        .collect();

    // `is_file()` follows symlinks, as do the `is_dir()`/`is_file()` checks
    // below; `DirEntry::file_type()` would not, and the two must agree.
    entries.sort_by_cached_key(|e| (e.path().is_file(), e.file_name()));

    let mut out = Vec::new();
    for entry in entries {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();

        if name.starts_with('.') || matches!(name.as_str(), "target" | "node_modules" | "__pycache__") {
            continue;
        }

        if path.is_dir() {
            let children = list_dir(&path)?;
            out.push(Entry::Folder { name, children });
        } else if path.is_file() {
            out.push(Entry::File { name, path });
        }
    }
    Ok(out)
}

fn collect_file_paths<'a>(entries: &'a [Entry], out: &mut Vec<&'a Path>) {
    for entry in entries {
        match entry {
            Entry::Folder { children, .. } => collect_file_paths(children, out),
            Entry::File { path, .. } => out.push(path),
        }
    }
}

fn read_and_parse(path: &Path) -> Result<ParsedFile> {
    let lang = SourceLanguage::from_path(path);
    // Unreadable or non-UTF-8 files still get a File node, just an empty one.
    let source = std::fs::read_to_string(path).unwrap_or_default();
    parse_file(&source, &lang)
}

/// Add Folder and File nodes for `entries` under `parent_id`, consuming one
/// parse per file from `parses` (which holds them in the same walk order).
fn splice_dir(
    tree: &mut CodeTree,
    entries: &[Entry],
    parent_id: usize,
    depth: usize,
    parses: &mut impl Iterator<Item = ParsedFile>,
    files: &mut Vec<(usize, ParsedFile)>,
) {
    for entry in entries {
        match entry {
            Entry::Folder { name, children } => {
                let folder_id = add_folder(tree, name, depth, Some(parent_id));
                splice_dir(tree, children, folder_id, depth + 1, parses, files);
            }
            Entry::File { name, .. } => {
                let parsed = parses.next().expect("one parse per listed file");
                let file_id = splice_file(tree, name, &parsed, depth, Some(parent_id));
                files.push((file_id, parsed));
            }
        }
    }
}

/// Add the File node and, directly after it, every construct of `parsed`.
fn splice_file(
    tree: &mut CodeTree,
    name: &str,
    parsed: &ParsedFile,
    depth: usize,
    parent: Option<usize>,
) -> usize {
    let file_id = tree.add_node(
        NodeKind::File,
        name,
        (0, parsed.byte_len),
        (0, parsed.last_line),
        depth,
        parent,
    );
    for (i, c) in parsed.constructs.iter().enumerate() {
        let id = tree.add_node(
            c.kind,
            &c.name,
            c.byte_range,
            c.line_range,
            depth + 1 + c.depth,
            Some(construct_id(file_id, c.parent)),
        );
        tree.nodes[id].noun = c.noun;
        debug_assert_eq!(id, construct_id(file_id, Some(i)));
    }
    file_id
}

/// Tree id of a file-local construct index; `None` is the File itself.
/// Valid because [`splice_file`] adds a file's constructs contiguously.
fn construct_id(file_id: usize, local: Option<usize>) -> usize {
    match local {
        Some(i) => file_id + 1 + i,
        None => file_id,
    }
}

// ─── Per-file parse ──────────────────────────────────────────────────────────

/// Parse one file's source: constructs and raw references from a single
/// tree-sitter pass, with no dependency on the surrounding tree.
fn parse_file(source: &str, lang: &SourceLanguage) -> Result<ParsedFile> {
    let mut parsed = ParsedFile {
        byte_len: source.len(),
        last_line: source.lines().count().saturating_sub(1),
        constructs: Vec::new(),
        refs: Vec::new(),
    };
    if matches!(lang, SourceLanguage::PlainText) {
        return Ok(parsed);
    }

    let mut parser = Parser::new();
    parser
        .set_language(&ts_language(lang))
        .context("Failed to set tree-sitter language")?;
    let ts_tree = parser
        .parse(source, None)
        .context("Tree-sitter failed to parse source")?;

    let source_bytes = source.as_bytes();
    let lines: Vec<&str> = source.lines().collect();
    walk_ts_node(ts_tree.root_node(), source_bytes, &lines, &mut parsed.constructs, None, 0);

    if !matches!(lang, SourceLanguage::Sql) {
        let scope_nodes: Vec<(usize, (usize, usize))> = parsed
            .constructs
            .iter()
            .enumerate()
            .filter(|(_, c)| matches!(c.kind, NodeKind::Function | NodeKind::Class))
            .map(|(i, c)| (i, c.byte_range))
            .collect();
        collect_ref_edges(ts_tree.root_node(), source_bytes, &scope_nodes, &mut parsed.refs);
    }
    Ok(parsed)
}

// ─── Reference extraction ─────────────────────────────────────────────────────

/// Build a name → node_id lookup from all Function, Class, Module, and File
/// nodes in the tree.  The first occurrence wins for duplicate names.
///
/// **Limitation**: symbols that share a name across files (e.g. two files each
/// defining `fn main`) can only be mapped to the first occurrence.  Fully
/// disambiguating same-named symbols would require tracking parent context or
/// module path, which is left for future work.
fn build_name_to_id_map(tree: &CodeTree) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    for node in tree.all_nodes_dfs() {
        if matches!(node.kind, NodeKind::Function | NodeKind::Class | NodeKind::Module | NodeKind::File) {
            map.entry(node.name.clone()).or_insert(node.id);
        }
    }
    map
}

/// Find the innermost scope (smallest byte range) among `scope_nodes` —
/// `(construct index, byte range)` pairs — that contains `byte_offset`;
/// `None` when no function/class scope does, i.e. the site is file-level.
///
/// This is O(n) in the number of scope nodes per call-site.  For typical
/// source files the number of functions is small enough that this is fast.
/// A future optimisation could sort `scope_nodes` by start position and use
/// binary search to find candidates before the linear containment filter.
fn find_containing_scope(byte_offset: usize, scope_nodes: &[(usize, (usize, usize))]) -> Option<usize> {
    scope_nodes
        .iter()
        .filter(|(_, (start, end))| *start <= byte_offset && byte_offset < *end)
        .min_by_key(|(_, (start, end))| end - start)
        .map(|(id, _)| *id)
}

/// Recursively walk a tree-sitter AST to collect call and import references.
///
/// A call is attributed to the innermost Function/Class construct textually
/// containing it; imports always belong to the file.
fn collect_ref_edges(
    node: Node<'_>,
    source: &[u8],
    scope_nodes: &[(usize, (usize, usize))],
    result: &mut Vec<RawRef>,
) {
    let kind = node.kind();

    // Call expressions (Rust, JS/TS) and plain calls (Python).
    if kind == "call_expression" || kind == "call" {
        let from = find_containing_scope(node.start_byte(), scope_nodes);
        if let Some(fn_node) = node.child_by_field_name("function") {
            if let Some(name) = extract_leaf_ident(fn_node, source) {
                if !is_trivial_name(&name) {
                    let line = fn_node.start_position().row;
                    result.push(RawRef { from, name, kind: ReferenceKind::Call, line });
                }
            }
        }
    }

    // Rust `use` declarations.
    if kind == "use_declaration" {
        for (name, line) in extract_use_leaf_names(node, source) {
            if !is_trivial_name(&name) {
                result.push(RawRef { from: None, name, kind: ReferenceKind::Import, line });
            }
        }
    }

    // Python / JS / TS import statements.
    if kind == "import_statement" || kind == "import_from_statement" {
        for (name, line) in extract_import_leaf_names(node, source) {
            if !is_trivial_name(&name) {
                result.push(RawRef { from: None, name, kind: ReferenceKind::Import, line });
            }
        }
    }

    for child in node.children(&mut node.walk()) {
        collect_ref_edges(child, source, scope_nodes, result);
    }
}

/// Extract the leaf identifier from a call-expression's function node.
///
/// Handles simple identifiers, field/member access (`obj.method`), and
/// qualified paths (`Module::function`).
fn extract_leaf_ident(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier"
        | "field_identifier"
        | "property_identifier"
        | "type_identifier"
        | "shorthand_property_identifier" => Some(extract_node_text(node, source)),
        _ => {
            // For qualified paths take the rightmost identifier-like child.
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            for child in children.iter().rev() {
                if let Some(name) = extract_leaf_ident(*child, source) {
                    return Some(name);
                }
            }
            None
        }
    }
}

/// Extract `(leaf name, row)` pairs from a Rust `use_declaration` node.
fn extract_use_leaf_names(node: Node<'_>, source: &[u8]) -> Vec<(String, usize)> {
    let mut names = Vec::new();
    collect_use_names(node, source, &mut names);
    names
}

fn collect_use_names(node: Node<'_>, source: &[u8], names: &mut Vec<(String, usize)>) {
    match node.kind() {
        "identifier" | "type_identifier" => {
            names.push((extract_node_text(node, source), node.start_position().row));
        }
        // `use foo as Bar` — extract the original name only.
        "use_as_clause" => {
            if let Some(first) = node.named_child(0) {
                collect_use_names(first, source, names);
            }
        }
        // `use_wildcard` (`use module::*`) — skip (no specific name to resolve).
        "use_wildcard" | "self" => {}
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_use_names(child, source, names);
            }
        }
    }
}

/// Extract `(leaf name, row)` pairs from Python/JS/TS import statements.
fn extract_import_leaf_names(node: Node<'_>, source: &[u8]) -> Vec<(String, usize)> {
    let mut names = Vec::new();
    collect_import_names(node, source, &mut names);
    names
}

fn collect_import_names(node: Node<'_>, source: &[u8], names: &mut Vec<(String, usize)>) {
    let row = node.start_position().row;
    match node.kind() {
        "identifier" => {
            names.push((extract_node_text(node, source), row));
        }
        // Python dotted names (e.g. `os.path`) — take the last segment.
        "dotted_name" | "relative_import" => {
            let text = extract_node_text(node, source);
            let leaf = text.split('.').last().unwrap_or(&text).trim().to_string();
            if !leaf.is_empty() {
                names.push((leaf, row));
            }
        }
        // JS `import_specifier`: `{ A as B }` → extract A.
        "import_specifier" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                names.push((extract_node_text(name_node, source), name_node.start_position().row));
            } else if let Some(first) = node.named_child(0) {
                names.push((extract_node_text(first, source), first.start_position().row));
            }
        }
        // Skip raw string module paths in JS/TS `from 'module'`.
        "string" | "string_fragment" => {}
        // `import module as alias` / `aliased_import` — use the original name.
        "as_pattern" | "aliased_import" => {
            if let Some(first) = node.named_child(0) {
                collect_import_names(first, source, names);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_import_names(child, source, names);
            }
        }
    }
}

/// Names that are too common/generic to be meaningful reference targets.
///
/// This list filters out standard-library/runtime functions, common helpers,
/// and language built-ins so that the reference graph is not cluttered with
/// noise.  Extend the list as new noisy patterns are encountered; all entries
/// should have near-universal presence across many files (e.g. `new`, `len`,
/// `println`) rather than project-specific names.
fn is_trivial_name(name: &str) -> bool {
    name.len() <= 1
        || matches!(
            name,
            "new"
                | "clone"
                | "unwrap"
                | "unwrap_or"
                | "unwrap_or_else"
                | "expect"
                | "to_string"
                | "to_owned"
                | "into"
                | "from"
                | "as_ref"
                | "as_mut"
                | "push"
                | "pop"
                | "len"
                | "is_empty"
                | "iter"
                | "iter_mut"
                | "get"
                | "set"
                | "insert"
                | "remove"
                | "contains"
                | "contains_key"
                | "println"
                | "print"
                | "eprintln"
                | "eprint"
                | "format"
                | "panic"
                | "Some"
                | "None"
                | "Ok"
                | "Err"
                | "true"
                | "false"
                | "self"
                | "Self"
                | "super"
                | "map"
                | "filter"
                | "collect"
                | "fold"
                | "for_each"
                | "find"
                | "any"
                | "all"
                | "flat_map"
                | "and_then"
                | "or_else"
                | "ok_or"
                | "next"
                | "parse"
                | "split"
                | "join"
                | "trim"
                | "default"
                | "ok"
                | "err"
                | "is_some"
                | "is_none"
                | "is_ok"
                | "is_err"
                | "log"
                | "error"
                | "warn"
                | "info"
                | "debug"
                | "trace"
                | "console"
                | "Object"
                | "Array"
                | "Math"
                | "JSON"
                | "input"
                | "range"
                | "enumerate"
                | "zip"
                | "list"
                | "dict"
                | "str"
                | "int"
                | "float"
                | "bool"
                | "assert"
                | "assert_eq"
                | "assert_ne"
                | "vec"
        )
}

#[allow(dead_code)]
fn add_plain_text_lines(tree: &mut CodeTree, source: &str, parent_id: usize, depth: usize) {
    let mut byte_offset = 0usize;
    for (i, line) in source.lines().enumerate() {
        let start = byte_offset;
        let end = start + line.len();
        tree.add_node(
            NodeKind::Line,
            line.trim_end(),
            (start, end),
            (i, i),
            depth,
            Some(parent_id),
        );
        byte_offset = end + 1;
    }
}

fn ts_language(lang: &SourceLanguage) -> Language {
    match lang {
        SourceLanguage::Rust => tree_sitter_rust::LANGUAGE.into(),
        SourceLanguage::Python => tree_sitter_python::LANGUAGE.into(),
        SourceLanguage::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        SourceLanguage::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        SourceLanguage::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        SourceLanguage::Sql => tree_sitter_sequel::LANGUAGE.into(),
        SourceLanguage::PlainText => unreachable!(),
    }
}

/// Recursively walk a tree-sitter node, lifting out interesting constructs
/// under `parent` (an index into `out`, or `None` for the file) in pre-order.
fn walk_ts_node(
    node: Node<'_>,
    source: &[u8],
    lines: &[&str],
    out: &mut Vec<Construct>,
    parent: Option<usize>,
    depth: usize,
) {
    for child in node.children(&mut node.walk()) {
        if let Some((kind, name_node)) = classify_ts_node(&child) {
            let name = name_node
                .map(|n| extract_node_text(n, source))
                .unwrap_or_else(|| first_line_preview(&child, source, lines));

            let index = out.len();
            out.push(Construct {
                kind,
                noun: noun_of(child.kind()),
                name,
                byte_range: (child.start_byte(), child.end_byte()),
                line_range: (child.start_position().row, child.end_position().row),
                depth,
                parent,
            });

            // Recurse into containers (only Module/Class/Function, not Block/Line)
            if matches!(kind, NodeKind::Module | NodeKind::Class | NodeKind::Function) {
                walk_ts_node(child, source, lines, out, Some(index), depth + 1);
            }
        } else {
            // Not interesting itself — pass the current parent through.
            walk_ts_node(child, source, lines, out, parent, depth);
        }
    }
}

/// Map a tree-sitter node type to our `NodeKind` and the name sub-node.
fn classify_ts_node<'a>(node: &Node<'a>) -> Option<(NodeKind, Option<Node<'a>>)> {
    match node.kind() {
        // ── Rust ──────────────────────────────────────────────────────────
        "function_item" | "function_signature_item" => {
            Some((NodeKind::Function, node.child_by_field_name("name")))
        }
        "impl_item" => Some((NodeKind::Class, node.child_by_field_name("type"))),
        "struct_item" => Some((NodeKind::Class, node.child_by_field_name("name"))),
        "enum_item" => Some((NodeKind::Class, node.child_by_field_name("name"))),
        "trait_item" => Some((NodeKind::Class, node.child_by_field_name("name"))),
        "mod_item" => Some((NodeKind::Module, node.child_by_field_name("name"))),
        // Basic constructs: if/for/while/loop/match
        // (removed - Block nodes are no longer produced)

        // ── Python ───────────────────────────────────────────────────────
        "function_definition" => Some((NodeKind::Function, node.child_by_field_name("name"))),
        "class_definition" => Some((NodeKind::Class, node.child_by_field_name("name"))),
        "decorated_definition" => Some((NodeKind::Function, None)),
        // Python-only basic constructs
        // (removed - Block nodes are no longer produced)

        // ── JavaScript ──────────────────────────────────────────────────
        "function_declaration" | "function" => {
            Some((NodeKind::Function, node.child_by_field_name("name")))
        }
        "method_definition" => Some((NodeKind::Function, node.child_by_field_name("name"))),
        "class_declaration" | "class" => {
            Some((NodeKind::Class, node.child_by_field_name("name")))
        }
        "arrow_function" => Some((NodeKind::Function, None)),
        // JS-only basic constructs
        // (removed - Block nodes are no longer produced)

        // ── TypeScript-only constructs ────────────────────────────────────
        // TypeScript files are also matched by the JS patterns above; the
        // entries below cover TS-specific node types not present in JS.
        // Interfaces, type aliases, and enums map to Class.
        "interface_declaration" => Some((NodeKind::Class, node.child_by_field_name("name"))),
        "type_alias_declaration" => Some((NodeKind::Class, node.child_by_field_name("name"))),
        "enum_declaration" => Some((NodeKind::Class, node.child_by_field_name("name"))),
        // Namespaces / internal modules map to Module.
        "internal_module" => Some((NodeKind::Module, node.child_by_field_name("name"))),
        // Abstract / interface method signatures map to Function.
        "abstract_method_signature" | "method_signature" => {
            Some((NodeKind::Function, node.child_by_field_name("name")))
        }

        // ── SQL (tree-sitter-sequel) ──────────────────────────────────────
        // Tables and views are class-level constructs; the name lives in the
        // first `object_reference` named child.
        "create_table" => {
            let name_node = find_named_child_by_kind(node, "object_reference");
            Some((NodeKind::Class, name_node))
        }
        "create_view" => {
            let name_node = find_named_child_by_kind(node, "object_reference");
            Some((NodeKind::Class, name_node))
        }
        // Individual SQL statements (SELECT, INSERT, …) are no longer shown as Blocks.

        // ── Shared Python + JS + TS basic constructs ─────────────────────
        // Note: Python uses `if_statement` / `for_statement` / `while_statement`,
        // JS/TS use the same names, so these are truly shared across three languages.
        // (removed - Block nodes are no longer produced)

        _ => None,
    }
}

/// What the language calls a construct [`classify_ts_node`] accepted. Every
/// type-like thing is a Class to the graph, but a reader shown "class" for a
/// trait or an interface is told something false.
fn noun_of(ts_kind: &str) -> Option<&'static str> {
    Some(match ts_kind {
        "function_item" | "function_signature_item" | "function_definition"
        | "function_declaration" | "function" | "arrow_function" | "decorated_definition" => "function",
        "method_definition" | "abstract_method_signature" | "method_signature" => "method",
        "impl_item" => "impl",
        "struct_item" => "struct",
        "enum_item" | "enum_declaration" => "enum",
        "trait_item" => "trait",
        "mod_item" => "module",
        "class_definition" | "class_declaration" | "class" => "class",
        "interface_declaration" => "interface",
        "type_alias_declaration" => "type alias",
        "internal_module" => "namespace",
        "create_table" => "table",
        "create_view" => "view",
        _ => return None,
    })
}

/// Return the first named child whose `kind()` equals `kind_str`, if any.
fn find_named_child_by_kind<'a>(node: &Node<'a>, kind_str: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind_str)
}

fn extract_node_text(node: Node<'_>, source: &[u8]) -> String {
    source[node.start_byte()..node.end_byte()]
        .iter()
        .map(|&b| b as char)
        .collect()
}

/// Maximum number of bytes used as a name preview when no better name is found.
const MAX_PREVIEW_LENGTH: usize = 40;

fn first_line_preview(node: &Node<'_>, source: &[u8], lines: &[&str]) -> String {
    let row = node.start_position().row;
    lines
        .get(row)
        .map(|l| l.trim().to_string())
        .unwrap_or_else(|| {
            let s = node.start_byte();
            let e = (s + MAX_PREVIEW_LENGTH).min(node.end_byte()).min(source.len());
            String::from_utf8_lossy(&source[s..e]).trim().to_string()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_SRC: &str = r#"
mod geometry {
    pub struct Point {
        x: f64,
        y: f64,
    }

    impl Point {
        pub fn new(x: f64, y: f64) -> Self {
            Point { x, y }
        }

        pub fn distance(&self, other: &Point) -> f64 {
            ((self.x - other.x).powi(2) + (self.y - other.y).powi(2)).sqrt()
        }
    }
}

fn main() {
    let p = geometry::Point::new(0.0, 0.0);
    println!("{:?}", p);
}
"#;

    #[test]
    fn test_rust_parse_finds_functions() {
        let tree = parse_source(RUST_SRC, &SourceLanguage::Rust, "test.rs").unwrap();
        let names: Vec<_> = tree
            .all_nodes_dfs()
            .iter()
            .filter(|n| n.kind == NodeKind::Function)
            .map(|n| n.name.clone())
            .collect();
        assert!(names.contains(&"new".to_string()), "Expected 'new': {names:?}");
        assert!(names.contains(&"main".to_string()), "Expected 'main': {names:?}");
    }

    #[test]
    fn test_rust_parse_finds_struct() {
        let tree = parse_source(RUST_SRC, &SourceLanguage::Rust, "test.rs").unwrap();
        let classes: Vec<_> = tree
            .all_nodes_dfs()
            .iter()
            .filter(|n| n.kind == NodeKind::Class)
            .map(|n| n.name.clone())
            .collect();
        assert!(
            classes.iter().any(|n| n.contains("Point")),
            "Expected Point struct: {classes:?}"
        );
    }

    #[test]
    fn test_rust_parse_finds_blocks() {
        let src = r#"
fn example() {
    if true {
        let x = 1;
    }
    for i in 0..10 {
        println!("{}", i);
    }
}
"#;
        let tree = parse_source(src, &SourceLanguage::Rust, "example.rs").unwrap();
        let has_block = tree
            .all_nodes_dfs()
            .iter()
            .any(|n| n.kind == NodeKind::Block);
        assert!(!has_block, "Block nodes should not be produced");
    }

    #[test]
    fn test_plain_text_gives_lines() {
        let src = "line one\nline two\nline three";
        let tree = parse_source(src, &SourceLanguage::PlainText, "notes.txt").unwrap();
        assert_eq!(tree.len(), 1); // just the file root, no children
        let vis = tree.visible_nodes();
        assert_eq!(vis.len(), 1);
    }

    #[test]
    fn test_language_from_extension() {
        assert_eq!(SourceLanguage::from_extension("rs"), SourceLanguage::Rust);
        assert_eq!(SourceLanguage::from_extension("py"), SourceLanguage::Python);
        assert_eq!(SourceLanguage::from_extension("txt"), SourceLanguage::PlainText);
        assert_eq!(SourceLanguage::from_extension("ts"), SourceLanguage::TypeScript);
        assert_eq!(SourceLanguage::from_extension("tsx"), SourceLanguage::Tsx);
        assert_eq!(SourceLanguage::from_extension("sql"), SourceLanguage::Sql);
    }

    const TYPESCRIPT_SRC: &str = r#"
interface Animal {
    name: string;
    speak(): void;
}

type Result<T> = { value: T } | null;

enum Direction {
    Up,
    Down,
}

class Dog implements Animal {
    name: string;
    constructor(name: string) { this.name = name; }
    speak(): void { console.log("Woof"); }
}

function greet(person: string): string {
    return "Hello " + person;
}

namespace Utils {
    export function helper(): void {}
}
"#;

    #[test]
    fn test_typescript_parse_finds_interface() {
        let tree = parse_source(TYPESCRIPT_SRC, &SourceLanguage::TypeScript, "test.ts").unwrap();
        let classes: Vec<_> = tree
            .all_nodes_dfs()
            .iter()
            .filter(|n| n.kind == NodeKind::Class)
            .map(|n| n.name.clone())
            .collect();
        assert!(
            classes.iter().any(|n| n == "Animal"),
            "Expected 'Animal' interface: {classes:?}"
        );
        // Every type-like thing is a Class to the graph, but each keeps the
        // word the language uses for it.
        let noun = |name: &str| tree.all_nodes_dfs().iter().find(|n| n.name == name).and_then(|n| n.noun);
        assert_eq!(noun("Animal"), Some("interface"));
        assert_eq!(noun("Dog"), Some("class"));
        assert_eq!(noun("Direction"), Some("enum"));
        assert_eq!(noun("greet"), Some("function"));
        assert_eq!(noun("speak"), Some("method"));
    }

    #[test]
    fn test_typescript_parse_finds_type_alias() {
        let tree = parse_source(TYPESCRIPT_SRC, &SourceLanguage::TypeScript, "test.ts").unwrap();
        let classes: Vec<_> = tree
            .all_nodes_dfs()
            .iter()
            .filter(|n| n.kind == NodeKind::Class)
            .map(|n| n.name.clone())
            .collect();
        assert!(
            classes.iter().any(|n| n == "Result"),
            "Expected 'Result' type alias: {classes:?}"
        );
    }

    #[test]
    fn test_typescript_parse_finds_enum() {
        let tree = parse_source(TYPESCRIPT_SRC, &SourceLanguage::TypeScript, "test.ts").unwrap();
        let classes: Vec<_> = tree
            .all_nodes_dfs()
            .iter()
            .filter(|n| n.kind == NodeKind::Class)
            .map(|n| n.name.clone())
            .collect();
        assert!(
            classes.iter().any(|n| n == "Direction"),
            "Expected 'Direction' enum: {classes:?}"
        );
    }

    #[test]
    fn test_typescript_parse_finds_class() {
        let tree = parse_source(TYPESCRIPT_SRC, &SourceLanguage::TypeScript, "test.ts").unwrap();
        let classes: Vec<_> = tree
            .all_nodes_dfs()
            .iter()
            .filter(|n| n.kind == NodeKind::Class)
            .map(|n| n.name.clone())
            .collect();
        assert!(
            classes.iter().any(|n| n == "Dog"),
            "Expected 'Dog' class: {classes:?}"
        );
    }

    #[test]
    fn test_typescript_parse_finds_function() {
        let tree = parse_source(TYPESCRIPT_SRC, &SourceLanguage::TypeScript, "test.ts").unwrap();
        let fns: Vec<_> = tree
            .all_nodes_dfs()
            .iter()
            .filter(|n| n.kind == NodeKind::Function)
            .map(|n| n.name.clone())
            .collect();
        assert!(
            fns.iter().any(|n| n == "greet"),
            "Expected 'greet' function: {fns:?}"
        );
    }

    #[test]
    fn test_typescript_parse_finds_namespace() {
        let tree = parse_source(TYPESCRIPT_SRC, &SourceLanguage::TypeScript, "test.ts").unwrap();
        let modules: Vec<_> = tree
            .all_nodes_dfs()
            .iter()
            .filter(|n| n.kind == NodeKind::Module)
            .map(|n| n.name.clone())
            .collect();
        assert!(
            modules.iter().any(|n| n == "Utils"),
            "Expected 'Utils' namespace: {modules:?}"
        );
    }

    const SQL_SRC: &str = r#"
CREATE TABLE users (
    id INT PRIMARY KEY,
    name VARCHAR(100)
);

CREATE VIEW active_users AS
SELECT * FROM users WHERE active = 1;

SELECT id, name FROM users;
"#;

    #[test]
    fn test_sql_parse_finds_table() {
        let tree = parse_source(SQL_SRC, &SourceLanguage::Sql, "schema.sql").unwrap();
        let classes: Vec<_> = tree
            .all_nodes_dfs()
            .iter()
            .filter(|n| n.kind == NodeKind::Class)
            .map(|n| n.name.clone())
            .collect();
        assert!(
            classes.iter().any(|n| n == "users"),
            "Expected 'users' table: {classes:?}"
        );
    }

    #[test]
    fn test_sql_parse_finds_view() {
        let tree = parse_source(SQL_SRC, &SourceLanguage::Sql, "schema.sql").unwrap();
        let classes: Vec<_> = tree
            .all_nodes_dfs()
            .iter()
            .filter(|n| n.kind == NodeKind::Class)
            .map(|n| n.name.clone())
            .collect();
        assert!(
            classes.iter().any(|n| n == "active_users"),
            "Expected 'active_users' view: {classes:?}"
        );
    }

    #[test]
    fn test_sql_parse_has_statements_as_blocks() {
        let tree = parse_source(SQL_SRC, &SourceLanguage::Sql, "schema.sql").unwrap();
        let has_block = tree
            .all_nodes_dfs()
            .iter()
            .any(|n| n.kind == NodeKind::Block);
        assert!(!has_block, "Block nodes should not be produced");
    }

    #[test]
    fn test_directory_parse_creates_folder_root() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        let tree = parse_directory(dir.path()).unwrap();
        let root = tree.get(tree.root.unwrap()).unwrap();
        assert_eq!(root.kind, NodeKind::Folder);
    }

    #[test]
    fn test_directory_parse_contains_file() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn hello() {}").unwrap();
        let tree = parse_directory(dir.path()).unwrap();
        let has_file = tree.all_nodes_dfs().iter().any(|n| n.kind == NodeKind::File);
        assert!(has_file);
    }

    #[test]
    fn test_directory_cross_file_references() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        // lib.rs defines `compute`; main.rs calls `compute` → should produce a
        // cross-file reference edge from the main.rs file node to the lib.rs
        // function node (or its file).
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn compute(x: i32) -> i32 { x * 2 }",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("main.rs"),
            "fn main() { let v = compute(1); }",
        )
        .unwrap();
        let tree = parse_directory(dir.path()).unwrap();
        // The reference graph should contain at least one edge involving `compute`.
        let refs = tree.references.references();
        assert!(
            !refs.is_empty(),
            "Expected at least one reference edge in the graph"
        );
        // Each file still keeps its own constructs.
        let all = tree.all_nodes_dfs();
        let fn_count = all.iter().filter(|n| n.kind == NodeKind::Function).count();
        assert!(fn_count >= 2, "Both files should have their own function nodes");
    }
}
