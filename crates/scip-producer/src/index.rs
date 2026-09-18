//! Keep a generated SCIP index on disk for a project, so callers can ask for
//! one by path and not care whether it had to be built.
//!
//! Indexes live under the system temp dir, one folder per project, so
//! indexing never writes into the tree it is indexing. A working-tree index
//! is reused while it is newer than every source file; a base-tree index is
//! keyed by commit and therefore never goes stale.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result};

pub use crate::indexer::Progress;

fn project_dir(root: &Path) -> PathBuf {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut h);
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    std::env::temp_dir().join("scip-index").join(format!("{name}-{:016x}", h.finish()))
}

/// Newest modification time under `root`, ignoring what indexers and package
/// managers write there themselves.
fn newest_mtime(root: &Path) -> Option<SystemTime> {
    let mut newest = None;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if let Ok(m) = meta.modified() {
                newest = Some(newest.map_or(m, |n: SystemTime| n.max(m)));
            }
        }
    }
    newest
}

fn is_fresh(index: &Path, root: &Path) -> bool {
    let Ok(indexed) = std::fs::metadata(index).and_then(|m| m.modified()) else { return false };
    newest_mtime(root).is_none_or(|newest| newest <= indexed)
}

fn generate(root: &Path, out: &Path, progress: Progress) -> Result<()> {
    let started = Instant::now();
    let lang = crate::indexer::index_project(root, out, progress)
        .with_context(|| format!("indexing {}", root.display()))?;
    if progress == Progress::Show {
        println!("Indexed {} with {} in {:.1?} → {}", root.display(), lang.tool(), started.elapsed(), out.display());
    }
    Ok(())
}

pub fn working_tree_index(root: &Path, progress: Progress) -> Result<PathBuf> {
    let out = project_dir(root).join("index.scip");
    if !is_fresh(&out, root) {
        generate(root, &out, progress)?;
    } else if progress == Progress::Show {
        println!("Reusing SCIP index {} (newer than every source file)", out.display());
    }
    Ok(out)
}

/// `tree` is the extracted base checkout; the index is filed under the
/// working tree's project dir so it is found again on the next diff.
pub fn base_index(
    tree: &Path,
    commit: &str,
    working_root: &Path,
    progress: Progress,
) -> Result<PathBuf> {
    let out = project_dir(working_root).join(format!("base-{commit}.scip"));
    if !out.is_file() {
        generate(tree, &out, progress)?;
    } else if progress == Progress::Show {
        println!("Reusing SCIP index {} for {commit}", out.display());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The freshness rule: an index older than a source file is stale, one
    /// newer than every source file is fresh, and edits under `target/` or
    /// dotfiles do not count as source changes.
    #[test]
    fn index_is_fresh_only_while_newer_than_the_sources() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        let index = dir.path().join("index.scip");
        let set_time = |p: &Path, secs_ago: u64| {
            let t = SystemTime::now() - Duration::from_secs(secs_ago);
            std::fs::File::options().write(true).create(true).truncate(false).open(p).unwrap().set_modified(t).unwrap();
        };
        set_time(&root.join("src/lib.rs"), 100);
        assert!(!is_fresh(&index, &root), "missing index is stale");

        set_time(&index, 50);
        assert!(is_fresh(&index, &root));

        set_time(&root.join("src/lib.rs"), 10);
        assert!(!is_fresh(&index, &root), "edited source makes the index stale");

        set_time(&root.join("src/lib.rs"), 100);
        set_time(&root.join("target/debug.log"), 0);
        set_time(&root.join(".hidden"), 0);
        assert!(is_fresh(&index, &root), "build output and dotfiles are not sources");

        assert!(project_dir(&root).starts_with(std::env::temp_dir().join("scip-index")));
        assert_ne!(project_dir(&root), project_dir(&dir.path().join("other")));
    }
}
