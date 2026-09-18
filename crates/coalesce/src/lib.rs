//! Expandable view over an [`entity_graph::EntityGraph`].
//!
//! A [`Cursor`] holds a set of active leaves in the containment hierarchy (no
//! leaf is an ancestor of another) and keeps every reference projected onto
//! those leaves as they are expanded and collapsed. It answers one question:
//! which entities are in view at this expansion, and which edges exist between
//! them ([`Cursor::coalesced`]). Laying that out as a tree, a diagram, or
//! anything else is a consumer's concern.

mod cursor;
pub mod migrate;

pub use cursor::*;

#[cfg(test)]
mod tests;
