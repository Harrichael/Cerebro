//! Request handling as a pure function of (method, path) -> response, so the
//! whole contract is testable without opening a socket. `main.rs` is the only
//! thing that knows about `tiny_http`.
//!
//! `Server` is the long-lived shell around a sequence of `Snapshot`
//! generations. Requests only read the live snapshot and flip flags; the
//! watcher thread (`watch.rs`) is the only caller of `rebuild` in production,
//! so no request ever waits on a re-index.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use coalesce::Cursor;
use entity_graph::{EntityGraph, EntityId};
use graph_diff::FileDiff;

use crate::dto::{self, ErrorDto, SourceDto, StatusDto};
use crate::reload::{self, DiffState, Loaded, Loader, RemapHistory, Snapshot};
use entity_graph::search::{Limits, TextIndex};

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

const JSON: &str = "application/json";
const HTML: &str = "text/html; charset=utf-8";
const JS: &str = "application/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";

/// The ES modules `index.html` imports, served at `/ui/<name>` and embedded
/// in the binary the same way `index_html` is.
const UI_MODULES: &[(&str, &str)] = &[
    ("common.js", include_str!("../ui/common.js")),
    ("nodes.js", include_str!("../ui/nodes.js")),
    ("bundling.js", include_str!("../ui/bundling.js")),
    ("model.js", include_str!("../ui/model.js")),
    ("layout.js", include_str!("../ui/layout.js")),
    ("code.js", include_str!("../ui/code.js")),
    ("goto.js", include_str!("../ui/goto.js")),
    ("panel.js", include_str!("../ui/panel.js")),
    ("search.js", include_str!("../ui/search.js")),
    ("app.js", include_str!("../ui/app.js")),
];

/// Third-party modules the import map in `index.html` points at, fetched by
/// `ui/fetch-vendor.sh` and served at `/ui/<name>` like `UI_MODULES`. Every
/// module's own imports are rewritten to `./` siblings in this table, so a
/// page load never leaves localhost.
///
/// Grammars have no extension because `code.js` imports them through the
/// import-map prefix `highlight.js/lib/languages/` + bare language name.
const VENDOR: &[(&str, &str)] = &[
    ("vendor/react.js", include_str!("../ui/vendor/react.js")),
    ("vendor/react-jsx-runtime.js", include_str!("../ui/vendor/react-jsx-runtime.js")),
    ("vendor/react-dom.js", include_str!("../ui/vendor/react-dom.js")),
    ("vendor/react-dom-client.js", include_str!("../ui/vendor/react-dom-client.js")),
    ("vendor/scheduler.js", include_str!("../ui/vendor/scheduler.js")),
    ("vendor/xyflow-react.js", include_str!("../ui/vendor/xyflow-react.js")),
    ("vendor/xyflow-react.css", include_str!("../ui/vendor/xyflow-react.css")),
    ("vendor/dagre.js", include_str!("../ui/vendor/dagre.js")),
    ("vendor/graphlib.js", include_str!("../ui/vendor/graphlib.js")),
    ("vendor/graphlib-alg.js", include_str!("../ui/vendor/graphlib-alg.js")),
    ("vendor/graphlib-json.js", include_str!("../ui/vendor/graphlib-json.js")),
    ("vendor/htm.js", include_str!("../ui/vendor/htm.js")),
    ("vendor/hljs-core.js", include_str!("../ui/vendor/hljs-core.js")),
    ("vendor/hljs/rust", include_str!("../ui/vendor/hljs/rust")),
    ("vendor/hljs/go", include_str!("../ui/vendor/hljs/go")),
    ("vendor/hljs/typescript", include_str!("../ui/vendor/hljs/typescript")),
    ("vendor/hljs/javascript", include_str!("../ui/vendor/hljs/javascript")),
    ("vendor/hljs/python", include_str!("../ui/vendor/hljs/python")),
    ("vendor/hljs/ini", include_str!("../ui/vendor/hljs/ini")),
    ("vendor/hljs/json", include_str!("../ui/vendor/hljs/json")),
    ("vendor/hljs/markdown", include_str!("../ui/vendor/hljs/markdown")),
    ("vendor/hljs/xml", include_str!("../ui/vendor/hljs/xml")),
    ("vendor/hljs/css", include_str!("../ui/vendor/hljs/css")),
    ("vendor/hljs/bash", include_str!("../ui/vendor/hljs/bash")),
    ("vendor/hljs/yaml", include_str!("../ui/vendor/hljs/yaml")),
    ("vendor/hljs/sql", include_str!("../ui/vendor/hljs/sql")),
    ("vendor/hljs/c", include_str!("../ui/vendor/hljs/c")),
    ("vendor/hljs/cpp", include_str!("../ui/vendor/hljs/cpp")),
    ("vendor/hljs/java", include_str!("../ui/vendor/hljs/java")),
    ("vendor/hljs/ruby", include_str!("../ui/vendor/hljs/ruby")),
    ("vendor/hljs/kotlin", include_str!("../ui/vendor/hljs/kotlin")),
    ("vendor/hljs/swift", include_str!("../ui/vendor/hljs/swift")),
    ("vendor/hljs/lua", include_str!("../ui/vendor/hljs/lua")),
];

/// Largest source file `/source` will serve; the viewer renders the whole
/// file in one `<pre>`, so anything bigger is refused rather than truncated.
const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;

/// `/search` result caps, one per kind.
const SEARCH_LIMITS: Limits = Limits { file: 30, path: 30, content: 100, symbol: 30 };

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Static,
    Auto,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Static => "static",
            Mode::Auto => "auto",
        }
    }
}

pub struct Server {
    // The project directory (or single file) the graph is loaded from;
    // `EntityGraph::file_path` results are resolved against it.
    root: PathBuf,
    index_html: &'static str,
    loader: Loader,
    live: RwLock<Arc<Snapshot>>,
    remaps: Mutex<RemapHistory>,
    mode: AtomicU8,
    // Sources changed since the live generation was loaded: static mode's
    // signal to offer a manual reload.
    dirty: AtomicBool,
    // A rebuild has been asked for (`/reload`, or `/watch?mode=auto` while
    // dirty) and the watcher has not picked it up yet.
    pending: AtomicBool,
    rebuilding: AtomicBool,
    last_error: Mutex<Option<String>>,
    wake: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl Server {
    /// Runs the loader once; the result is generation 1.
    pub fn new(
        loader: Loader,
        root: PathBuf,
        index_html: &'static str,
        mode: Mode,
    ) -> anyhow::Result<Server> {
        let first = Snapshot::build(1, loader()?, &root, None);
        Ok(Server {
            root,
            index_html,
            loader,
            live: RwLock::new(Arc::new(first)),
            remaps: Mutex::new(RemapHistory::new()),
            mode: AtomicU8::new(mode as u8),
            dirty: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            rebuilding: AtomicBool::new(false),
            last_error: Mutex::new(None),
            wake: Mutex::new(None),
        })
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.live.read().unwrap().clone()
    }

    pub fn mode(&self) -> Mode {
        match self.mode.load(Ordering::Relaxed) {
            0 => Mode::Static,
            _ => Mode::Auto,
        }
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn request_rebuild(&self) {
        self.pending.store(true, Ordering::Relaxed);
        if let Some(wake) = self.wake.lock().unwrap().as_ref() {
            wake();
        }
    }

    /// Watcher only: consumes a pending request.
    pub fn take_request(&self) -> bool {
        self.pending.swap(false, Ordering::Relaxed)
    }

    /// Called whenever a rebuild is requested, so the watcher thread can wake
    /// up early instead of waiting for a file event.
    pub fn set_wake(&self, wake: impl Fn() + Send + Sync + 'static) {
        *self.wake.lock().unwrap() = Some(Box::new(wake));
    }

    /// Watcher thread only. Loads the next generation and swaps it in with
    /// the current expansion migrated across. On error the live snapshot is
    /// untouched and `/status` reports the failure until the next success.
    pub fn rebuild(&self) -> anyhow::Result<()> {
        self.rebuilding.store(true, Ordering::Relaxed);
        // Cleared before the load rather than after: an edit landing during
        // the load must leave the graph dirty again.
        self.dirty.store(false, Ordering::Relaxed);
        let result = self.rebuild_inner();
        self.rebuilding.store(false, Ordering::Relaxed);
        match &result {
            Ok(()) => *self.last_error.lock().unwrap() = None,
            Err(e) => {
                *self.last_error.lock().unwrap() = Some(format!("{e:#}"));
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
        result
    }

    fn rebuild_inner(&self) -> anyhow::Result<()> {
        // A clone, not a guard: std's RwLock cannot be upgraded, so holding
        // the read guard here would deadlock the write below.
        let old = self.snapshot();
        let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.loader)()))
            .map_err(|_| anyhow::anyhow!("the loader panicked"))??;
        let new_graph = match &loaded {
            Loaded::Graph(g) => g,
            Loaded::Diff { diff, .. } => &diff.graph,
        };
        let map = coalesce::migrate::id_map(&old.graph, new_graph);
        let next = Snapshot::build(old.generation + 1, loaded, &self.root, Some(&map));

        // Held from reading the old leaves through the swap: an expansion served
        // in between would be acknowledged and then silently thrown away.
        let old_cursor = old.cursor.lock().unwrap();
        *next.cursor.lock().unwrap() =
            coalesce::migrate::migrate_cursor(&old.graph, &old_cursor.leaves, &map, &next.graph);
        // Recorded before the swap, so no request ever sees the new
        // generation without the step that leads to it.
        self.remaps.lock().unwrap().push(old.generation, map);
        *self.live.write().unwrap() = Arc::new(next);
        drop(old_cursor);
        Ok(())
    }

    pub fn respond(&self, method: &str, path_and_query: &str) -> Response {
        let (path, query) = match path_and_query.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (path_and_query, None),
        };
        let snap = self.snapshot();

        match path {
            "/" | "/graph.json" | "/coalesced.json" | "/source" | "/search" | "/status"
                if method != "GET" =>
            {
                error(405, "method not allowed; use GET")
            }
            "/" => ok(HTML, self.index_html.as_bytes().to_vec()),
            "/graph.json" => self.graph_json(&snap, query),
            "/coalesced.json" => coalesced_response(&snap, &snap.cursor.lock().unwrap()),
            "/source" => match parse_id(query) {
                Ok(id) => source(&snap, &self.root, id),
                Err(msg) => error(400, msg),
            },
            "/search" => search(&snap, query),
            "/status" => self.status(&snap),
            p if p.starts_with("/ui/") && method != "GET" => {
                error(405, "method not allowed; use GET")
            }
            p if p.starts_with("/ui/") => ui_module(p),

            // The wire names are the older vocabulary, from before expanding
            // a node and zooming the drawing were told apart. They are a
            // contract with a shipped page, so they stay as they are.
            "/coalesced/zoom-in" | "/coalesced/zoom-out" | "/coalesced/reset" | "/watch"
            | "/reload"
                if method != "POST" =>
            {
                error(405, "method not allowed; use POST")
            }
            "/watch" => self.watch(&snap, query),
            "/reload" => {
                self.request_rebuild();
                self.status(&snap)
            }
            "/coalesced/reset" => match check_generation(&snap, query) {
                Ok(()) => {
                    let mut cursor = snap.cursor.lock().unwrap();
                    *cursor = Cursor::new(&snap.graph);
                    coalesced_response(&snap, &cursor)
                }
                Err(resp) => resp,
            },
            "/coalesced/zoom-in" | "/coalesced/zoom-out" => {
                let target = check_generation(&snap, query)
                    .and_then(|()| parse_id(query).map_err(|msg| error(400, msg)));
                match target {
                    Ok(id) => expand_or_collapse(&snap, id, path == "/coalesced/zoom-in"),
                    Err(resp) => resp,
                }
            }

            _ => error(404, &format!("no route for {method} {path}")),
        }
    }

    /// `?from=G` asks for `remap` relative to generation G instead of the
    /// previous one. Anything the history cannot answer (too old, not behind
    /// the live generation) gets the cached payload, whose `remap.from` then
    /// tells the client it has to reset.
    fn graph_json(&self, snap: &Snapshot, query: Option<&str>) -> Response {
        let cached = || ok(JSON, snap.graph_json.clone().into_bytes());
        let Some(raw) = param(query, "from") else { return cached() };
        let Ok(from) = raw.parse::<u64>() else {
            return error(400, "query parameter `from` must be a generation number");
        };
        if from + 1 >= snap.generation {
            return cached();
        }
        match self.remaps.lock().unwrap().compose(from, snap.generation) {
            Some(ids) => ok(JSON, snap.render_graph_json(Some(reload::remap_dto(from, &ids))).into_bytes()),
            None => cached(),
        }
    }

    fn status(&self, snap: &Snapshot) -> Response {
        let dto = StatusDto {
            generation: snap.generation,
            mode: self.mode().as_str(),
            dirty: self.dirty.load(Ordering::Relaxed),
            rebuilding: self.pending.load(Ordering::Relaxed) || self.rebuilding.load(Ordering::Relaxed),
            error: self.last_error.lock().unwrap().clone(),
        };
        ok(JSON, serde_json::to_vec(&dto).unwrap())
    }

    fn watch(&self, snap: &Snapshot, query: Option<&str>) -> Response {
        let mode = match param(query, "mode") {
            Some("static") => Mode::Static,
            Some("auto") => Mode::Auto,
            Some(_) => return error(400, "query parameter `mode` must be `static` or `auto`"),
            None => return error(400, "missing query parameter `mode`"),
        };
        self.mode.store(mode as u8, Ordering::Relaxed);
        if mode == Mode::Auto && self.dirty.load(Ordering::Relaxed) {
            self.request_rebuild();
        }
        self.status(snap)
    }
}

fn source(snap: &Snapshot, root: &Path, id: EntityId) -> Response {
    let Some(rel) = snap.graph.file_path(id) else {
        return error(404, &format!("entity {} has no source file", id.0));
    };
    let file_id = file_ancestor(&snap.graph, id);
    match snap.diff.as_ref().filter(|d| d.files.contains_key(&file_id)) {
        Some(d) => diff_source(snap.generation, file_id, &rel, d),
        None => match read_source(root, &rel) {
            Ok(text) => {
                let dto = SourceDto {
                    generation: snap.generation,
                    id: file_id.0,
                    path: wire_path(&rel),
                    text,
                    old_text: None,
                    ops: None,
                };
                ok(JSON, serde_json::to_vec(&dto).unwrap())
            }
            Err(resp) => resp,
        },
    }
}

/// Both sides of a changed file, plus the line ops that interleave them.
/// The texts come from the diff computed for this generation, not from disk:
/// the ops only line up with the exact texts they were computed from, and
/// the working tree may have moved on since. The side a one-sided file does
/// not have is empty rather than an error.
fn diff_source(generation: u64, file_id: EntityId, rel: &Path, d: &DiffState) -> Response {
    let file = &d.files[&file_id];
    let dto = SourceDto {
        generation,
        id: file_id.0,
        path: wire_path(rel),
        text: file.new_text.clone().unwrap_or_default(),
        old_text: Some(file.old_text.clone().unwrap_or_default()),
        ops: Some(file.ops.iter().map(dto::op_dto).collect()),
    };
    ok(JSON, serde_json::to_vec(&dto).unwrap())
}

/// `q` is percent-encoded, as query values are; a missing or non-UTF-8 `q`
/// is the only way this 400s, since an empty needle is a valid (if empty)
/// search.
fn search(snap: &Snapshot, query: Option<&str>) -> Response {
    let Some(raw) = param(query, "q") else { return error(400, "missing query parameter `q`") };
    let q = match percent_decode(raw) {
        Ok(q) => q,
        Err(()) => return error(400, "query parameter `q` is not valid UTF-8"),
    };
    let result = snap.index.search(&q, SEARCH_LIMITS);
    ok(JSON, serde_json::to_vec(&dto::search_dto(&result, snap.generation)).unwrap())
}

// Takes the caller's cursor guard rather than locking itself: an expand or
// reset that dropped the lock before rendering could answer with some other
// request's move once requests are served concurrently.
fn coalesced_response(snap: &Snapshot, cursor: &Cursor) -> Response {
    let coalesced = cursor.coalesced(&snap.graph);
    let status = snap.diff.as_ref().map(|d| d.reference_status.as_slice());
    ok(JSON, serde_json::to_vec(&dto::coalesced_dto(&coalesced, status, snap.generation)).unwrap())
}

// A no-op move is a 409 rather than a 200 with the unchanged payload so the
// UI can tell "nothing to expand here" apart from a successful move without
// diffing leaf sets.
fn expand_or_collapse(snap: &Snapshot, id: EntityId, down: bool) -> Response {
    let mut cursor = snap.cursor.lock().unwrap();
    let moved = if down { cursor.move_down(id, &snap.graph) } else { cursor.move_up(id, &snap.graph) };
    if moved {
        return coalesced_response(snap, &cursor);
    }

    let Some(entity) = snap.graph.get(id) else {
        return error(409, &format!("unknown entity id {}", id.0));
    };
    if !cursor.leaves.contains(&id) {
        return error(409, &format!("entity {} is not a current leaf", id.0));
    }
    if down {
        error(409, &format!("entity {} has no children", id.0))
    } else {
        debug_assert!(entity.parent.is_none());
        error(409, &format!("entity {} is a root", id.0))
    }
}

/// An expand names an entity by id, and ids are per generation, so a request
/// that crossed a rebuild would act on some other entity: the caller must say
/// which generation it meant.
fn check_generation(snap: &Snapshot, query: Option<&str>) -> Result<(), Response> {
    let requested = param(query, "generation")
        .ok_or_else(|| error(400, "missing query parameter `generation`"))?
        .parse::<u64>()
        .map_err(|_| error(400, "query parameter `generation` must be a non-negative integer"))?;
    if requested != snap.generation {
        return Err(error(
            409,
            &format!("graph changed (generation {}, request was for {requested})", snap.generation),
        ));
    }
    Ok(())
}

/// Read `rel` under `base`, refusing anything the viewer cannot render. The
/// error arm is the response to send.
fn read_source(base: &Path, rel: &Path) -> Result<String, Response> {
    let path = base.join(rel);

    // Both sides canonicalized so symlinked roots compare equal; the check
    // is what stops a hierarchy path containing `..` from escaping the
    // project (the producer contract forbids it, but the index is data).
    let (real, real_root) = match (path.canonicalize(), base.canonicalize()) {
        (Ok(p), Ok(r)) => (p, r),
        (Err(e), _) | (_, Err(e)) => {
            return Err(error(404, &format!("cannot read {}: {e}", path.display())));
        }
    };
    if !real.starts_with(&real_root) {
        return Err(error(403, &format!("{} is outside the project root", rel.display())));
    }

    let meta = match std::fs::metadata(&real) {
        Ok(m) => m,
        Err(e) => return Err(error(404, &format!("cannot read {}: {e}", path.display()))),
    };
    if meta.len() > MAX_SOURCE_BYTES {
        return Err(error(
            413,
            &format!("{} is {} bytes; limit is {MAX_SOURCE_BYTES}", rel.display(), meta.len()),
        ));
    }
    let bytes = match std::fs::read(&real) {
        Ok(b) => b,
        Err(e) => return Err(error(404, &format!("cannot read {}: {e}", path.display()))),
    };
    String::from_utf8(bytes)
        .map_err(|_| error(415, &format!("{} is not valid UTF-8", rel.display())))
}

/// Reads each File entity the same way `/source` would, so `/search` finds
/// exactly the text the code pane can open: in diff mode, a changed file's
/// new side, or the old side for a file that only exists on the base tree
/// (its line numbers are then old-side line numbers, matching where the code
/// pane opens a removed entity).
pub(crate) fn build_text_index(
    graph: &EntityGraph,
    root: &Path,
    diff_files: Option<&HashMap<EntityId, FileDiff>>,
) -> TextIndex {
    TextIndex::build(graph, |id| {
        let rel = graph.file_path(id)?;
        let text = match diff_files.and_then(|files| files.get(&id)) {
            Some(diff) => diff.new_text.clone().or_else(|| diff.old_text.clone())?,
            None => read_source(root, &rel).ok()?,
        };
        Some((wire_path(&rel), text))
    })
}

fn file_ancestor(graph: &EntityGraph, id: EntityId) -> EntityId {
    let mut cur = graph.get(id).expect("file_path succeeded, so the id exists");
    while cur.kind != entity_graph::EntityKind::File {
        cur = graph.get(cur.parent.expect("file_path found a File ancestor")).unwrap();
    }
    cur.id
}

fn wire_path(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// `+` decodes to space and `%XX` (either hex case) to its byte; a `%` not
/// followed by two hex digits, including at the end of the string, is kept
/// literally rather than treated as an error.
fn percent_decode(s: &str) -> Result<String, ()> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                // `from_str_radix` alone would also accept a sign (`%+f`).
                let hex = bytes
                    .get(i + 1..i + 3)
                    .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
                    .and_then(|h| std::str::from_utf8(h).ok());
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

fn param<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query.unwrap_or("").split('&').find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
}

fn parse_id(query: Option<&str>) -> Result<EntityId, &'static str> {
    let raw = param(query, "id").ok_or("missing query parameter `id`")?;
    raw.parse::<usize>().map(EntityId).map_err(|_| "query parameter `id` must be a non-negative integer")
}

fn ok(content_type: &'static str, body: Vec<u8>) -> Response {
    Response { status: 200, content_type, body }
}

fn ui_module(path: &str) -> Response {
    let name = path.trim_start_matches("/ui/");
    match UI_MODULES.iter().chain(VENDOR).find(|(n, _)| *n == name) {
        Some((_, src)) => {
            let content_type = if name.ends_with(".css") { CSS } else { JS };
            ok(content_type, src.as_bytes().to_vec())
        }
        None => error(404, &format!("no route for GET {path}")),
    }
}

pub(crate) fn error(status: u16, message: &str) -> Response {
    let body = serde_json::to_vec(&ErrorDto { error: message.to_string() }).unwrap();
    Response { status, content_type: JSON, body }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use entity_graph::EntityKind::{self, *};
    use entity_graph::ReferenceKind::*;
    use entity_graph::Site;
    use entity_graph::test_support::graph_from_parents;
    use serde_json::{Value, json};

    use super::*;

    /// Mirrors `ui/fixture.json` node for node. `graph_from_parents` only
    /// knows names and kinds, so paths, line ranges and sites are overlaid
    /// after.
    fn fixture_graph() -> EntityGraph {
        let rows: &[(&str, EntityKind, Option<usize>, &str, usize, usize)] = &[
            ("demo", Folder, None, "demo", 0, 0),
            ("src", Folder, Some(0), "demo/src", 0, 0),
            ("main.rs", File, Some(1), "demo/src/main.rs", 0, 30),
            ("main", Function, Some(2), "demo/src/main.rs/main", 2, 12),
            ("helper", Function, Some(2), "demo/src/main.rs/helper", 14, 20),
            ("lib.rs", File, Some(1), "demo/src/lib.rs", 0, 80),
            ("Point", Class, Some(5), "demo/src/lib.rs/Point", 1, 6),
            ("new", Function, Some(6), "demo/src/lib.rs/Point/new", 8, 12),
            ("distance", Function, Some(6), "demo/src/lib.rs/Point/distance", 13, 20),
            ("geometry", Module, Some(5), "demo/src/lib.rs/geometry", 22, 60),
            ("area", Function, Some(9), "demo/src/lib.rs/geometry/area", 24, 40),
            ("tests", Folder, Some(0), "demo/tests", 0, 0),
            ("smoke.rs", File, Some(11), "demo/tests/smoke.rs", 0, 25),
            ("test_area", Function, Some(12), "demo/tests/smoke.rs/test_area", 3, 15),
        ];
        let parents: Vec<_> = rows.iter().map(|r| (r.0, r.1, r.2)).collect();
        let references = [
            (2, 6, Import),
            (3, 7, Call),
            (3, 4, Call),
            (4, 8, Call),
            (10, 8, Call),
            (10, 6, TypeRef),
            (12, 9, Import),
            (13, 10, Call),
            (8, 6, TypeRef),
        ];
        let mut graph = graph_from_parents(&parents, &references);
        for (entity, row) in graph.entities.iter_mut().zip(rows) {
            entity.path = PathBuf::from(row.3);
            entity.line_range = row.4..row.5;
        }
        graph.references[1].sites = vec![Site { line: 4 }, Site { line: 9 }];
        graph.references[6].sites = vec![Site { line: 0 }];
        graph
    }

    const INDEX: &str = "<!doctype html><title>t</title>";

    fn fixture_loader() -> Loader {
        Box::new(|| Ok(Loaded::Graph(fixture_graph())))
    }

    /// Re-parses both trees with tree-sitter on every call, so a rebuild sees
    /// whatever the test wrote to disk in between.
    fn diff_loader(old_root: PathBuf, new_root: PathBuf) -> Loader {
        Box::new(move || {
            let old = treesitter_producer::graph_from_path(&old_root)?;
            let new = treesitter_producer::graph_from_path(&new_root)?;
            let diff = graph_diff::diff(&old, &old_root, &new, &new_root)?;
            Ok(Loaded::Diff { diff, base_label: "HEAD~1".into(), base_commit: "0123abcd".into() })
        })
    }

    fn server_at(loader: Loader, root: PathBuf) -> Server {
        Server::new(loader, root, INDEX, Mode::Static).unwrap()
    }

    fn server() -> Server {
        server_at(fixture_loader(), PathBuf::from("/nonexistent/demo"))
    }

    fn json(resp: &Response) -> Value {
        assert_eq!(resp.content_type, JSON);
        serde_json::from_slice(&resp.body).unwrap()
    }

    /// The node at a wire `path` in a `/graph.json` payload.
    fn node<'a>(graph: &'a Value, path: &str) -> &'a Value {
        graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["path"] == path)
            .unwrap_or_else(|| panic!("no node at {path}"))
    }

    fn id_of(graph: &Value, path: &str) -> u64 {
        node(graph, path)["id"].as_u64().unwrap()
    }

    #[test]
    fn graph_dto_matches_frontend_fixture() {
        let actual = serde_json::to_value(dto::graph_dto(&fixture_graph(), 1, None)).unwrap();
        let expected: Value = serde_json::from_str(include_str!("../ui/fixture.json")).unwrap();
        assert_eq!(actual, expected);
    }

    /// Walks the full expand contract against the fixture: root view, a
    /// successful expand whose payload must equal `ui/fixture.coalesced.json`,
    /// then every rejection class (409 no-op, 409 unknown id, 400 bad query,
    /// 404 route, 405 method).
    #[test]
    fn expand_sequence_follows_contract() {
        let s = server();
        let at_root = json!({ "generation": 1, "leaves": [0], "edges": [] });

        let root = s.respond("GET", "/coalesced.json");
        assert_eq!(root.status, 200);
        assert_eq!(json(&root), at_root);

        let expanded = s.respond("POST", "/coalesced/zoom-in?id=0&generation=1");
        assert_eq!(expanded.status, 200);
        let expected: Value =
            serde_json::from_str(include_str!("../ui/fixture.coalesced.json")).unwrap();
        assert_eq!(json(&expanded), expected);

        let again = s.respond("POST", "/coalesced/zoom-in?id=0&generation=1");
        assert_eq!(again.status, 409);
        assert!(json(&again)["error"].as_str().unwrap().contains("not a current leaf"));

        let out = s.respond("POST", "/coalesced/zoom-out?id=1&generation=1");
        assert_eq!(out.status, 200);
        assert_eq!(json(&out), at_root);

        let root_out = s.respond("POST", "/coalesced/zoom-out?id=0&generation=1");
        assert_eq!(root_out.status, 409);
        assert!(json(&root_out)["error"].as_str().unwrap().contains("root"));

        let unknown = s.respond("POST", "/coalesced/zoom-in?id=999&generation=1");
        assert_eq!(unknown.status, 409);
        assert!(json(&unknown)["error"].as_str().unwrap().contains("unknown"));

        assert_eq!(s.respond("POST", "/coalesced/zoom-in?generation=1").status, 400);
        assert_eq!(s.respond("POST", "/coalesced/zoom-in?id=abc&generation=1").status, 400);
        assert_eq!(s.respond("GET", "/nope").status, 404);
        assert_eq!(s.respond("GET", "/coalesced/zoom-in?id=0&generation=1").status, 405);
        assert_eq!(s.respond("POST", "/graph.json").status, 405);

        s.respond("POST", "/coalesced/zoom-in?id=0&generation=1");
        s.respond("POST", "/coalesced/zoom-in?id=1&generation=1");
        let reset = s.respond("POST", "/coalesced/reset?generation=1");
        assert_eq!(reset.status, 200);
        assert_eq!(json(&reset), at_root);
    }

    /// `/source` resolves any entity to its File ancestor's path under the
    /// project root and serves the text; folders, unknown ids, missing files
    /// and non-UTF-8 files each map to their own status.
    #[test]
    fn source_serves_file_of_any_entity_and_rejects_the_rest() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("demo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\nfn helper() {}\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), [0xffu8, 0xfe, b'x']).unwrap();
        let s = server_at(fixture_loader(), root);

        // A function id resolves to its file (id 2) and its path.
        let by_fn = s.respond("GET", "/source?id=4");
        assert_eq!(by_fn.status, 200);
        assert_eq!(
            json(&by_fn),
            json!({ "generation": 1, "id": 2, "path": "src/main.rs", "text": "fn main() {}\nfn helper() {}\n" })
        );
        assert_eq!(json(&s.respond("GET", "/source?id=2")), json(&by_fn));

        let folder = s.respond("GET", "/source?id=1");
        assert_eq!(folder.status, 404);
        assert_eq!(json(&folder)["error"], "entity 1 has no source file");

        assert_eq!(s.respond("GET", "/source?id=999").status, 404);
        // smoke.rs exists in the graph but not on disk.
        let stale = s.respond("GET", "/source?id=13");
        assert_eq!(stale.status, 404);
        assert!(json(&stale)["error"].as_str().unwrap().contains("smoke.rs"));

        assert_eq!(s.respond("GET", "/source?id=6").status, 415);
        assert_eq!(s.respond("GET", "/source").status, 400);
        assert_eq!(s.respond("GET", "/source?id=x").status, 400);
        assert_eq!(s.respond("POST", "/source?id=2").status, 405);
    }

    /// Single-file load: the root is the File and its `file_path` is empty,
    /// so the loaded path itself is served.
    #[test]
    fn source_of_single_file_root_is_the_loaded_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("only.rs");
        std::fs::write(&file, "fn f() {}\n").unwrap();
        let loader: Loader = Box::new(|| {
            Ok(Loaded::Graph(graph_from_parents(&[("only.rs", File, None), ("f", Function, Some(0))], &[])))
        });
        let s = server_at(loader, file);

        let resp = s.respond("GET", "/source?id=1");
        assert_eq!(resp.status, 200);
        assert_eq!(json(&resp), json!({ "generation": 1, "id": 0, "path": "", "text": "fn f() {}\n" }));
    }

    /// `/search` end to end over a temp tree matching `fixture_graph()`'s
    /// file paths (`tests/smoke.rs` is deliberately left off disk, so its
    /// entity contributes no documents): unrestricted search, the `kind:`
    /// prefix syntax, percent-decoding, case folding, a needle under 3 chars,
    /// the content limit and its `more` count, and the file/path dedupe rule
    /// in both directions.
    #[test]
    fn search_finds_files_paths_and_content() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("demo");

        // 106 lines, 103 of which contain "main" — enough to trip the
        // content limit (100) and prove `more` counts the rest.
        let mut main_rs = String::from("fn main() {\n    helper();\n}\nfn helper() {}\n");
        for i in 0..102 {
            main_rs.push_str(&format!("// main line {i}\n"));
        }
        write_tree(&root, &[
            ("src/main.rs", &main_rs),
            ("src/lib.rs", "// calls main eventually\nfn lib_fn() {}\n"),
        ]);
        let s = server_at(fixture_loader(), root);

        // Unrestricted "main": the symbol hit leads -- a thing called `main`
        // beats a file whose name contains it -- then the file hit for
        // main.rs, with offsets matching the contract example exactly. The
        // path hit is suppressed
        // (the match sits in the filename tail, the same occurrence the file
        // hit already shows); content hits are ordered by path then line and
        // truncated to the limit.
        let main = json(&s.respond("GET", "/search?q=main"));
        assert_eq!(main["query"], "main");
        let hits = main["hits"].as_array().unwrap();
        assert_eq!(
            hits[0],
            json!({ "kind": "symbol", "id": 2, "path": "src/main.rs", "line": 2, "text": "main", "start": 0, "end": 4 })
        );
        assert_eq!(
            hits[1],
            json!({ "kind": "file", "id": 2, "path": "src/main.rs", "text": "main.rs", "start": 0, "end": 4 })
        );
        assert!(hits.iter().all(|h| h["kind"] != "path"), "main.rs's path hit must be suppressed: {hits:?}");
        let content: Vec<_> = hits.iter().filter(|h| h["kind"] == "content").collect();
        assert_eq!(content.len(), 100);
        assert_eq!((content[0]["path"].as_str(), content[0]["line"].as_i64()), (Some("src/lib.rs"), Some(0)));
        assert_eq!((content[1]["path"].as_str(), content[1]["line"].as_i64()), (Some("src/main.rs"), Some(0)));
        assert_eq!(main["more"], json!({ "file": 0, "path": 0, "content": 4, "symbol": 0 }));

        // A match confined to a directory segment ("src") is a different
        // occurrence from any filename tail and is kept, unlike above.
        let src = json(&s.respond("GET", "/search?q=src"));
        let src_hits = src["hits"].as_array().unwrap();
        assert!(src_hits.iter().all(|h| h["kind"] == "path"), "{src_hits:?}");
        assert_eq!(src_hits.len(), 2);

        // Restricted by prefix: only path hits, shortest path first.
        let path_only = json(&s.respond("GET", "/search?q=path:src"));
        assert_eq!(path_only["query"], "path:src");
        let hits = path_only["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0]["path"], "src/lib.rs");
        assert_eq!(hits[1]["path"], "src/main.rs");

        // `content:` prefix + case folding: "FN" finds a lowercase "fn" line.
        let fn_hits = json(&s.respond("GET", "/search?q=content:FN"));
        let lib_fn =
            fn_hits["hits"].as_array().unwrap().iter().find(|h| h["text"] == "fn lib_fn() {}").unwrap();
        assert_eq!((lib_fn["start"].as_i64(), lib_fn["end"].as_i64()), (Some(0), Some(2)));

        // A 2-char needle bypasses the postings (linear scan); "rs" matches
        // both file names, but its only path occurrence in each is the same
        // filename tail, so both path hits are suppressed too.
        let rs = json(&s.respond("GET", "/search?q=rs"));
        let rs_hits = rs["hits"].as_array().unwrap();
        let file_paths: Vec<_> =
            rs_hits.iter().filter(|h| h["kind"] == "file").map(|h| h["path"].as_str().unwrap()).collect();
        assert_eq!(file_paths, vec!["src/lib.rs", "src/main.rs"]);
        assert!(rs_hits.iter().all(|h| h["kind"] != "path"), "{rs_hits:?}");

        // Percent-decoding: "+" is a space, which splits terms; both must be
        // on the line, in any order, unless quoted into one phrase.
        let decoded = json(&s.respond("GET", "/search?q=fn+main"));
        assert_eq!(decoded["query"], "fn main");
        assert!(decoded["hits"].as_array().unwrap().iter().any(|h| h["text"] == "fn main() {"));
        assert!(json(&s.respond("GET", "/search?q=main+fn"))["hits"].as_array().unwrap().iter().any(|h| h["text"] == "fn main() {"));
        assert!(json(&s.respond("GET", "/search?q=%22main+fn%22"))["hits"].as_array().unwrap().is_empty());

        // Mixed terms: a tag on another kind filters by file. "main" lines in
        // files named "lib": one content hit and nothing else, since lib.rs's
        // own name and path do not contain "main".
        let mixed = json(&s.respond("GET", "/search?q=main+file:lib"));
        let mixed_hits = mixed["hits"].as_array().unwrap();
        assert_eq!(mixed_hits.len(), 1, "{mixed_hits:?}");
        assert_eq!((mixed_hits[0]["kind"].as_str(), mixed_hits[0]["path"].as_str(), mixed_hits[0]["line"].as_i64()), (Some("content"), Some("src/lib.rs"), Some(0)));
        // The other way round: files named "lib" that contain "main" is a
        // file hit (marked on the name) plus the same line hit.
        let both = json(&s.respond("GET", "/search?q=file:lib+content:main"));
        let kinds: Vec<_> = both["hits"].as_array().unwrap().iter().map(|h| h["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["file", "content"]);

        assert_eq!(s.respond("GET", "/search").status, 400);
        assert_eq!(s.respond("POST", "/search?q=main").status, 405);
        assert_eq!(json(&s.respond("GET", "/search?q=")), json!({
            "generation": 1, "query": "", "hits": [], "more": { "file": 0, "path": 0, "content": 0, "symbol": 0 }
        }));
    }

    /// Diff mode end to end over two hand-built trees (no git). The union
    /// graph carries the base labels and per-node status and churn, coalesced
    /// edges carry their member references plus an aggregated status (`mixed`
    /// where a bundle holds both an unchanged and an added reference), and
    /// `/source` serves both sides of a changed file with the ops that
    /// interleave them — reading a removed file out of the base tree.
    #[test]
    fn diff_mode_payloads_carry_change_tags() {
        const LIB: &str =
            "/// Doc.\nfn alpha() {\n    let x = 1;\n}\n\nfn beta() {\n    alpha();\n}\n";
        const LIB_EDITED: &str = "/// Doc.\nfn alpha() {\n    let x = 1;\n    let y = 2;\n}\n\n\
                                  fn beta() {\n    alpha();\n}\n";
        const UTIL: &str = "fn one() {\n    alpha();\n}\n\nfn two() {\n    1\n}\n";
        const UTIL_EDITED: &str = "fn one() {\n    alpha();\n}\n\nfn two() {\n    beta();\n}\n";

        let trees = tempfile::TempDir::new().unwrap();
        let old_root = trees.path().join("base/proj");
        let new_root = trees.path().join("work/proj");
        write_tree(&old_root, &[
            ("src/lib.rs", LIB),
            ("src/util.rs", UTIL),
            ("src/gone.rs", "fn gone() {\n    alpha();\n}\n"),
        ]);
        write_tree(&new_root, &[("src/lib.rs", LIB_EDITED), ("src/util.rs", UTIL_EDITED)]);
        let s = server_at(diff_loader(old_root, new_root.clone()), new_root);

        let graph = json(&s.respond("GET", "/graph.json"));
        assert_eq!(graph["diff"], json!({ "base": "HEAD~1", "base_commit": "0123abcd" }));
        let alpha = node(&graph, "proj/src/lib.rs/alpha");
        assert_eq!(alpha["status"], "modified");
        assert_eq!(alpha["added"], 1);
        assert_eq!(alpha["removed"], 0);
        assert_eq!(node(&graph, "proj/src/lib.rs/beta")["status"], "same");
        assert_eq!(node(&graph, "proj/src/gone.rs")["status"], "removed");
        assert_eq!(node(&graph, "proj/src/gone.rs")["removed"], 3);
        assert_eq!(node(&graph, "proj/src")["status"], "modified");

        // Expand to the file level: every edge names its member references, and
        // the util.rs -> lib.rs bundle holds an unchanged and an added call.
        s.respond("POST", "/coalesced/zoom-in?id=0&generation=1");
        let src_id = id_of(&graph, "proj/src");
        let view = json(&s.respond("POST", &format!("/coalesced/zoom-in?id={src_id}&generation=1")));
        let edges = view["edges"].as_array().unwrap();
        assert!(edges.iter().all(|e| !e["refs"].as_array().unwrap().is_empty()), "{edges:?}");
        let edge = |from: &str, to: &str| {
            edges
                .iter()
                .find(|e| e["from"] == id_of(&graph, from) && e["to"] == id_of(&graph, to))
                .unwrap_or_else(|| panic!("no edge {from} -> {to} in {edges:?}"))
        };
        assert_eq!(edge("proj/src/util.rs", "proj/src/lib.rs")["status"], "mixed");
        assert_eq!(edge("proj/src/gone.rs", "proj/src/lib.rs")["status"], "removed");

        let lib_id = id_of(&graph, "proj/src/lib.rs");
        let source = json(&s.respond("GET", &format!("/source?id={lib_id}")));
        assert_eq!(source["text"], LIB_EDITED);
        assert_eq!(source["old_text"], LIB);
        assert_eq!(source["ops"], json!([["=", 0, 3, 0, 3], ["+", 3, 0, 3, 1], ["=", 3, 5, 4, 5]]));

        // The removed file only exists in the base tree.
        let gone_id = id_of(&graph, "proj/src/gone.rs");
        let removed = json(&s.respond("GET", &format!("/source?id={gone_id}")));
        assert_eq!(removed["text"], "");
        assert_eq!(removed["old_text"], "fn gone() {\n    alpha();\n}\n");
        assert_eq!(removed["ops"], json!([["-", 0, 3, 0, 0]]));

        // An unchanged file keeps the plain shape, so the UI reads absent ops
        // as "nothing to interleave".
        let util_id = id_of(&graph, "proj/src/util.rs");
        let util = json(&s.respond("GET", &format!("/source?id={util_id}")));
        assert!(util.get("ops").is_some(), "util.rs did change");
        assert_eq!(util["old_text"], UTIL);
    }

    /// A rebuild in diff mode re-diffs the working tree against the same
    /// base: a function untouched at load and edited since flips to
    /// `modified`, and `/source` serves the text the new ops were computed
    /// from.
    #[test]
    fn diff_mode_rebuild_retags_the_working_side() {
        const LIB: &str = "fn alpha() {\n    let x = 1;\n}\n\nfn beta() {\n    alpha();\n}\n";
        const LIB_EDITED: &str =
            "fn alpha() {\n    let x = 1;\n}\n\nfn beta() {\n    alpha();\n    alpha();\n}\n";

        let trees = tempfile::TempDir::new().unwrap();
        let old_root = trees.path().join("base/proj");
        let new_root = trees.path().join("work/proj");
        write_tree(&old_root, &[("src/lib.rs", LIB)]);
        write_tree(&new_root, &[("src/lib.rs", LIB)]);
        let s = server_at(diff_loader(old_root, new_root.clone()), new_root.clone());

        let before = json(&s.respond("GET", "/graph.json"));
        assert_eq!(node(&before, "proj/src/lib.rs/beta")["status"], "same");
        assert!(json(&s.respond("GET", &format!("/source?id={}", id_of(&before, "proj/src/lib.rs"))))
            .get("ops")
            .is_none());

        write_tree(&new_root, &[("src/lib.rs", LIB_EDITED)]);
        s.rebuild().unwrap();

        let after = json(&s.respond("GET", "/graph.json"));
        assert_eq!(after["generation"], 2);
        assert_eq!(after["diff"], before["diff"]);
        assert_eq!(node(&after, "proj/src/lib.rs/beta")["status"], "modified");
        assert_eq!(node(&after, "proj/src/lib.rs/alpha")["status"], "same");
        let source = json(&s.respond("GET", &format!("/source?id={}", id_of(&after, "proj/src/lib.rs"))));
        assert_eq!(source["generation"], 2);
        assert_eq!(source["text"], LIB_EDITED);
        assert_eq!(source["old_text"], LIB);
        assert!(source["ops"].as_array().is_some_and(|ops| ops.iter().any(|op| op[0] == "+")));
    }

    /// `/search` indexes the same diff-aware text `/source` does: a removed
    /// file only from its old side, a changed file from its new side.
    #[test]
    fn search_indexes_diff_aware_text() {
        const LIB: &str = "fn alpha() {\n    let x = 1;\n}\n";
        const LIB_EDITED: &str = "fn alpha() {\n    let x = 1;\n    let y = 2;\n}\n";
        const GONE: &str = "fn gone() {\n    alpha();\n}\n";

        let trees = tempfile::TempDir::new().unwrap();
        let old_root = trees.path().join("base/proj");
        let new_root = trees.path().join("work/proj");
        write_tree(&old_root, &[("src/lib.rs", LIB), ("src/gone.rs", GONE)]);
        write_tree(&new_root, &[("src/lib.rs", LIB_EDITED)]);
        let s = server_at(diff_loader(old_root, new_root.clone()), new_root);

        let gone = json(&s.respond("GET", "/search?q=content:gone"));
        let texts: Vec<_> =
            gone["hits"].as_array().unwrap().iter().map(|h| h["text"].as_str().unwrap()).collect();
        assert!(texts.iter().any(|t| t.contains("fn gone()")), "{texts:?}");

        // "let y = 2;" only exists on the new side.
        let edited = json(&s.respond("GET", "/search?q=content:y+%3D+2"));
        let texts: Vec<_> =
            edited["hits"].as_array().unwrap().iter().map(|h| h["text"].as_str().unwrap()).collect();
        assert!(texts.iter().any(|t| t.contains("let y = 2;")), "{texts:?}");
    }

    /// The live-update contract across one real rebuild of a tree-sitter
    /// tree: an expansion of `src` survives an edit that grows `a.rs`, adds
    /// `c.rs` and deletes `b.rs`; `/graph.json` carries the id remap from the
    /// previous generation; an expand posted for the old generation is refused;
    /// `/search` sees the new text and stamps the generation; `/status`
    /// reports the shape the UI polls. Then a failing loader leaves the live
    /// generation in place and reports the error until the next success.
    #[test]
    fn live_rebuild_keeps_the_expansion_and_remaps_ids() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("proj");
        write_tree(&root, &[("src/a.rs", "fn a_one() {}\n"), ("src/b.rs", "fn b_one() {}\n")]);
        let fail = Arc::new(Mutex::new(false));
        let loader: Loader = {
            let (fail, root) = (fail.clone(), root.clone());
            Box::new(move || {
                if *fail.lock().unwrap() {
                    anyhow::bail!("indexer exploded");
                }
                Ok(Loaded::Graph(treesitter_producer::graph_from_path(&root)?))
            })
        };
        let s = server_at(loader, root.clone());

        let g1 = json(&s.respond("GET", "/graph.json"));
        assert_eq!(g1["generation"], 1);
        assert!(g1.get("remap").is_none());
        let (old_src, old_a, old_b) =
            (id_of(&g1, "proj/src"), id_of(&g1, "proj/src/a.rs"), id_of(&g1, "proj/src/b.rs"));
        assert_eq!(s.respond("POST", &format!("/coalesced/zoom-in?id={}&generation=1", id_of(&g1, "proj"))).status, 200);
        let expanded = json(&s.respond("POST", &format!("/coalesced/zoom-in?id={old_src}&generation=1")));
        assert_eq!(expanded["generation"], 1);
        assert_eq!(leaf_set(&expanded), [old_a, old_b].into_iter().collect());

        write_tree(&root, &[("src/a.rs", "fn a_one() {}\nfn a_two() {}\n"), ("src/c.rs", "fn c_one() {}\n")]);
        std::fs::remove_file(root.join("src/b.rs")).unwrap();
        s.rebuild().unwrap();

        let g2 = json(&s.respond("GET", "/graph.json"));
        assert_eq!(g2["generation"], 2);
        assert_eq!(g2["remap"]["from"], 1);
        let ids = g2["remap"]["ids"].as_array().unwrap();
        assert_eq!(ids.len(), g1["nodes"].as_array().unwrap().len());
        assert_eq!(ids[old_a as usize], id_of(&g2, "proj/src/a.rs"));
        assert_eq!(ids[old_b as usize], Value::Null);
        assert_eq!(ids[old_src as usize], id_of(&g2, "proj/src"));

        let view = json(&s.respond("GET", "/coalesced.json"));
        assert_eq!(view["generation"], 2);
        assert_eq!(
            leaf_set(&view),
            [id_of(&g2, "proj/src/a.rs"), id_of(&g2, "proj/src/c.rs")].into_iter().collect(),
            "the zoom into src survives with its new file set"
        );

        let stale = s.respond("POST", &format!("/coalesced/zoom-in?id={old_a}&generation=1"));
        assert_eq!(stale.status, 409);
        assert_eq!(json(&stale)["error"], "graph changed (generation 2, request was for 1)");
        assert_eq!(s.respond("POST", &format!("/coalesced/zoom-in?id={}&generation=2", id_of(&g2, "proj/src/a.rs"))).status, 200);

        let found = json(&s.respond("GET", "/search?q=a_two"));
        assert_eq!(found["generation"], 2);
        assert!(found["hits"].as_array().unwrap().iter().any(|h| h["text"] == "fn a_two() {}"));

        assert_eq!(
            json(&s.respond("GET", "/status")),
            json!({ "generation": 2, "mode": "static", "dirty": false, "rebuilding": false, "error": null })
        );

        *fail.lock().unwrap() = true;
        assert!(s.rebuild().is_err());
        let status = json(&s.respond("GET", "/status"));
        assert_eq!(status["generation"], 2);
        assert_eq!(status["dirty"], true, "a failed rebuild leaves the sources unreflected");
        assert!(status["error"].as_str().unwrap().contains("indexer exploded"));
        assert_eq!(json(&s.respond("GET", "/graph.json"))["generation"], 2);

        *fail.lock().unwrap() = false;
        s.rebuild().unwrap();
        let status = json(&s.respond("GET", "/status"));
        assert_eq!((status["generation"].as_u64(), &status["error"]), (Some(3), &Value::Null));
    }

    /// A client two generations behind asks `/graph.json?from=1` and gets a
    /// remap composed across both rebuilds: `a.rs` follows its shifting id,
    /// `b.rs` (deleted in the first rebuild) is null. Asking from the
    /// previous, the live or an unparsable generation degrades to the cached
    /// payload or a 400.
    #[test]
    fn graph_json_remaps_from_an_older_generation() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("proj");
        write_tree(&root, &[("src/a.rs", "fn a() {}\n"), ("src/b.rs", "fn b() {}\n")]);
        let loader: Loader = {
            let root = root.clone();
            Box::new(move || Ok(Loaded::Graph(treesitter_producer::graph_from_path(&root)?)))
        };
        let s = server_at(loader, root.clone());
        let g1 = json(&s.respond("GET", "/graph.json"));
        let (a1, b1) = (id_of(&g1, "proj/src/a.rs"), id_of(&g1, "proj/src/b.rs"));

        std::fs::remove_file(root.join("src/b.rs")).unwrap();
        s.rebuild().unwrap();
        write_tree(&root, &[("src/0.rs", "fn zero() {}\n")]);
        s.rebuild().unwrap();

        let g3 = json(&s.respond("GET", "/graph.json?from=1"));
        assert_eq!(g3["generation"], 3);
        assert_eq!(g3["remap"]["from"], 1);
        let ids = g3["remap"]["ids"].as_array().unwrap();
        assert_eq!(ids.len(), g1["nodes"].as_array().unwrap().len());
        assert_eq!(ids[a1 as usize], id_of(&g3, "proj/src/a.rs"));
        assert_eq!(ids[b1 as usize], Value::Null);

        let cached = json(&s.respond("GET", "/graph.json"));
        assert_eq!(cached["remap"]["from"], 2);
        assert_eq!(json(&s.respond("GET", "/graph.json?from=2")), cached);
        assert_eq!(json(&s.respond("GET", "/graph.json?from=3")), cached);
        assert_eq!(json(&s.respond("GET", "/graph.json?from=0")), cached, "never recorded: the client resets");
        assert_eq!(s.respond("GET", "/graph.json?from=abc").status, 400);
    }

    /// The flag routes: `/status` defaults to static, `/watch` switches mode
    /// (and asks for a rebuild when switching to auto while dirty), the
    /// watcher's `mark_dirty` shows up, `/reload` shows `rebuilding` until a
    /// `rebuild()` clears both flags and bumps the generation; plus the
    /// rejection classes.
    #[test]
    fn watch_mode_and_reload_routes() {
        let s = server();
        let status = |s: &Server| json(&s.respond("GET", "/status"));
        assert_eq!(
            status(&s),
            json!({ "generation": 1, "mode": "static", "dirty": false, "rebuilding": false, "error": null })
        );

        let auto = json(&s.respond("POST", "/watch?mode=auto"));
        assert_eq!(auto["mode"], "auto");
        assert_eq!(s.mode(), Mode::Auto);
        assert!(!s.take_request(), "switching to auto while clean requests nothing");
        assert_eq!(json(&s.respond("POST", "/watch?mode=static"))["mode"], "static");

        s.mark_dirty();
        assert_eq!(status(&s)["dirty"], true);
        assert_eq!(json(&s.respond("POST", "/watch?mode=auto"))["dirty"], true);
        assert!(s.take_request(), "switching to auto while dirty asks the watcher to rebuild");
        s.respond("POST", "/watch?mode=static");

        let reload = json(&s.respond("POST", "/reload"));
        assert_eq!((reload["rebuilding"].as_bool(), reload["dirty"].as_bool()), (Some(true), Some(true)));
        assert!(s.take_request());
        s.rebuild().unwrap();
        assert_eq!(
            status(&s),
            json!({ "generation": 2, "mode": "static", "dirty": false, "rebuilding": false, "error": null })
        );

        assert_eq!(s.respond("POST", "/watch?mode=fast").status, 400);
        assert_eq!(s.respond("POST", "/watch").status, 400);
        assert_eq!(s.respond("GET", "/watch?mode=auto").status, 405);
        assert_eq!(s.respond("GET", "/reload").status, 405);
        assert_eq!(s.respond("POST", "/status").status, 405);
        assert_eq!(s.respond("POST", "/coalesced/zoom-in?id=0").status, 400);
        assert_eq!(s.respond("POST", "/coalesced/zoom-in?id=0&generation=x").status, 400);
        assert_eq!(s.respond("POST", "/coalesced/reset").status, 400);
    }

    fn leaf_set(view: &Value) -> std::collections::HashSet<u64> {
        view["leaves"].as_array().unwrap().iter().map(|l| l.as_u64().unwrap()).collect()
    }

    fn write_tree(root: &std::path::Path, files: &[(&str, &str)]) {
        for (rel, body) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
    }

    #[test]
    fn static_routes_have_content_types_and_bodies() {
        let s = server();

        let index = s.respond("GET", "/");
        assert_eq!(index.status, 200);
        assert_eq!(index.content_type, HTML);
        assert!(!index.body.is_empty());

        let graph = s.respond("GET", "/graph.json");
        assert_eq!(graph.status, 200);
        assert_eq!(graph.content_type, JSON);
        assert_eq!(json(&graph)["root"], "demo");

        let module = s.respond("GET", "/ui/app.js");
        assert_eq!(module.status, 200);
        assert_eq!(module.content_type, JS);
        assert!(!module.body.is_empty());

        let vendored = s.respond("GET", "/ui/vendor/react.js");
        assert_eq!(vendored.status, 200);
        assert_eq!(vendored.content_type, JS);

        let grammar = s.respond("GET", "/ui/vendor/hljs/rust");
        assert_eq!(grammar.status, 200);
        assert_eq!(grammar.content_type, JS);

        let stylesheet = s.respond("GET", "/ui/vendor/xyflow-react.css");
        assert_eq!(stylesheet.status, 200);
        assert_eq!(stylesheet.content_type, CSS);

        let missing = s.respond("GET", "/ui/nope.js");
        assert_eq!(missing.status, 404);
        assert_eq!(s.respond("GET", "/ui/vendor/hljs/cobol").status, 404);

        let wrong_method = s.respond("POST", "/ui/app.js");
        assert_eq!(wrong_method.status, 405);
    }

    /// Every static string a module imports: `from "x"`, `import "x"`,
    /// `import("x")`, single- or double-quoted. The keyword must sit at a
    /// token boundary and the string must look like a specifier, because
    /// minified grammars carry `"import"` in keyword lists and xyflow's hint
    /// text says `import '@xyflow/${e}/dist/style.css'`.
    fn module_specifiers(src: &str) -> Vec<String> {
        let ident = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '.' | '"' | '\'' | '`');
        let spec_char =
            |c: char| c.is_ascii_alphanumeric() || matches!(c, '@' | '/' | '.' | '_' | '~' | '^' | '?' | '=' | '&' | ':' | '+' | '-');
        let mut out = Vec::new();
        for kw in ["from", "import"] {
            for (at, _) in src.match_indices(kw) {
                if src[..at].chars().next_back().is_some_and(ident) {
                    continue;
                }
                let mut rest = src[at + kw.len()..].trim_start();
                if kw == "import" {
                    if let Some(r) = rest.strip_prefix('(') {
                        rest = r.trim_start();
                    }
                }
                let Some(quote) = rest.chars().next().filter(|c| matches!(c, '"' | '\'')) else { continue };
                let Some(spec) = rest[1..].split(quote).next() else { continue };
                if !spec.is_empty() && spec.chars().all(spec_char) {
                    out.push(spec.to_string());
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// The vendored dependency graph is closed over localhost: the page never
    /// asks esm.sh for anything, and every import inside a vendored module is
    /// either a bare name the import map resolves or a `./` sibling in
    /// `VENDOR`. A tree of esm.sh stubs (whose inner targets are root-relative
    /// paths nothing in the import map covers) would pass a plain "every
    /// import-map target is 200" check, which is why this walks the bytes.
    #[test]
    fn vendored_modules_form_a_closure_over_the_import_map() {
        let index = include_str!("../ui/index.html");
        assert!(!index.contains("esm.sh"), "index.html still references esm.sh");

        let map_json = index
            .split_once(r#"<script type="importmap">"#)
            .and_then(|(_, rest)| rest.split_once("</script>"))
            .map(|(json, _)| json)
            .expect("index.html has an import map");
        let map: Value = serde_json::from_str(map_json).unwrap();
        let imports = map["imports"].as_object().unwrap();
        let served = |name: &str| VENDOR.iter().any(|(n, _)| *n == name);

        for (key, target) in imports {
            let target = target.as_str().unwrap();
            let name = target.strip_prefix("./ui/").unwrap_or_else(|| panic!("{key} -> {target} is not under /ui/"));
            if key.ends_with('/') {
                assert!(target.ends_with('/') && VENDOR.iter().any(|(n, _)| n.starts_with(name)), "prefix {key} -> {target} serves nothing");
            } else {
                assert!(served(name), "{key} -> {target} is not vendored");
            }
        }
        let stylesheet = index.split("href=\"").skip(1).map(|s| s.split('"').next().unwrap()).find(|h| h.ends_with(".css")).unwrap();
        assert!(served(stylesheet.strip_prefix("./ui/").unwrap()), "stylesheet {stylesheet} is not vendored");

        let bare_resolves = |spec: &str| {
            imports.iter().any(|(k, _)| k == spec || (k.ends_with('/') && spec.starts_with(k.as_str())))
        };
        // Pins the extractor to the two shapes that matter (a rewritten
        // sibling and a bare import-map name), so the loop below cannot pass
        // by finding nothing.
        let vendored = |name: &str| VENDOR.iter().find(|(n, _)| *n == name).unwrap().1;
        assert_eq!(module_specifiers(vendored("vendor/react-dom.js")), ["./react.js", "./scheduler.js"]);
        assert_eq!(module_specifiers(vendored("vendor/xyflow-react.js")), ["react", "react-dom", "react/jsx-runtime"]);
        for (name, src) in VENDOR {
            if name.ends_with(".css") {
                assert!(!src.contains("url(") && !src.contains("@import"), "{name} pulls in other files");
                continue;
            }
            let dir = &name[..name.rfind('/').unwrap()];
            for spec in module_specifiers(src) {
                if let Some(rel) = spec.strip_prefix("./") {
                    assert!(!rel.contains('/') && served(&format!("{dir}/{rel}")), "{name} imports {spec}, not in VENDOR");
                } else {
                    assert!(
                        !spec.starts_with(['/', '.']) && !spec.starts_with("http"),
                        "{name} imports {spec}, which is not local"
                    );
                    assert!(bare_resolves(&spec), "{name} imports {spec}, not in the import map");
                }
            }
        }
    }
}
