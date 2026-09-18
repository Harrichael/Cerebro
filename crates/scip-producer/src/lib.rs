//! Produce an [`EntityGraph`] from a SCIP index.
//!
//! SCIP (Sourcegraph's code-intelligence protobuf) is emitted by
//! rust-analyzer, scip-typescript, scip-go and others. This producer reads
//! one `index.scip`, turns every definition of a module/type/function-like
//! symbol into an entity under a shared Folder/File tree, and every other
//! occurrence of those symbols into a reference edge.
//!
//! Source files are read from `project_root` (never from the index's own
//! `metadata.project_root`, which is an absolute URI from the indexing
//! machine) to convert line/column positions into byte offsets.
//!
//! [`indexer`] runs the external indexer for a project so callers do not
//! have to know which tool produces the index for which language; [`index`]
//! keeps the result on disk and rebuilds it only once it has gone stale.

mod build;
mod dialect;
pub mod index;
pub mod indexer;
mod indexers;
mod source;
mod symbols;

use std::path::Path;

use anyhow::Context;
use entity_graph::EntityGraph;
use protobuf::Message;
use scip::types::Index;

pub fn graph_from_index(index: &Path, project_root: &Path) -> anyhow::Result<EntityGraph> {
    let bytes = std::fs::read(index).with_context(|| format!("reading {}", index.display()))?;
    let index = Index::parse_from_bytes(&bytes).context("parsing SCIP index")?;
    Ok(build::build(&index, project_root))
}
