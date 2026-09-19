//! How big the drawing is, as distinct from how much of the graph is in it.
//!
//! Two different things were both called zoom. Expanding a node swaps it for
//! its children: the graph in view changes, and no amount of it can be undone
//! by looking harder. Zooming changes nothing about the graph -- the same
//! nodes and the same edges are drawn smaller, so more of them fit on screen.
//! [`crate::view`] owns the first; this owns the second.
//!
//! A terminal cannot shrink a glyph, so zooming out here spends fewer *cells*
//! per node instead: the detail line goes, then the border, then the gaps and
//! the room a label is allowed.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zoom {
    Close,
    Mid,
    Far,
}

/// The cell budget one zoom level gives a node.
#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    /// Between siblings, and between ranks -- the rows where the horizontal
    /// runs of an edge travel.
    pub gap_x: u16,
    pub gap_y: u16,
    pub leaf_h: u16,
    /// Leaves are drawn as bordered boxes. Without a border a leaf is two
    /// rows: its name and a rule under it. It cannot be one, because the
    /// router only blocks a zone's *interior* boundaries, and a rect one cell
    /// tall has none to block -- edges would be routed straight through the
    /// name.
    pub bordered: bool,
    /// Leaves carry the line saying what kind of thing they are.
    pub detail: bool,
    /// Boxes carry their size beside their name.
    pub box_size: bool,
    /// Room a label is allowed before it is cut.
    pub label_max: usize,
    /// However deep the nesting, a box keeps room for one clamped leaf.
    pub min_inner_w: u16,
}

impl Zoom {
    pub fn metrics(self) -> Metrics {
        match self {
            Zoom::Close => Metrics {
                gap_x: 4,
                gap_y: 6,
                leaf_h: 4,
                bordered: true,
                detail: true,
                box_size: true,
                label_max: 32,
                min_inner_w: 34,
            },
            Zoom::Mid => Metrics {
                gap_x: 3,
                gap_y: 4,
                leaf_h: 3,
                bordered: true,
                detail: false,
                box_size: true,
                label_max: 24,
                min_inner_w: 26,
            },
            Zoom::Far => Metrics {
                gap_x: 2,
                gap_y: 3,
                leaf_h: 2,
                bordered: false,
                detail: false,
                box_size: false,
                label_max: 16,
                min_inner_w: 18,
            },
        }
    }

    /// How large this draws next to the unit the virtual layout is kept in.
    /// A position that persists across zooms is scaled by this on the way to
    /// the screen, so the same arrangement is the same arrangement drawn
    /// smaller rather than a fresh one.
    pub fn scale(self) -> f32 {
        f32::from(self.metrics().leaf_h) / f32::from(Zoom::Close.metrics().leaf_h)
    }

    pub fn name(self) -> &'static str {
        match self {
            Zoom::Close => "close",
            Zoom::Mid => "mid",
            Zoom::Far => "far",
        }
    }

    /// `None` at the end of the range, so a caller can leave the view alone
    /// rather than redrawing an identical diagram.
    pub fn in_(self) -> Option<Zoom> {
        match self {
            Zoom::Close => None,
            Zoom::Mid => Some(Zoom::Close),
            Zoom::Far => Some(Zoom::Mid),
        }
    }

    pub fn out(self) -> Option<Zoom> {
        match self {
            Zoom::Close => Some(Zoom::Mid),
            Zoom::Mid => Some(Zoom::Far),
            Zoom::Far => None,
        }
    }

    /// The next level round, for a control that has one key and three states.
    pub fn cycle(self) -> Zoom {
        self.out().unwrap_or(Zoom::Close)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Zooming out has to actually buy space on every axis a node spends it,
    /// or it is a relabelling rather than a zoom.
    #[test]
    fn every_step_out_costs_a_node_fewer_cells() {
        let levels = [Zoom::Close, Zoom::Mid, Zoom::Far];
        for pair in levels.windows(2) {
            let (near, far) = (pair[0].metrics(), pair[1].metrics());
            assert!(far.leaf_h < near.leaf_h, "{:?} is no shorter than {:?}", pair[1], pair[0]);
            assert!(far.gap_x < near.gap_x && far.gap_y < near.gap_y);
            assert!(far.label_max < near.label_max);
        }
        assert!(Zoom::Far.metrics().leaf_h >= 2, "a leaf the router can route around");
    }

    #[test]
    fn the_ends_of_the_range_stay_put_and_the_cycle_comes_round() {
        assert_eq!(Zoom::Close.in_(), None);
        assert_eq!(Zoom::Far.out(), None);
        assert_eq!(Zoom::Far.cycle(), Zoom::Close);
        assert_eq!(Zoom::Close.cycle(), Zoom::Mid);
    }
}
