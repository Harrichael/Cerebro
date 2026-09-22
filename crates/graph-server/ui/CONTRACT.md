# graph-server HTTP contract

All JSON. Ids are `EntityId` arena indices from the loaded graph, and they are
stable only within one **generation**: every rebuild of the graph (see "Live
updates") renumbers them, so every payload carries `generation` and a client
must not mix ids across generations. Entities match across adjacent
generations on `(kind, path)`; when several share a key, the i-th such entity
in id order matches the i-th in the other. The old→new map ships with
`/graph.json` as `remap`.

**Diff mode** (`graph-server --diff <git-ref> PATH`) serves the *union* of the
tree at `<ref>` and the working tree as one ordinary graph, so every route
below keeps its shape. What it adds is change tags, and every one of them is
omitted outside diff mode: the presence of `diff` in `/graph.json` is how the
UI detects the mode, and an absent `status` always means "unchanged". A
rename is a removed entity plus an added one, never a modified one.

## GET /
The UI shell (`ui/index.html`), embedded in the binary. It loads its logic as
ES modules from `/ui/*.js` (`common.js`, `nodes.js`, `model.js`, `layout.js`,
`code.js`, `goto.js`, `panel.js`, `search.js`, `app.js`), also embedded in the
binary.

Its third-party modules (React, xyflow, dagre, htm, highlight.js and its
grammars) and the xyflow stylesheet are embedded too, under `/ui/vendor/`,
and the page's import map points only there: a page load makes no request
off localhost. `ui/fetch-vendor.sh` regenerates `ui/vendor/` from esm.sh
and is the only place a dependency version lives; the vendored files are
committed, and `handlers::VENDOR` lists exactly what is served. Grammars are
served extensionless (`/ui/vendor/hljs/rust`) to match the import-map prefix
`highlight.js/lib/languages/` that `code.js` resolves lazily per language.

## GET /graph.json  — the raw entity graph
```json
{
  "root": "myapp",
  "generation": 3,
  "remap": { "from": 2, "ids": [0, 1, null, 4] },
  "nodes": [
    { "id": 0, "kind": "folder", "name": "myapp", "path": "myapp",
      "line_start": 0, "line_end": 0, "parent": null, "loc": 41 },
    { "id": 1, "kind": "file", "name": "main.rs", "path": "myapp/main.rs",
      "line_start": 0, "line_end": 40, "parent": 0, "loc": 41 }
  ],
  "references": [
    { "from": 3, "to": 7, "kind": "call", "sites": [12, 30] }
  ]
}
```
- `kind` ∈ `folder | module | file | class | function`
- reference `kind` ∈ `call | import | type_ref | var_ref | generic`
- `line_start`/`line_end` are 0-indexed and inclusive, `0/0` when not
  applicable.
- `loc` is `line_end - line_start + 1` for any node with a range; a node
  without one (folders) carries the sum of its children's `loc`.
- `is_test` (omitted when false) marks test code by the conventions in
  `entity_graph::test_code`; every descendant of a test node is a test node.
  `test_loc` (omitted when 0) is the part of `loc` inside test nodes, so a
  view hiding tests shows `loc - test_loc`.
- `sites` are the 0-indexed lines, in the `from` entity's file, of every
  occurrence that produced the reference; sorted, no duplicates. Producers
  always record at least one; hand-built graphs may leave it empty.
  Besides listing them, the UI reads sites for **go-to from the code pane**
  (`goto.js`): a click on an identifier at line L of the open file takes the
  references sited at L whose `from` lies in that file (matched on the File
  entity's id, never on `path`, whose shape differs from `/source`'s), keeps
  those whose `to` is *named* by the clicked token (whole token, not
  substring), and selects the target's nearest drawn node — itself, or the
  leaf or box it is collapsed into — through the same selection as a click
  on the graph; the pane does not move. A target whose name is not on the
  line at all (an alias) is reached from any identifier there. Two targets
  that still resolve to different drawn nodes, or no target at all, select
  nothing. Under tree-sitter this is as approximate as the producer's
  name-based resolution; under SCIP it is precise.
  This paragraph is the spec for two implementations, not one: `goto.js`
  here, and `entity_graph::goto` for the terminal viewer, which reaches the
  same rule from `ctrl-w d`. They answer to the same examples; changing one
  means changing the other.
- Containment is `parent` on the node; there is no separate edge list.
- `nodes` is sorted by `id` and dense (`nodes[i].id == i`).
- `generation` starts at 1 and grows by one per successful rebuild. `remap`
  is present only when `generation > 1`: `ids[i]` is the id in this
  generation of the entity that was `i` in generation `from`, or `null` if
  it has no counterpart. Without a query, `from` is the previous generation.
  `?from=G` asks for the map from generation G: the server composes the
  steps it remembers (the last 16), so a client that slept through several
  rebuilds still gets a usable map. A `from` it cannot serve (older than
  that, or not behind the live generation) yields the default payload, whose
  `remap.from` then tells the client it has to reset; non-numeric is `400`.

In diff mode only, three additions:
```json
{
  "diff": { "base": "HEAD~1", "base_commit": "9f1c2ab..." },
  "nodes": [ { "id": 3, "status": "modified", "added": 12, "removed": 3, "...": "" } ],
  "references": [ { "from": 3, "to": 7, "kind": "call", "sites": [12], "status": "added" } ]
}
```
- `diff.base` is the ref as the user typed it, `diff.base_commit` the sha it
  resolved to. The working tree is always the new side.
- node `status` ∈ `same | added | removed | modified`. A File is `same` iff
  its bytes are; a Function/Class/Module is `modified` iff lines changed
  inside its range (widened upward over its doc comments, attributes and
  decorators), so a pure line shift stays `same`; a Folder is `same` iff all
  its children are.
- node `added`/`removed` are churn: lines inserted and deleted inside the
  node's range. A File counts its whole diff, a Folder sums its children.
- `loc`/`test_loc` stay the **current** size: a Folder sums only the
  children that still exist, so a `removed` child is not counted in any
  ancestor, while an `added` one is. A `removed` node itself keeps its old
  size (a deleted file or folder reads as what it was, never as 0).
- reference `status` ∈ `same | added | removed`, never `modified`. `sites`
  come from the side that has the reference (new when both do), so the sites
  of a `removed` reference are lines in the **old** file.
- A removed entity keeps its old `path` and line range and hangs under the
  parent it had; the union still has one root and dense ids.

## GET /source?id=N  — the source file containing entity N
```json
{ "generation": 1, "id": 1, "path": "src/main.rs", "text": "fn main() {\n..." }
```
`N` may be any entity; the server walks up to its File ancestor, whose id is
returned as `id`. `path` is relative to the loaded project root with `/`
separators; it is `""` when the loaded root is itself the file. The whole
file is returned, never truncated.

In diff mode, a file whose content changed (or that exists on one side only)
carries both sides and the line ops that interleave them, as computed for
this generation (a rebuild re-diffs the working tree against the same base):
```json
{ "generation": 1, "id": 1, "path": "src/main.rs", "text": "new side", "old_text": "old side",
  "ops": [["=", 0, 3, 0, 3], ["-", 3, 2, 3, 0], ["+", 5, 0, 3, 4]] }
```
- an op is `[tag, old_start, old_len, new_start, new_len]`, 0-indexed lines,
  tag `"="` equal, `"-"` deleted from old, `"+"` inserted in new. Ops cover
  both files in order; a delete and an insert each carry their position on the
  opposite side, which is what lets them be rendered as one listing.
- `text` is `""` for a removed file, `old_text` `""` for an added one. A
  removed file's old side is read from the extracted base tree.
- Any other file — unchanged, binary, or over 1 MiB on either side — returns
  the plain shape above with no `old_text` and no `ops`.

Errors, all `{"error": "..."}`: `400` bad or missing `id`; `404` unknown id,
an entity above every File (a folder: "entity N has no source file"), or an
unreadable file (message includes the io error — the index may be stale);
`403` if the file resolves outside the project root (in diff mode, outside
whichever root that side is read from); `413` if the file is over 2 MiB;
`415` if it is not valid UTF-8. `generation` is the graph the `id` was
resolved in; ids are dense, so a client must drop a response whose generation
is not its graph's rather than let a stale id name some other real entity.

## GET /coalesced.json  — the current coalesced view
```json
{ "generation": 1, "leaves": [0, 5, 9],
  "edges": [ { "from": 0, "to": 5, "kind": "call", "sites": [], "refs": [3, 7], "status": "mixed" } ] }
```
Leaves are entity ids; the UI joins them against `/graph.json` for labels.
Edges are deduplicated on `(from, to, kind)`; self-loops never appear.
Coalesced edges never carry sites (`sites` is always `[]`): an edge stands
for many raw references, listed in `refs` as indices into `/graph.json`'s
`references`, in graph order — that is where the UI reads their sites.
In diff mode an edge also carries `status` ∈ `same | added | removed |
mixed`, the status its members agree on, or `mixed` when they do not.
`generation` says which `/graph.json` the ids belong to; a payload whose
generation differs from the client's graph must not be joined against it.

## GET /search?q=...  — substring search over files
```json
{ "generation": 1, "query": "main file:lib",
  "hits": [
    { "kind": "file",    "id": 2, "path": "src/main.rs", "text": "main.rs",     "start": 0, "end": 4 },
    { "kind": "path",    "id": 9, "path": "src/domain/x.rs", "text": "src/domain/x.rs", "start": 4, "end": 8 },
    { "kind": "content", "id": 2, "path": "src/main.rs", "line": 0, "text": "fn main() {", "start": 3, "end": 7 }
  ],
  "more": { "file": 0, "path": 0, "content": 12 } }
```
Substring search, case-insensitive, over three kinds built per generation
from every File entity in the graph: `file` (the entity's name), `path` (its wire
path, `/`-separated), `content` (its lines, one document per line). Only
files the server can read text for are indexed — same texts `/source` would
serve, so a file that fails to read (unreadable, non-UTF-8, too big, or, in
diff mode, absent from both sides) is simply not searchable; a removed file
is indexed from its old text, an edited file from its new text.

- `q` is percent-encoded (`+` or `%20` for space). Missing `q` is `400`
  `` missing query parameter `q` ``; `q` that percent-decodes to invalid UTF-8
  is `400`; a query with no terms (`q=`) is `200` with no hits. Any other
  method is `405`. `query` echoes the trimmed query as typed.
- **Terms.** The query is split on whitespace into terms that must *all*
  hold. Double quotes join a phrase into one term and are removed
  (`"fn respond"`). A term may carry a tag, `file:`, `path:` or `content:`
  (case-insensitive, written outside any quotes); anything else with a colon
  (`foo:bar`, `C:\x`) is a plain term, and `"file:x"` in quotes is too. A tag
  with nothing after it is ignored.
- **How a term holds.** For a hit of kind K, a bare term or a term tagged K
  must occur in the hit's own text (the name, the path, or the line). A term
  tagged with another kind is a filter on the hit's file: `file:` on its
  name, `path:` on its path, `content:` on any one of its lines. A kind only
  produces hits when at least one term is about its own text, so `file:x`
  alone lists files, not every line inside them. Hence `class file:resolver`
  is "lines containing `class` in files whose name contains `resolver`", and
  `file:resolver content:class` additionally lists those files themselves.
  A hit's `start`/`end` mark the first own-text term.
- A hit's `kind` is which of the three kinds matched — not an entity kind,
  even though `"file"` happens to also be one. `path` is always the file's
  wire path, regardless of which kind matched; `text` is the thing the term
  was found in (name / path / line) and `start`/`end` the marked span within
  it. `line` (0-indexed, omitted on file/path hits) is present only on
  content hits.
- `start`/`end` are **UTF-16 code units** into `text`, not chars or bytes,
  because the UI slices JS strings with them.
- **Ordering.** File hits sort by (name length, path), path hits by (path
  length, path), content hits by (path, line); output is all file hits, then
  all path hits, then all content hits. A path hit whose mark falls inside
  the trailing filename segment, for a file that is also a file hit, is
  dropped — it is the same occurrence — but a mark in a directory segment
  (`main/` in `main/src/main.rs` for `main`) is kept as its own hit.
- **Limits.** Each kind is capped (`file` 30, `path` 30, `content` 100); the
  count past the cap for a kind is `more.<kind>`, `0` when nothing was cut.
- `generation` is the graph the hit ids belong to; drop a response whose
  generation is not the client's.

## POST /coalesced/zoom-in?id=N&generation=G
Replace leaf `N` with its children. `200` + the new coalesced payload on
change; `409` + `{"error": "..."}` if `N` is not a leaf or has no children.

## POST /coalesced/zoom-out?id=N&generation=G
Replace leaf `N` (and its sibling leaves) with its parent. `200` + payload on
change; `409` if `N` is not a leaf or is a root.

## POST /coalesced/reset?generation=G
Back to the root leaves. `200` + payload.

All three require `generation`, the generation the caller's ids come from.
Missing or non-numeric: `400`. Not the live generation: `409`
`{"error": "graph changed (generation 4, request was for 3)"}` — the message
always starts with `graph changed`, and the cursor is untouched — so a click
posted just before a swap never acts on a different entity. The generation
check runs before the id is looked at.

## Live updates
The server watches the project root (recursively; anything under a dotted
directory, `target` or `node_modules` is not a source) and can rebuild the
graph in place. A rebuild is a **generation change**: the whole graph is
re-produced with fresh ids, `generation` goes up by one, and `/graph.json`
carries `remap` from the previous generation (matching rule at the top).
The server migrates its own zoom through the same map: each leaf moves to its
match, or to its nearest matched ancestor when it is gone (a deleted file
coarsens to its folder; a renamed folder collapses everything inside it to
the rename's parent), and the ancestors of those targets are re-expanded. A
client translates its id-keyed state the same way — exact match for state
that names one thing (selection, open popover, open file), nearest matched
ancestor for state that can survive coarsening (bundling, hidden ids, scope)
— asking `/graph.json?from=<its generation>` so missed generations are
composed for it, and, when the `remap.from` it gets back is still not its
own generation, resets that state instead of guessing.

Two modes. **Static**: a source change only marks the graph `dirty`; the
client offers a manual reload. **Auto**: a source change rebuilds after 300 ms
of quiet. A failed rebuild (indexer error, unparsable tree) keeps the live
generation, sets `error`, and marks the graph dirty again; the next success
clears `error`. `--auto-update` starts in Auto; the default is Static.

### GET /status
```json
{ "generation": 3, "mode": "static", "dirty": false, "rebuilding": false, "error": null }
```
- `mode` ∈ `static | auto`.
- `dirty`: sources changed since the live generation was loaded and no
  rebuild has picked them up (set by the watcher in static mode, or by a
  failed rebuild in either mode).
- `rebuilding`: a rebuild has been requested or is running.
- `error`: the last rebuild failure, `null` once a rebuild succeeds.
A client polls this; when `generation` is not its graph's, it refetches
`/graph.json` and `/coalesced.json` (until the two agree) and migrates.

### POST /watch?mode=static|auto
Switch mode; returns the status payload. Switching to `auto` while `dirty`
requests a rebuild. Missing or other `mode`: `400`.

### POST /reload
Request a rebuild regardless of mode (Static's manual pull); returns the
status payload with `rebuilding: true`. The rebuild happens on the watcher
thread, so this returns at once; poll `/status` for the new generation.

Method mismatches on these routes are `405` like everywhere else.

Anything else: `404`.
