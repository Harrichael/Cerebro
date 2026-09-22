//! Expandable view over an [`entity_graph::EntityGraph`].
//!
//! A [`Cursor`] is a cut through the containment hierarchy: no leaf is an
//! ancestor of another, everything above the cut is shown as a container and
//! everything below it is folded into the leaf that holds it. It answers one
//! question -- which entities are in view at this expansion, and which edges
//! run between them ([`Cursor::coalesced`]).
//!
//! A container is a node in its own right, not scaffolding. A file is what
//! `use crate::x;` points at and a struct is what `let p: Point` points at,
//! so an edge may end on one, and opening a container does not take away the
//! lines that named it.

mod cursor;
pub mod migrate;

pub use cursor::*;

#[cfg(test)]
mod tests;
