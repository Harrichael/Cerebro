//! Throwaway localhost viewer for an `EntityGraph`. See `ui/CONTRACT.md`.

mod dto;
mod handlers;
mod reload;
mod text_index;
mod watch;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result, bail};
use clap::Parser;
use entity_graph::EntityGraph;
use tempfile::TempDir;
use tiny_http::{Header, Request, Response, Server as HttpServer};

use crate::handlers::{Mode, Server};
use crate::reload::{Loaded, Loader};

const INDEX_HTML: &str = include_str!("../ui/index.html");

#[derive(Parser)]
#[command(name = "terraform-http", about = "Serve an entity graph to the browser")]
struct Args {
    #[arg(long, default_value_t = 7878)]
    port: u16,
    /// Build the graph from a SCIP index instead of tree-sitter (needs `--features scip`).
    #[arg(long, value_name = "index.scip")]
    scip: Option<PathBuf>,
    /// Generate the SCIP index (rust-analyzer, scip-typescript or scip-go, by
    /// manifest) under the system temp dir and build the graph from it. With
    /// --diff both sides are indexed.
    #[arg(long, conflicts_with = "scip")]
    scip_index: bool,
    /// Serve the union of this git ref and the working tree, tagged by change.
    #[arg(long, value_name = "ref", conflicts_with = "scip")]
    diff: Option<String>,
    /// Start in Auto Update mode: rebuild the graph whenever sources change
    /// (the default, Static, only flags the change and waits for Reload).
    #[arg(long)]
    auto_update: bool,
    /// Project root (or single file) to load.
    path: PathBuf,
}

fn load_graph(scip: Option<&Path>, scip_index: bool, root: &Path) -> Result<EntityGraph> {
    match scip {
        Some(index) => load_scip(index, root),
        // Not a cached index path: `working_tree_index`'s freshness rule is
        // what makes a rebuild re-index after an edit.
        None if scip_index => load_scip(&working_tree_index(root)?, root),
        None => load_treesitter(root),
    }
}

#[cfg(feature = "scip")]
use scip_producer::index::{base_index, working_tree_index};

#[cfg(not(feature = "scip"))]
fn working_tree_index(_root: &Path) -> Result<PathBuf> {
    anyhow::bail!("--scip-index requires a binary built with `--features scip`")
}

#[cfg(not(feature = "scip"))]
fn base_index(_tree: &Path, _commit: &str, _root: &Path) -> Result<PathBuf> {
    anyhow::bail!("--scip-index requires a binary built with `--features scip`")
}

#[cfg(feature = "scip")]
fn load_scip(index: &std::path::Path, root: &std::path::Path) -> Result<EntityGraph> {
    scip_producer::graph_from_index(index, root)
        .with_context(|| format!("loading SCIP index {}", index.display()))
}

#[cfg(not(feature = "scip"))]
fn load_scip(_index: &std::path::Path, _root: &std::path::Path) -> Result<EntityGraph> {
    anyhow::bail!("--scip requires a binary built with `--features scip`")
}

#[cfg(feature = "treesitter")]
fn load_treesitter(root: &std::path::Path) -> Result<EntityGraph> {
    treesitter_producer::graph_from_path(root)
        .with_context(|| format!("parsing {}", root.display()))
}

#[cfg(not(feature = "treesitter"))]
fn load_treesitter(_root: &std::path::Path) -> Result<EntityGraph> {
    anyhow::bail!("this binary was built without the `treesitter` feature; pass --scip <index.scip>")
}

/// The base tree at `base_ref`, extracted under a directory named like the
/// working tree: both producers name the root after its directory, and the
/// diff only matches trees whose roots agree.
fn extract_base(repo: &Path, base_ref: &str, root_name: &Path) -> Result<(TempDir, PathBuf)> {
    let tmp = TempDir::new().context("creating a temp dir for the base tree")?;
    let dest = tmp.path().join(root_name);
    std::fs::create_dir(&dest).with_context(|| format!("creating {}", dest.display()))?;

    let mut archive = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["archive", base_ref])
        .stdout(Stdio::piped())
        .spawn()
        .context("running `git archive` (is git installed?)")?;
    let stdout = archive.stdout.take().expect("stdout was piped");
    let untar = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(&dest)
        .stdin(stdout)
        .spawn()
        .context("running `tar -x`")?
        .wait()?;
    let archived = archive.wait()?;
    if !archived.success() {
        bail!("`git archive {base_ref}` failed ({archived})");
    }
    if !untar.success() {
        bail!("unpacking the base tree failed ({untar})");
    }
    Ok((tmp, dest))
}

fn base_commit(repo: &Path, base_ref: &str) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", &format!("{base_ref}^{{commit}}")])
        .output()
        .context("running `git rev-parse` (is git installed?)")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("{base_ref} is not a commit in {}: {}", repo.display(), stderr.trim());
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

/// The base side is fixed for the process: extracted and indexed here, once,
/// and owned by the loader. Each call re-indexes only the working tree and
/// re-diffs it against that base.
fn diff_loader(base_ref: &str, root: PathBuf, scip_index: bool) -> Result<Loader> {
    if !root.is_dir() {
        bail!("--diff needs a project directory; {} is a file", root.display());
    }
    let name = root.file_name().map(Path::new).context("the project root has no name")?;
    let commit = base_commit(&root, base_ref)?;
    let (tree, old_root) = extract_base(&root, base_ref, name)?;
    let old = if scip_index {
        load_scip(&base_index(&old_root, &commit, &root)?, &old_root)?
    } else {
        load_treesitter(&old_root)?
    };
    println!("Diffing {base_ref} ({commit}) → working tree: base {} entities", old.entities.len());
    let base_label = base_ref.to_string();
    Ok(Box::new(move || {
        // The graph's paths name the extracted tree and `/source` reads
        // removed files from it, so `tree` must outlive every generation.
        let old_root = tree.path().join(root.file_name().expect("checked above"));
        let new = if scip_index {
            load_scip(&working_tree_index(&root)?, &root)?
        } else {
            load_treesitter(&root)?
        };
        let diff = graph_diff::diff(&old, &old_root, &new, &root)?;
        Ok(Loaded::Diff { diff, base_label: base_label.clone(), base_commit: commit.clone() })
    }))
}

fn make_loader(args: &Args, root: PathBuf) -> Result<Loader> {
    match &args.diff {
        Some(base_ref) => diff_loader(base_ref, root, args.scip_index),
        None => {
            let (scip, scip_index) = (args.scip.clone(), args.scip_index);
            Ok(Box::new(move || load_graph(scip.as_deref(), scip_index, &root).map(Loaded::Graph)))
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let root = args
        .path
        .canonicalize()
        .with_context(|| format!("resolving {}", args.path.display()))?;
    let mode = if args.auto_update { Mode::Auto } else { Mode::Static };
    let loader = make_loader(&args, root.clone())?;
    let server = Arc::new(Server::new(loader, root.clone(), INDEX_HTML, mode)?);
    if let Err(e) = watch::spawn(root.clone(), server.clone()) {
        eprintln!(
            "warning: not watching {} for changes ({e:#}); the graph will stay as loaded",
            root.display()
        );
    }

    let addr = format!("127.0.0.1:{}", args.port);
    let http = HttpServer::http(&addr).map_err(|e| anyhow::anyhow!("binding {addr}: {e}"))?;
    let snapshot = server.snapshot();
    println!(
        "Serving {} ({} entities, {} references) at http://{addr}/",
        dto::root_name(&snapshot.graph),
        snapshot.graph.entities.len(),
        snapshot.graph.references.len(),
    );
    drop(snapshot);

    // A small pool so a slow /search or a large /source cannot head-of-line
    // block the status poll; the main thread serves too rather than idling.
    let http = Arc::new(http);
    for i in 0..WORKERS {
        let (http, server) = (http.clone(), server.clone());
        thread::Builder::new()
            .name(format!("http-worker-{i}"))
            .spawn(move || serve_forever(&http, |method, url| server.respond(method, url)))
            .with_context(|| format!("spawning http-worker-{i}"))?;
    }
    serve_forever(&http, |method, url| server.respond(method, url));
    Ok(())
}

const WORKERS: usize = 4;

/// `recv` fails for exactly two reasons, the accept thread died or `unblock`
/// told one waiter to stop, and neither is transient: retrying would park the
/// worker forever, so an `Err` ends the loop.
fn serve_forever(http: &HttpServer, respond: impl Fn(&str, &str) -> handlers::Response) {
    while let Ok(request) = http.recv() {
        serve(request, &respond);
    }
}

fn serve(request: Request, respond: &impl Fn(&str, &str) -> handlers::Response) {
    let method = request.method().as_str().to_string();
    let url = request.url().to_string();
    // Without this a panicking request would silently kill its worker, and
    // one that panicked while holding a handler mutex would leave it poisoned
    // for every later request. The panic hook has already printed the
    // message and location; this names the request it belonged to.
    let resp = match catch_unwind(AssertUnwindSafe(|| respond(&method, &url))) {
        Ok(resp) => resp,
        Err(_) => {
            eprintln!("panic while handling {method} {url}; answering 500");
            handlers::error(500, "internal server error")
        }
    };
    let header = Header::from_bytes("Content-Type", resp.content_type)
        .expect("static content-type strings are valid header values");
    let http_resp = Response::from_data(resp.body).with_status_code(resp.status).with_header(header);
    if let Err(e) = request.respond(http_resp) {
        eprintln!("failed to write response for {method} {url}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    /// A panicking request costs exactly that request: it gets a 500, the
    /// worker that caught it serves the next one, and an `Err` from `recv`
    /// ends the loop instead of being retried.
    #[test]
    fn a_panicking_request_gets_a_500_and_the_worker_lives_on() {
        let http = Arc::new(HttpServer::http("127.0.0.1:0").unwrap());
        let port = http.server_addr().to_ip().unwrap().port();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = {
            let http = http.clone();
            thread::spawn(move || {
                serve_forever(&http, |_, url| {
                    if url == "/boom" {
                        panic!("handler exploded on purpose");
                    }
                    handlers::Response { status: 200, content_type: "text/plain", body: b"still here".to_vec() }
                });
                done_tx.send(()).unwrap();
            })
        };

        let boom = get(port, "/boom");
        assert!(boom.starts_with("HTTP/1.1 500 "), "{boom}");
        assert!(boom.ends_with(r#"{"error":"internal server error"}"#), "{boom}");
        let after = get(port, "/after");
        assert!(after.starts_with("HTTP/1.1 200 "), "{after}");
        assert!(after.ends_with("still here"), "{after}");

        http.unblock();
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("serve_forever must return once recv fails");
        worker.join().unwrap();
    }

    fn get(port: u16, path: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    /// A SCIP index is one build of one tree, so there is no base side to
    /// diff against; clap rejects the pair rather than the loader.
    #[test]
    fn diff_and_scip_cannot_be_combined() {
        assert!(Args::try_parse_from(["graph-server", "--diff", "HEAD", "."]).is_ok());
        assert!(Args::try_parse_from(["graph-server", "--scip", "index.scip", "."]).is_ok());
        let both =
            Args::try_parse_from(["graph-server", "--diff", "HEAD", "--scip", "index.scip", "."]);
        assert!(both.is_err(), "--diff with --scip must be rejected");
    }

    /// `--scip-index` is the generated counterpart of `--scip`, so the two
    /// are exclusive; unlike `--scip` it does combine with `--diff`.
    #[test]
    fn scip_index_replaces_scip_and_allows_diff() {
        assert!(Args::try_parse_from(["graph-server", "--scip-index", "."]).is_ok());
        assert!(Args::try_parse_from(["graph-server", "--diff", "HEAD", "--scip-index", "."]).is_ok());
        assert!(Args::try_parse_from(["graph-server", "--scip-index", "--scip", "i.scip", "."]).is_err());
    }
}
