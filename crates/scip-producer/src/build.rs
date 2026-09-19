use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use entity_graph::{Entity, EntityGraph, EntityId, EntityKind, Reference, ReferenceKind, Site};
use scip::types::symbol_information::Kind;
use scip::types::{Document, Index, Occurrence, SymbolRole};

use crate::dialect::{self, Dialect};
use crate::source::{ColumnUnit, Pos, SourceFile, Span};
use crate::symbols::ParsedSymbol;

pub fn build(index: &Index, project_root: &Path) -> EntityGraph {
    let mut docs: Vec<&Document> = index
        .documents
        .iter()
        .filter(|d| within_project(&d.relative_path))
        .collect();
    docs.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    docs.dedup_by(|a, b| a.relative_path == b.relative_path);

    let dialect = dialect::for_tool(&index.metadata.tool_info);
    let sources = docs
        .iter()
        .map(|doc| {
            let unit = ColumnUnit::resolve(
                doc.position_encoding.enum_value_or_default(),
                dialect.unspecified_column_unit(),
            );
            std::fs::read_to_string(project_root.join(&doc.relative_path))
                .ok()
                .map(|text| {
                    let import_lines = dialect.import_lines(&text);
                    SourceFile::new(text, unit, import_lines)
                })
        })
        .collect();

    let mut b = Builder {
        dialect,
        docs,
        sources,
        entities: Vec::new(),
        file_of_doc: Vec::new(),
        parsed: HashMap::new(),
        by_symbol: HashMap::new(),
        defs: Vec::new(),
        defs_by_doc: Vec::new(),
        claimed: HashSet::new(),
    };
    b.add_file_tree(root_name(project_root));
    b.add_definitions();
    b.resolve_parents();
    b.finish_paths_and_children();
    let references = b.references();
    let mut graph = EntityGraph {
        entities: b.entities,
        references,
    };
    entity_graph::test_code::mark(&mut graph, |rel| std::fs::read_to_string(project_root.join(rel)).ok());
    graph
}

/// An indexer can emit documents from outside the tree it was pointed at:
/// scip-go includes the generated test mains it left in the Go build cache,
/// whose relative path climbs out of the project with `..`. They are no part
/// of the project, and folding them in grows a `../../../..` folder spine
/// from the root.
fn within_project(relative_path: &str) -> bool {
    let mut components = Path::new(relative_path).components().peekable();
    components.peek().is_some()
        && components.all(|c| matches!(c, std::path::Component::Normal(_)))
}

fn root_name(project_root: &Path) -> String {
    let named = |p: &Path| p.file_name().map(|s| s.to_string_lossy().into_owned());
    named(project_root)
        .or_else(|| named(&std::fs::canonicalize(project_root).ok()?))
        .unwrap_or_else(|| "root".to_string())
}

struct Target {
    /// The entity that owns what this symbol declares; anything whose
    /// symbol-parent is this symbol is parented here.
    entity: EntityId,
    /// Where a use of this symbol points. Only a package differs: it is the
    /// directory, while the things it declares still belong to whichever
    /// file spells them.
    referent: EntityId,
    doc: usize,
    /// Whether the symbol *is* a file or directory rather than something
    /// declared inside one. Only the document that first declared it carries
    /// its `Kind`, so packagehood has to be remembered here.
    container: bool,
}

struct Def<'a> {
    entity: EntityId,
    doc: usize,
    span: Span,
    raw: &'a str,
    sym: Rc<ParsedSymbol>,
}

struct Builder<'a> {
    dialect: &'static dyn Dialect,
    docs: Vec<&'a Document>,
    sources: Vec<Option<SourceFile>>,
    entities: Vec<Entity>,
    file_of_doc: Vec<EntityId>,
    parsed: HashMap<&'a str, Option<Rc<ParsedSymbol>>>,
    by_symbol: HashMap<String, Target>,
    defs: Vec<Def<'a>>,
    defs_by_doc: Vec<Vec<usize>>,
    /// (doc, occurrence) pairs that are declarations rather than uses: the
    /// ones that became entities, plus the repeat declarations of a container
    /// symbol, which every file it spans restates.
    claimed: HashSet<(usize, usize)>,
}

impl<'a> Builder<'a> {
    fn push(&mut self, kind: EntityKind, name: String, parent: Option<EntityId>) -> EntityId {
        let id = EntityId(self.entities.len());
        self.entities.push(Entity {
            id,
            kind,
            noun: None,
            name,
            parent,
            children: Vec::new(),
            path: PathBuf::new(),
            byte_range: 0..0,
            line_range: 0..0,
            is_test: false,
        });
        id
    }

    fn add_file_tree(&mut self, root_name: String) {
        let root = self.push(EntityKind::Folder, root_name, None);
        let mut folders: HashMap<Vec<&str>, EntityId> = HashMap::new();
        folders.insert(Vec::new(), root);
        for d in 0..self.docs.len() {
            let relative_path = self.docs[d].relative_path.as_str();
            let segments: Vec<&str> = relative_path.split('/').filter(|s| !s.is_empty()).collect();
            let mut parent = root;
            for depth in 1..segments.len() {
                let key = segments[..depth].to_vec();
                parent = match folders.get(&key) {
                    Some(&id) => id,
                    None => {
                        let id = self.push(
                            EntityKind::Folder,
                            segments[depth - 1].to_string(),
                            Some(parent),
                        );
                        folders.insert(key, id);
                        id
                    }
                };
            }
            let name = segments
                .last()
                .copied()
                .unwrap_or(relative_path)
                .to_string();
            let file = self.push(EntityKind::File, name, Some(parent));
            if let Some(src) = &self.sources[d] {
                self.entities[file.0].byte_range = 0..src.len();
                self.entities[file.0].line_range = 0..src.line_count().saturating_sub(1);
            }
            self.file_of_doc.push(file);
        }
        self.defs_by_doc = vec![Vec::new(); self.docs.len()];
    }

    fn parsed(&mut self, raw: &'a str) -> Option<Rc<ParsedSymbol>> {
        self.parsed
            .entry(raw)
            .or_insert_with(|| ParsedSymbol::parse(raw).map(Rc::new))
            .clone()
    }

    fn add_definitions(&mut self) {
        for d in 0..self.docs.len() {
            let doc = self.docs[d];
            let kinds: HashMap<&str, Kind> = doc
                .symbols
                .iter()
                .map(|s| (s.symbol.as_str(), s.kind.enum_value_or_default()))
                .collect();
            let names: HashMap<&str, &str> = doc
                .symbols
                .iter()
                .map(|s| (s.symbol.as_str(), s.display_name.as_str()))
                .collect();
            for (o, occ) in doc.occurrences.iter().enumerate() {
                if occ.symbol_roles & SymbolRole::Definition as i32 == 0 {
                    continue;
                }
                let Some(sym) = self.parsed(&occ.symbol) else {
                    continue;
                };
                if let Some(&Target { container, .. }) = self.by_symbol.get(&sym.canonical) {
                    // A package is declared again by every file in it. Those
                    // repeats are still declarations, not uses; letting them
                    // fall through would give each file an edge to whichever
                    // sibling happened to be indexed first.
                    if container {
                        self.claimed.insert((d, o));
                    }
                    continue;
                }
                let scip_kind = kinds.get(occ.symbol.as_str()).copied().unwrap_or_default();
                let Some(kind) = self.dialect.entity_kind(&sym, scip_kind) else {
                    continue;
                };
                let Some(name_span) = Span::from_scip(&occ.range) else {
                    continue;
                };
                let enclosing = Span::from_scip(&occ.enclosing_range);
                self.claimed.insert((d, o));

                // The next two rules are conventions every indexer we have
                // met happens to share, not protocol, and so the seam where a
                // fourth one is likeliest to need a `Dialect` hook.
                //
                // A Module whose definition is the file itself, or a bare
                // package line, is the File entity; only an inline
                // `mod x { .. }` earns its own. An indexer that omitted
                // `enclosing_range` for an *inline* namespace would fold that
                // namespace into the File and register it as a container, so
                // every other file naming it would be claimed against this
                // one. That is the day to cut.
                //
                // `Kind::Package` is the other: that a package is its
                // directory is what Go, Java and Python all mean by the word,
                // not something SCIP says -- the kind is undocumented in the
                // proto. The descriptor cannot answer it at all, since SCIP's
                // `Package` suffix is a deprecated alias for `Namespace`
                // (both proto value 1) and so matches any file module.
                let is_whole_file = kind == EntityKind::Module
                    && enclosing.is_none_or(|e| e.start == Pos { line: 0, col: 0 });
                if is_whole_file {
                    let file = self.file_of_doc[d];
                    // A package spans every file of its directory, so it *is*
                    // the directory; picking one file would make an arbitrary
                    // sibling the target of every reference to the package.
                    let referent = if scip_kind == Kind::Package {
                        self.entities[file.0].parent.unwrap_or(file)
                    } else {
                        file
                    };
                    self.by_symbol.insert(
                        sym.canonical.clone(),
                        Target {
                            entity: file,
                            referent,
                            doc: d,
                            container: true,
                        },
                    );
                    continue;
                }

                let display = names.get(occ.symbol.as_str()).copied().unwrap_or("");
                let name = if display.is_empty() {
                    sym.last_name().to_string()
                } else {
                    display.to_string()
                };
                let span = enclosing.unwrap_or(name_span);
                let id = self.push(kind, name, None);
                self.entities[id.0].noun = sym.noun(scip_kind);
                if let Some(src) = &self.sources[d] {
                    self.entities[id.0].byte_range =
                        src.byte_offset(span.start)..src.byte_offset(span.end);
                }
                // Mirrors the tree-sitter producer: the end is the row holding
                // the last character, so a one-line entity has an empty range.
                self.entities[id.0].line_range = span.start.line..span.end.line;
                self.by_symbol.insert(
                    sym.canonical.clone(),
                    Target {
                        entity: id,
                        referent: id,
                        doc: d,
                        container: false,
                    },
                );
                self.defs_by_doc[d].push(self.defs.len());
                self.defs.push(Def {
                    entity: id,
                    doc: d,
                    span,
                    raw: &occ.symbol,
                    sym,
                });
            }
        }
    }

    fn same_doc_target(&self, canonical: &str, doc: usize) -> Option<EntityId> {
        let t = self.by_symbol.get(canonical)?;
        (t.doc == doc).then_some(t.entity)
    }

    /// Containment prefers the symbol's own structure over ranges, but a
    /// parent only counts when it is defined in this same document: a module
    /// declared elsewhere must not swallow this file's items.
    fn resolve_parents(&mut self) {
        let doc_symbols: Vec<HashMap<&str, &str>> = self
            .docs
            .iter()
            .map(|doc| {
                doc.symbols
                    .iter()
                    .map(|s| (s.symbol.as_str(), s.enclosing_symbol.as_str()))
                    .collect()
            })
            .collect();
        for i in 0..self.defs.len() {
            let Def {
                entity,
                doc,
                span,
                raw,
                ..
            } = self.defs[i];
            let sym = Rc::clone(&self.defs[i].sym);

            let structural = self
                .dialect
                .parent_symbol(&sym)
                .and_then(|p| self.same_doc_target(&p, doc))
                .or_else(|| {
                    let enclosing = doc_symbols[doc].get(raw).copied().unwrap_or("");
                    let enclosing = ParsedSymbol::parse(enclosing)?;
                    self.same_doc_target(&enclosing.canonical, doc)
                });
            let parent = structural
                .or_else(|| {
                    self.innermost_def(doc, |d| d.entity != entity && d.span.contains(&span))
                })
                .unwrap_or(self.file_of_doc[doc]);
            self.entities[entity.0].parent = Some(parent);
        }
    }

    fn innermost_def(&self, doc: usize, pred: impl Fn(&Def<'a>) -> bool) -> Option<EntityId> {
        self.defs_by_doc[doc]
            .iter()
            .map(|&i| &self.defs[i])
            .filter(|d| pred(d))
            .max_by_key(|d| (d.span.start, std::cmp::Reverse(d.span.end)))
            .map(|d| d.entity)
    }

    fn finish_paths_and_children(&mut self) {
        for i in 0..self.entities.len() {
            let mut parts = vec![self.entities[i].name.clone()];
            let mut cur = self.entities[i].parent;
            while let Some(p) = cur {
                parts.push(self.entities[p.0].name.clone());
                cur = self.entities[p.0].parent;
            }
            parts.reverse();
            self.entities[i].path = PathBuf::from(parts.join("/"));
            if let Some(p) = self.entities[i].parent {
                self.entities[p.0].children.push(EntityId(i));
            }
        }
    }

    fn references(&mut self) -> Vec<Reference> {
        let mut index: HashMap<(EntityId, EntityId, ReferenceKind), usize> = HashMap::new();
        let mut out: Vec<Reference> = Vec::new();
        for d in 0..self.docs.len() {
            for (o, occ) in self.docs[d].occurrences.iter().enumerate() {
                if self.claimed.contains(&(d, o)) {
                    continue;
                }
                let Some(sym) = self.parsed(&occ.symbol) else {
                    continue;
                };
                let Some(to) = self.by_symbol.get(&sym.canonical).map(|t| t.referent) else {
                    continue;
                };
                let Some(span) = Span::from_scip(&occ.range) else {
                    continue;
                };
                let from = self
                    .innermost_def(d, |def| def.span.contains_pos(span.start))
                    .unwrap_or(self.file_of_doc[d]);
                if from == to {
                    continue;
                }
                let kind = self.reference_kind(d, occ, span.start, self.entities[to.0].kind);
                let site = Site { line: span.start.line };
                match index.get(&(from, to, kind)) {
                    Some(&i) => out[i].sites.push(site),
                    None => {
                        index.insert((from, to, kind), out.len());
                        out.push(Reference { from, to, kind, sites: vec![site] });
                    }
                }
            }
        }
        for r in &mut out {
            r.sites.sort();
            r.sites.dedup();
        }
        out
    }

    fn reference_kind(
        &self,
        doc: usize,
        occ: &Occurrence,
        at: Pos,
        target: EntityKind,
    ) -> ReferenceKind {
        let imported = occ.symbol_roles & SymbolRole::Import as i32 != 0
            || self.sources[doc]
                .as_ref()
                .is_some_and(|s| s.is_import_line(at.line));
        if imported {
            return ReferenceKind::Import;
        }
        match target {
            EntityKind::Function => ReferenceKind::Call,
            EntityKind::Class => ReferenceKind::TypeRef,
            EntityKind::Module | EntityKind::File | EntityKind::Folder => ReferenceKind::Generic,
        }
    }
}
