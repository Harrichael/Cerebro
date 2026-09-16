//! One adapter per indexer, each implementing both [`Dialect`] and [`Indexer`].
//! Adding a language is one file here, one `mod` line and one entry in `ALL`.
//!
//! [`Dialect`]: crate::dialect::Dialect

mod rust_analyzer;
mod scip_go;
mod scip_typescript;

use crate::indexer::Indexer;

/// Order is detection precedence. `Cargo.toml` and `go.mod` mean the code is
/// Rust or Go; `package.json` turns up in polyglot repos for tooling alone, so
/// it only wins when nothing else claims the tree.
pub static ALL: &[&dyn Indexer] = &[
    &rust_analyzer::RustAnalyzer,
    &scip_go::ScipGo,
    &scip_typescript::ScipTypescript,
];
