//! Regenerate a project's SCIP index with whichever indexer its manifest
//! implies. The committed fixtures under `tests/fixtures/` are why this
//! exists: `--scip-index` keeps its indexes in the temp dir, so a fixture's
//! index has to be produced deliberately and checked in.
//!
//! `cargo run -p scip-producer --example index -- <project-dir> [out]`,
//! where `out` defaults to `<project-dir>/index.scip`.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use scip_producer::indexer::{Progress, index_project};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let Some(dir) = args.get(1).map(PathBuf::from) else {
        bail!("usage: index <project-dir> [out]");
    };
    let out = args
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join("index.scip"));
    let indexer =
        index_project(&dir, &out, Progress::Show).with_context(|| format!("indexing {}", dir.display()))?;
    println!("wrote {} with {}", out.display(), indexer.tool());
    Ok(())
}
