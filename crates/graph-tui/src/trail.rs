//! Where the focus has been, so that going back retraces going forth.
//!
//! Stepping off a node leaves a note on the node landed on: the opposite key
//! leads back here. Tab leaves the same note on a box, which without one
//! always drops the focus on the first child in reading order rather than the
//! one the focus last stepped out of.
//!
//! The notes are hints, not instructions. This module knows ids and keys and
//! nothing about where anything is drawn, so it cannot tell whether the node
//! a note names is still somewhere that key could reach. The caller checks
//! that against the picture it has now and ignores a note that no longer
//! fits, which is how a remembered neighbour stops mattering once the layout
//! has moved out from under it.

use std::collections::BTreeMap;

use entity_graph::EntityId;

/// Which of the caller's two passes a step belonged to. Kept apart because
/// the passes run in turn rather than together: a note left by a step of one
/// kind must not answer for the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Near,
    Far,
}

#[derive(Debug, Default)]
pub struct Trail {
    beside: BTreeMap<(EntityId, (i32, i32), Tier), EntityId>,
    inside: BTreeMap<Option<EntityId>, EntityId>,
}

impl Trail {
    /// The focus went from `from` to `to` by pressing `dir`. Inverting the
    /// direction is this module's whole share of the idea, so it happens in
    /// the one place that writes the note rather than at every call site.
    pub fn stepped(&mut self, from: EntityId, dir: (i32, i32), tier: Tier, to: EntityId) {
        self.beside.insert((to, (-dir.0, -dir.1), tier), from);
    }

    pub fn back(&self, at: EntityId, dir: (i32, i32), tier: Tier) -> Option<EntityId> {
        self.beside.get(&(at, dir, tier)).copied()
    }

    /// The focus left `from` for the level around it.
    pub fn rose(&mut self, from: EntityId, to: Option<EntityId>) {
        self.inside.insert(to, from);
    }

    pub fn inward(&self, level: Option<EntityId>) -> Option<EntityId> {
        self.inside.get(&level).copied()
    }

    /// Carry the trail across a rebuild that renumbered every entity.
    ///
    /// Every note names two entities, and a note whose *other* end died is
    /// the dangerous one: it would survive renumbering as a live id pointing
    /// at whichever entity inherited the number, and no amount of checking it
    /// against the picture would catch that. So either end dying drops it.
    pub fn migrate(&mut self, map: &[Option<EntityId>]) {
        let moved = |id: EntityId| map.get(id.0).copied().flatten();
        self.beside = std::mem::take(&mut self.beside)
            .into_iter()
            .filter_map(|((at, dir, tier), from)| Some(((moved(at)?, dir, tier), moved(from)?)))
            .collect();
        self.inside = std::mem::take(&mut self.inside)
            .into_iter()
            .filter_map(|(level, from)| {
                let level = match level {
                    None => None,
                    Some(id) => Some(moved(id)?),
                };
                Some((level, moved(from)?))
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UP: (i32, i32) = (0, -1);
    const DOWN: (i32, i32) = (0, 1);
    const LEFT: (i32, i32) = (-1, 0);

    /// A step is filed against the key that would undo it, and against
    /// nothing else: not the key that made it, not another direction, and not
    /// the other tier -- the caller searches the tiers in turn, so a note
    /// answering across them would hand back a node that pass never offered.
    #[test]
    fn the_way_back_is_the_key_you_did_not_press() {
        let (a, b) = (EntityId(1), EntityId(2));
        let mut trail = Trail::default();
        trail.stepped(a, DOWN, Tier::Near, b);

        assert_eq!(trail.back(b, UP, Tier::Near), Some(a));
        assert_eq!(trail.back(b, DOWN, Tier::Near), None, "the way back is not the way there");
        assert_eq!(trail.back(b, LEFT, Tier::Near), None);
        assert_eq!(trail.back(b, UP, Tier::Far), None, "one tier answered for the other");
        assert_eq!(trail.back(a, UP, Tier::Near), None, "the note went on the wrong node");

        // Each tier keeps its own last-comer rather than overwriting.
        let c = EntityId(3);
        trail.stepped(c, DOWN, Tier::Far, b);
        assert_eq!(trail.back(b, UP, Tier::Near), Some(a));
        assert_eq!(trail.back(b, UP, Tier::Far), Some(c));
    }

    /// Tab goes back to the child the focus left, per box, and the roots are
    /// a level like any other.
    #[test]
    fn a_box_remembers_the_child_the_focus_rose_out_of() {
        let (parent, child) = (EntityId(1), EntityId(2));
        let mut trail = Trail::default();
        assert_eq!(trail.inward(Some(parent)), None, "a box never left is not remembered");

        trail.rose(child, Some(parent));
        assert_eq!(trail.inward(Some(parent)), Some(child));
        assert_eq!(trail.inward(None), None, "the child was filed against the wrong level");
    }

    /// Renumbering follows both ends of a note. An entity that did not
    /// survive takes every note touching it with it, because a note half
    /// translated points at whatever inherited the number.
    #[test]
    fn a_renumbered_graph_carries_the_trail_or_drops_it() {
        let (a, b, gone) = (EntityId(1), EntityId(2), EntityId(3));
        let mut trail = Trail::default();
        trail.stepped(a, DOWN, Tier::Near, b);
        trail.stepped(gone, DOWN, Tier::Near, a);
        trail.stepped(b, DOWN, Tier::Near, gone);
        trail.rose(b, Some(a));
        trail.rose(a, Some(gone));

        // Everything shifts up one; the third entity is not in the new graph.
        let map = [Some(EntityId(0)), Some(EntityId(11)), Some(EntityId(12)), None];
        trail.migrate(&map);
        let (a, b) = (EntityId(11), EntityId(12));

        assert_eq!(trail.back(b, UP, Tier::Near), Some(a), "a surviving note lost its way");
        assert_eq!(trail.back(a, UP, Tier::Near), None, "a note from a dead entity survived");
        assert_eq!(trail.inward(Some(a)), Some(b));
        assert_eq!(trail.inward(Some(gone)), None, "a note on a dead box survived");
    }
}
