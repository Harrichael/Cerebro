//! The entity graph: a codebase reduced to two structures.
//!
//! 1. **Containment**: Folder → Module → File → Class → Function, stored as
//!    parent/children links on each [`Entity`]. Ids are arena indices, so a
//!    graph is immutable once built and ids are stable for its lifetime.
//! 2. **References**: directed, kinded edges between entities, independent
//!    of containment.
//!
//! This crate is the contract between *producers* (anything that builds an
//! [`EntityGraph`] from a codebase) and *consumers* (views, analysis, UIs).
//! Producers are plain functions in their own crates and must agree on:
//!
//! - one root entity, a Folder named after the project directory (or a File
//!   when a single file was loaded);
//! - an entity's `path` is its ancestor names joined by `/`, so a File's path
//!   is the root name followed by its filesystem path relative to the project
//!   directory ([`EntityGraph::file_path`] depends on this);
//! - references deduplicated on `(from, to, kind)`, self-loops dropped; every
//!   occurrence that contributed to an edge is recorded in its `sites`;
//! - `byte_range`/`line_range` are `0..0` when not applicable to the kind;
//! - `is_test` follows the conventions in [`test_code`], applied by calling
//!   [`test_code::mark`] on the finished graph.
//!
//! Consumers never learn which producer built the graph.

mod model;
pub mod goto;
pub mod search;
pub mod test_code;

pub use model::*;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
