//! Run a SCIP indexer over a project tree. The indexer is chosen from the
//! project's manifest, and the tools themselves have to be on PATH.
//!
//! A Rust index means a build and can take a while, so by default the tool's
//! progress is passed straight through to the terminal. A caller that has
//! drawn something on that terminal asks for [`Progress::Quiet`] instead.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::dialect::Dialect;

/// Where the indexer's own output goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Straight to the terminal, which is what a command line wants.
    Show,
    /// Nowhere. For a caller with a full-screen drawing on that terminal,
    /// where the tool's progress would be painted over the top of it.
    Quiet,
}

pub trait Indexer: Dialect {
    /// Files at the project root that mark a tree as this indexer's. Data
    /// rather than a predicate so the "no indexer" error can list what it
    /// looked for. Ties between indexers go to the earlier entry in `ALL`.
    fn manifests(&self) -> &'static [&'static str];

    /// The executable, for error messages. Not always the indexer's own name:
    /// scip-typescript runs through `npx`.
    fn tool(&self) -> &'static str;

    fn install_hint(&self) -> &'static str;

    /// Runs with the project root as working directory; `out` is absolute.
    fn command(&self, out: &Path) -> Command;
}

fn detect(root: &Path) -> Option<&'static dyn Indexer> {
    crate::indexers::ALL
        .iter()
        .copied()
        .find(|i| i.manifests().iter().any(|m| root.join(m).is_file()))
}

/// Index `root` into `out`, returning which indexer ran.
pub fn index_project(root: &Path, out: &Path, progress: Progress) -> Result<&'static dyn Indexer> {
    let indexer = detect(root).with_context(|| {
        let manifests: Vec<&str> = crate::indexers::ALL
            .iter()
            .flat_map(|i| i.manifests())
            .copied()
            .collect();
        format!(
            "cannot pick a SCIP indexer: no {} in {}",
            manifests.join(", "),
            root.display()
        )
    })?;
    run(indexer, root, out, progress)?;
    Ok(indexer)
}

fn run(indexer: &dyn Indexer, root: &Path, out: &Path, progress: Progress) -> Result<()> {
    // The indexer runs with `root` as its working directory, so a relative
    // output path would land inside the project.
    let out = std::path::absolute(out)?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut command = indexer.command(&out);
    command.current_dir(root);
    if progress == Progress::Quiet {
        command.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    }
    let status = match command.status() {
        Ok(status) => status,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("{} is not on PATH ({})", indexer.tool(), indexer.install_hint())
        }
        Err(e) => return Err(e).with_context(|| format!("running {}", indexer.tool())),
    };
    if !status.success() {
        bail!("{} failed ({status}) while indexing {}", indexer.tool(), root.display());
    }
    if !out.is_file() {
        bail!("{} exited successfully but wrote no index at {}", indexer.tool(), out.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_follows_the_manifest_with_rust_and_go_taking_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let detected = |d: &Path| detect(d).map(|i| i.name());
        assert_eq!(detected(dir.path()), None);
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert_eq!(detected(dir.path()), Some("scip-typescript"));
        std::fs::write(dir.path().join("go.mod"), "module x").unwrap();
        assert_eq!(detected(dir.path()), Some("scip-go"));
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(detected(dir.path()), Some("rust-analyzer"));

        let err = index_project(&dir.path().join("nowhere"), &dir.path().join("out.scip"), Progress::Quiet)
            .err()
            .unwrap();
        assert!(err.to_string().contains("cannot pick a SCIP indexer"), "{err}");
    }
}
