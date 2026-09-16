//! Run a SCIP indexer over a project tree. The indexer is chosen from the
//! project's manifest; the tools themselves have to be on PATH, and their
//! progress output is passed straight through to the terminal because a Rust
//! index means a build and can take a while.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::dialect::Dialect;

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
pub fn index_project(root: &Path, out: &Path) -> Result<&'static dyn Indexer> {
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
    run(indexer, root, out)?;
    Ok(indexer)
}

fn run(indexer: &dyn Indexer, root: &Path, out: &Path) -> Result<()> {
    // The indexer runs with `root` as its working directory, so a relative
    // output path would land inside the project.
    let out = std::path::absolute(out)?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let status = match indexer.command(&out).current_dir(root).status() {
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

        let err = index_project(&dir.path().join("nowhere"), &dir.path().join("out.scip"))
            .err()
            .unwrap();
        assert!(err.to_string().contains("cannot pick a SCIP indexer"), "{err}");
    }
}
