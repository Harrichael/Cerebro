//! JSON shapes for the HTTP contract in `ui/CONTRACT.md`. Field names and
//! kind strings are the wire format; the fixture test in `handlers.rs` pins
//! them to `ui/fixture.json`.
//!
//! Diff-mode fields are `Option` + `skip_serializing_if`, so a plain load
//! serializes byte-identically to before diffing existed.

use coalesce::{Coalesced, CoalescedEdge};
use entity_graph::{Entity, EntityGraph, EntityKind, Reference, ReferenceKind};
use graph_diff::{LineOp, Status, Tag};
use serde::Serialize;

use entity_graph::search::SearchResult;

#[derive(Serialize)]
pub struct GraphDto {
    pub root: String,
    pub generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remap: Option<RemapDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffDto>,
    pub nodes: Vec<NodeDto>,
    pub references: Vec<ReferenceDto>,
}

/// Old id -> new id across one rebuild, indexed by the previous generation's
/// ids; `null` where an entity has no counterpart.
#[derive(Serialize)]
pub struct RemapDto {
    pub from: u64,
    pub ids: Vec<Option<usize>>,
}

#[derive(Serialize)]
pub struct DiffDto {
    pub base: String,
    pub base_commit: String,
}

#[derive(Serialize)]
pub struct NodeDto {
    pub id: usize,
    pub kind: &'static str,
    pub name: String,
    pub path: String,
    pub line_start: usize,
    pub line_end: usize,
    pub parent: Option<usize>,
    pub loc: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub is_test: bool,
    #[serde(skip_serializing_if = "is_zero")]
    pub test_loc: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<usize>,
}

#[derive(Serialize)]
pub struct ReferenceDto {
    pub from: usize,
    pub to: usize,
    pub kind: &'static str,
    pub sites: Vec<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
}

/// A coalesced edge stands for many raw references; `refs` are their indices
/// into `graph.references`, so the UI can join back for sites and status.
#[derive(Serialize)]
pub struct EdgeDto {
    pub from: usize,
    pub to: usize,
    pub kind: &'static str,
    pub sites: Vec<usize>,
    pub refs: Vec<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
}

#[derive(Serialize)]
pub struct CoalescedDto {
    pub generation: u64,
    pub leaves: Vec<usize>,
    pub edges: Vec<EdgeDto>,
}

/// `[tag, old_start, old_len, new_start, new_len]`, tag one of `= - +`.
pub type OpDto = (&'static str, usize, usize, usize, usize);

#[derive(Serialize)]
pub struct SourceDto {
    pub generation: u64,
    pub id: usize,
    pub path: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ops: Option<Vec<OpDto>>,
}

#[derive(Serialize)]
pub struct ErrorDto {
    pub error: String,
}

#[derive(Serialize)]
pub struct SearchDto {
    pub generation: u64,
    pub query: String,
    pub hits: Vec<HitDto>,
    pub more: MoreDto,
}

#[derive(Serialize)]
pub struct HitDto {
    pub kind: &'static str,
    pub id: usize,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    pub text: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Serialize)]
pub struct MoreDto {
    pub file: usize,
    pub path: usize,
    pub content: usize,
    pub symbol: usize,
}

/// `error` is always present (`null` when the last rebuild succeeded) so the
/// UI can bind to it without an existence check.
#[derive(Serialize)]
pub struct StatusDto {
    pub generation: u64,
    pub mode: &'static str,
    pub dirty: bool,
    pub rebuilding: bool,
    pub error: Option<String>,
}

/// The wire slices JS strings, which count UTF-16 code units; the engine
/// counts chars. `text` is the string the offsets are measured against.
fn utf16_span(text: &str, start_chars: usize, end_chars: usize) -> (usize, usize) {
    let upto = |n: usize| text.chars().take(n).map(char::len_utf16).sum();
    (upto(start_chars), upto(end_chars))
}

pub fn search_dto(r: &SearchResult, generation: u64) -> SearchDto {
    SearchDto {
        generation,
        query: r.query.clone(),
        hits: r
            .hits
            .iter()
            .map(|h| {
                let (start, end) = utf16_span(&h.text, h.start, h.end);
                HitDto {
                    kind: h.kind.as_str(),
                    id: h.file.0,
                    path: h.path.clone(),
                    line: h.line,
                    text: h.text.clone(),
                    start,
                    end,
                }
            })
            .collect(),
        more: MoreDto {
            file: r.more.file,
            path: r.more.path,
            content: r.more.content,
            symbol: r.more.symbol,
        },
    }
}

/// The diff side tables a payload needs, borrowed from wherever they live.
pub struct DiffView<'a> {
    pub base: &'a str,
    pub base_commit: &'a str,
    pub entity_status: &'a [Status],
    pub reference_status: &'a [Status],
    pub churn: &'a [(usize, usize)],
}

// `EntityKind`'s Display is for humans ("class/struct"); the wire format is
// the contract's identifiers.
fn entity_kind(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Folder => "folder",
        EntityKind::Module => "module",
        EntityKind::File => "file",
        EntityKind::Class => "class",
        EntityKind::Function => "function",
    }
}

fn reference_kind(kind: ReferenceKind) -> &'static str {
    match kind {
        ReferenceKind::Call => "call",
        ReferenceKind::Import => "import",
        ReferenceKind::TypeRef => "type_ref",
        ReferenceKind::VarRef => "var_ref",
        ReferenceKind::Generic => "generic",
    }
}

fn status(status: Status) -> &'static str {
    match status {
        Status::Same => "same",
        Status::Added => "added",
        Status::Removed => "removed",
        Status::Modified => "modified",
    }
}

pub fn op_dto(op: &LineOp) -> OpDto {
    let tag = match op.tag {
        Tag::Equal => "=",
        Tag::Delete => "-",
        Tag::Insert => "+",
    };
    (tag, op.old_start, op.old_len, op.new_start, op.new_len)
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

fn node_dto(e: &Entity, (loc, test_loc): (usize, usize)) -> NodeDto {
    NodeDto {
        id: e.id.0,
        kind: entity_kind(e.kind),
        name: e.name.clone(),
        path: e.path.to_string_lossy().replace('\\', "/"),
        line_start: e.line_range.start,
        line_end: e.line_range.end,
        parent: e.parent.map(|p| p.0),
        loc,
        is_test: e.is_test,
        test_loc,
        status: None,
        added: None,
        removed: None,
    }
}

impl From<&Reference> for ReferenceDto {
    fn from(r: &Reference) -> Self {
        ReferenceDto {
            from: r.from.0,
            to: r.to.0,
            kind: reference_kind(r.kind),
            sites: r.sites.iter().map(|s| s.line).collect(),
            status: None,
        }
    }
}

pub fn graph_dto(graph: &EntityGraph, generation: u64, remap: Option<RemapDto>) -> GraphDto {
    plain_graph_dto(graph, generation, remap, None)
}

fn plain_graph_dto(
    graph: &EntityGraph,
    generation: u64,
    remap: Option<RemapDto>,
    entity_status: Option<&[Status]>,
) -> GraphDto {
    let loc = graph.loc_per_entity(|id| entity_status.is_some_and(|s| s[id.0] == Status::Removed));
    GraphDto {
        root: root_name(graph).to_string(),
        generation,
        remap,
        diff: None,
        // The arena index is the id, so iterating in order yields the
        // dense, id-sorted `nodes` the contract promises.
        nodes: graph.entities.iter().zip(&loc).map(|(e, &l)| node_dto(e, l)).collect(),
        references: graph.references.iter().map(ReferenceDto::from).collect(),
    }
}

/// The union graph plus the change tags every node and reference carries in
/// diff mode.
pub fn graph_dto_with_diff(
    graph: &EntityGraph,
    view: &DiffView<'_>,
    generation: u64,
    remap: Option<RemapDto>,
) -> GraphDto {
    let mut dto = plain_graph_dto(graph, generation, remap, Some(view.entity_status));
    dto.diff =
        Some(DiffDto { base: view.base.to_string(), base_commit: view.base_commit.to_string() });
    for node in &mut dto.nodes {
        let (added, removed) = view.churn[node.id];
        node.status = Some(status(view.entity_status[node.id]));
        node.added = Some(added);
        node.removed = Some(removed);
    }
    for (reference, &st) in dto.references.iter_mut().zip(view.reference_status) {
        reference.status = Some(status(st));
    }
    dto
}

pub fn coalesced_dto(c: &Coalesced, reference_status: Option<&[Status]>, generation: u64) -> CoalescedDto {
    CoalescedDto {
        generation,
        leaves: c.leaves.iter().map(|id| id.0).collect(),
        edges: c.edges.iter().map(|e| edge_dto(e, reference_status)).collect(),
    }
}

fn edge_dto(e: &CoalescedEdge, reference_status: Option<&[Status]>) -> EdgeDto {
    EdgeDto {
        from: e.from.0,
        to: e.to.0,
        kind: reference_kind(e.kind),
        // The UI derives an edge's sites from `/graph.json` via `refs`.
        sites: Vec::new(),
        refs: e.refs.iter().map(|r| r.0).collect(),
        status: reference_status.map(|all| edge_status(e, all)),
    }
}

/// One tag for a bundle of references: the members' status when they agree,
/// `mixed` when they do not.
fn edge_status(e: &CoalescedEdge, reference_status: &[Status]) -> &'static str {
    let mut members = e.refs.iter().filter_map(|r| reference_status.get(r.0));
    let Some(&first) = members.next() else { return status(Status::Same) };
    if members.all(|&s| s == first) { status(first) } else { "mixed" }
}

pub fn root_name(graph: &EntityGraph) -> &str {
    graph
        .entities
        .iter()
        .find(|e| e.parent.is_none())
        .map(|e| e.name.as_str())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use entity_graph::test_support::graph_from_parents;
    use EntityKind::*;

    /// `test_loc` is the part of `loc` inside test entities: a file with an
    /// inline `mod tests` counts only the module's lines, a test file counts
    /// itself once however deep its own test children go, and a folder sums
    /// both without double counting.
    #[test]
    fn test_loc_counts_each_test_line_once() {
        let mut g = graph_from_parents(
            &[
                ("proj", Folder, None),
                ("lib.rs", File, Some(0)),
                ("work", Function, Some(1)),
                ("tests", Module, Some(1)),
                ("check", Function, Some(3)),
                ("it.rs", File, Some(0)),
                ("helper", Function, Some(5)),
            ],
            &[],
        );
        let range = |g: &mut EntityGraph, id: usize, r: std::ops::Range<usize>| g.entities[id].line_range = r;
        range(&mut g, 1, 0..19);
        range(&mut g, 2, 0..4);
        range(&mut g, 3, 10..19);
        range(&mut g, 4, 12..18);
        range(&mut g, 5, 0..9);
        range(&mut g, 6, 1..8);
        for id in [3, 4, 5, 6] {
            g.entities[id].is_test = true;
        }

        let dto = graph_dto(&g, 1, None);
        let by_name = |n: &str| dto.nodes.iter().find(|x| x.name == n).unwrap();
        assert_eq!((by_name("lib.rs").loc, by_name("lib.rs").test_loc), (20, 10));
        assert_eq!((by_name("it.rs").loc, by_name("it.rs").test_loc), (10, 10));
        assert_eq!((by_name("proj").loc, by_name("proj").test_loc), (30, 20));
        assert!(by_name("check").is_test && !by_name("work").is_test);
        let json = serde_json::to_value(by_name("work")).unwrap();
        assert!(json.get("is_test").is_none() && json.get("test_loc").is_none(), "false/zero are omitted: {json}");
    }

    /// In diff mode the graph is the union of both sides, but a folder's
    /// `loc`/`test_loc` is what the working tree holds now: an added child
    /// counts, a removed one does not. A removed entity itself still reports
    /// its old size, so a deleted folder is not shown as empty.
    #[test]
    fn diff_folder_loc_is_the_new_side() {
        use Status::*;
        let mut g = graph_from_parents(
            &[
                ("proj", Folder, None),
                ("src", Folder, Some(0)),
                ("keep.rs", File, Some(1)),
                ("gone.rs", File, Some(1)),
                ("same.rs", File, Some(1)),
                ("new.rs", File, Some(1)),
                ("old", Folder, Some(0)),
                ("dead.rs", File, Some(6)),
            ],
            &[],
        );
        let ranges = [(2, 0..6), (3, 0..2), (4, 0..1), (5, 0..3), (7, 0..9)];
        for (id, r) in ranges {
            g.entities[id].line_range = r;
        }
        for id in [3, 5] {
            g.entities[id].is_test = true;
        }
        let entity_status = [Modified, Modified, Modified, Removed, Same, Added, Removed, Removed];
        let churn = vec![(0, 0); g.entities.len()];
        let view = DiffView {
            base: "HEAD",
            base_commit: "0123abcd",
            entity_status: &entity_status,
            reference_status: &[],
            churn: &churn,
        };

        let dto = graph_dto_with_diff(&g, &view, 1, None);
        let by_name = |n: &str| dto.nodes.iter().find(|x| x.name == n).unwrap();
        let counts = |n: &str| (by_name(n).loc, by_name(n).test_loc);
        assert_eq!(counts("src"), (7 + 2 + 4, 4), "keep + same + new, not gone");
        assert_eq!(counts("proj"), (13, 4), "the deleted folder adds nothing");
        assert_eq!(counts("gone.rs"), (3, 3), "a removed file keeps its old size");
        assert_eq!(counts("old"), (10, 0), "so does a removed folder");
    }

    /// The engine counts chars; this wire counts UTF-16 code units, because
    /// the page slices JS strings with them. An astral char is where the two
    /// part company: one char, two units.
    #[test]
    fn a_hit_reaches_the_page_in_the_units_it_slices_with() {
        assert_eq!(utf16_span("😀 Foo", 2, 5), (3, 6));
        assert_eq!(utf16_span("plain", 0, 2), (0, 2));
    }

}
