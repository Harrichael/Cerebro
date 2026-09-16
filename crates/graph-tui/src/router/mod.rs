//! Orthogonal edge routing on a character grid, forked from `ratatui-flow`
//! 0.1.1 (MIT — see `LICENSE-ratatui-flow`), which forked it from `tui-nodes`.
//!
//! Only the router came across. Its placement engine could not draw a code
//! graph: it finds roots as nodes that are never a source and recurses
//! backward from sinks, so in a cyclic graph most nodes are never placed at
//! all. Placement is ours, in [`crate::placer`]; this module keeps the part
//! that is genuinely hard — pathfinding with turn costs, and choosing the box
//! glyph at every cell.
//!
//! It takes obstacles and attachment points and has no opinion about where
//! they came from, which is why nesting never enters its vocabulary: a
//! container border is one more `block_zone`, and an edge lifted to a meeting
//! level is one more `insert_port`.

// Upstream's style is kept as-is, lints included: this is a fork we will want
// to diff against ratatui-flow when it moves, and reformatting it to our taste
// would bury the changes that are actually ours.
#![allow(clippy::collapsible_if, clippy::while_let_loop, clippy::question_mark,
         clippy::double_ended_iterator_last, clippy::clone_on_copy, dead_code)]

mod connection;
mod direction;
mod id;

pub use connection::{Connection, ConnectionsLayout, Diagnostic, LineType};
pub use direction::FlowDirection;
pub use id::{NodeId, PortId};
