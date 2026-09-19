//! What part of the diagram is on screen, and how a screen cell maps back.
//!
//! The diagram is drawn once at its own size; this decides which window of it
//! the viewport shows. Keeping that here rather than in the event loop is what
//! lets a mouse click, a scroll wheel and a selection step all agree on where
//! the diagram is: they are all the same arithmetic.
//!
//! [`Camera::offset`] is signed on purpose. A diagram smaller than the
//! terminal has to sit in the middle of it rather than jammed into the corner,
//! and "centred" is exactly "the viewport starts at a negative diagram
//! coordinate". Treating the offset as unsigned forces centring to live in the
//! renderer instead, where it would have to be re-derived for every hit test.

use ratatui::layout::Rect;

/// Cells of the diagram that must stay on screen. Panning is free until the
/// last of the picture would leave, because a user who has scrolled into empty
/// space has no landmark to scroll back by.
const KEEP: u16 = 6;
/// Breathing room left around a node scrolled into view, so a selection never
/// lands flush against an edge of the screen.
const REVEAL_PAD: u16 = 2;

#[derive(Debug, Clone, Copy)]
pub struct Camera {
    /// Where on screen the diagram is drawn.
    pub viewport: Rect,
    /// The diagram's own size.
    pub extent: (u16, u16),
    /// The diagram coordinate shown at the viewport's top-left. Negative means
    /// the diagram is inset from that edge.
    pub offset: (i32, i32),
}

impl Camera {
    pub fn new(viewport: Rect, extent: (u16, u16)) -> Self {
        let mut c = Camera { viewport, extent, offset: (0, 0) };
        c.center();
        c
    }

    /// Put the diagram in the middle of the viewport on whichever axis it
    /// fits. On an axis it overflows, centring would open the view halfway
    /// down a picture whose top is where reading starts, so that axis goes to
    /// the beginning instead.
    pub fn center(&mut self) {
        self.offset = (
            centred(self.extent.0, self.viewport.width),
            centred(self.extent.1, self.viewport.height),
        );
        self.clamp();
    }

    /// Adopt the *same* picture at a different size, holding one point of it
    /// still under one cell of the screen.
    ///
    /// This is what zooming needs and [`Camera::fit`] cannot give it. `fit`
    /// opens a differently-sized diagram centred, on the reasoning that a
    /// different size means a different picture -- which is true of expanding
    /// a node and false of zooming, the one case where the size changes and
    /// the picture does not. Going through `fit` sends a reader at the bottom
    /// of a long diagram back to the top on every press.
    pub fn rescale(&mut self, extent: (u16, u16), screen: (u16, u16), point: (u16, u16)) {
        let scale = |v: u16, from: u16, to: u16| match from {
            0 => 0,
            from => (i64::from(v) * i64::from(to) / i64::from(from)) as i32,
        };
        let (px, py) = (scale(point.0, self.extent.0, extent.0), scale(point.1, self.extent.1, extent.1));
        self.extent = extent;
        self.offset = (
            px - i32::from(screen.0.saturating_sub(self.viewport.x)),
            py - i32::from(screen.1.saturating_sub(self.viewport.y)),
        );
        self.clamp();
    }

    /// Adopt a freshly laid-out diagram. A picture of a different size is a
    /// different picture and opens centred; one that came back the same size
    /// keeps the user where they were, so turning a switch on and off does
    /// not throw away a scroll.
    pub fn fit(&mut self, extent: (u16, u16)) {
        if self.extent == extent {
            self.clamp();
            return;
        }
        self.extent = extent;
        self.center();
    }

    pub fn resize(&mut self, viewport: Rect) {
        self.viewport = viewport;
        // An axis the diagram fits on has one sensible position and no other,
        // so a window that grew re-centres rather than leaving the picture
        // hanging off to one side.
        if self.extent.0 <= viewport.width {
            self.offset.0 = centred(self.extent.0, viewport.width);
        }
        if self.extent.1 <= viewport.height {
            self.offset.1 = centred(self.extent.1, viewport.height);
        }
        self.clamp();
    }

    /// The last screenful, with no overscroll. [`KEEP`] is a budget for
    /// panning past the end by hand; End means the end of the picture, not
    /// the end of how far you are allowed to push it.
    pub fn bottom(&mut self) {
        self.offset.1 = if self.extent.1 > self.viewport.height {
            i32::from(self.extent.1) - i32::from(self.viewport.height)
        } else {
            centred(self.extent.1, self.viewport.height)
        };
    }

    /// How far down the diagram the view is, and how far down it could go --
    /// the numbers the scroll position is worth reporting as.
    pub fn row(&self) -> (i32, i32) {
        (self.offset.1, i32::from(self.extent.1) - i32::from(self.viewport.height))
    }

    pub fn scroll(&mut self, dx: i32, dy: i32) {
        self.offset = (self.offset.0 + dx, self.offset.1 + dy);
        self.clamp();
    }

    /// Stop panning once only [`KEEP`] cells of the diagram would be left on
    /// screen, on the axis being panned. Without a floor the picture can be
    /// scrolled clean out of the terminal, and what is left behind -- blank
    /// cells -- says nothing about which way to scroll back.
    pub fn clamp(&mut self) {
        self.offset = (
            clamp_axis(self.offset.0, self.extent.0, self.viewport.width),
            clamp_axis(self.offset.1, self.extent.1, self.viewport.height),
        );
    }

    /// The diagram cell under a screen cell, or `None` outside the viewport or
    /// off the diagram.
    pub fn at(&self, col: u16, row: u16) -> Option<(u16, u16)> {
        if !self.viewport.contains((col, row).into()) {
            return None;
        }
        let x = i32::from(col - self.viewport.x) + self.offset.0;
        let y = i32::from(row - self.viewport.y) + self.offset.1;
        (x >= 0 && y >= 0 && x < i32::from(self.extent.0) && y < i32::from(self.extent.1))
            .then_some((x as u16, y as u16))
    }

    /// The diagram coordinate under a screen cell, wherever it is -- off the
    /// diagram, off the viewport. A sweep that starts on the margin and ends
    /// on the picture still has to mean a rectangle.
    /// The screen cell a diagram point is drawn at, which may be off screen.
    pub fn screen_of(&self, point: (u16, u16)) -> (i32, i32) {
        (
            i32::from(point.0) - self.offset.0 + i32::from(self.viewport.x),
            i32::from(point.1) - self.offset.1 + i32::from(self.viewport.y),
        )
    }

    pub fn unclamped_at(&self, col: u16, row: u16) -> (i32, i32) {
        (
            i32::from(col) - i32::from(self.viewport.x) + self.offset.0,
            i32::from(row) - i32::from(self.viewport.y) + self.offset.1,
        )
    }

    /// Scroll the smallest amount that brings `r` fully on screen.
    pub fn reveal(&mut self, r: Rect) {
        self.offset.0 = reveal_axis(self.offset.0, r.x, r.right(), self.viewport.width);
        self.offset.1 = reveal_axis(self.offset.1, r.y, r.bottom(), self.viewport.height);
        self.clamp();
    }
}

fn reveal_axis(offset: i32, near: u16, far: u16, viewport: u16) -> i32 {
    let (near, far, viewport) = (i32::from(near), i32::from(far), i32::from(viewport));
    let pad = i32::from(REVEAL_PAD);
    if near - pad < offset {
        near - pad
    } else if far + pad > offset + viewport {
        far + pad - viewport
    } else {
        offset
    }
}

fn centred(extent: u16, viewport: u16) -> i32 {
    if extent <= viewport { (i32::from(extent) - i32::from(viewport)) / 2 } else { 0 }
}

fn clamp_axis(offset: i32, extent: u16, viewport: u16) -> i32 {
    let keep = i32::from(KEEP.min(extent).min(viewport));
    let (extent, viewport) = (i32::from(extent), i32::from(viewport));
    offset.clamp(keep - viewport, extent - keep)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam(extent: (u16, u16)) -> Camera {
        Camera::new(Rect::new(0, 0, 80, 24), extent)
    }

    /// A diagram smaller than the terminal sits in the middle of it, and the
    /// cell in the middle of the screen is the cell in the middle of it.
    #[test]
    fn a_small_diagram_opens_centred() {
        let c = cam((40, 10));
        assert_eq!(c.offset, (-20, -7));
        assert_eq!(c.at(40, 12), Some((20, 5)), "the screen's middle is the diagram's middle");
        assert_eq!(c.at(5, 2), None, "the margin around it belongs to no cell");
    }

    /// Height is the axis a zoom level grows along, so a tall diagram opens at
    /// its top -- centring it would start the view halfway down.
    #[test]
    fn a_tall_diagram_opens_at_its_top_and_still_centres_sideways() {
        let c = cam((40, 400));
        assert_eq!(c.offset, (-20, 0));
    }

    /// Home and End are jumps to the ends of the picture. They must not stop
    /// at the overscroll limit, which is six rows of diagram and a screenful
    /// of nothing.
    #[test]
    fn the_ends_of_the_diagram_are_a_full_screen_of_diagram() {
        let mut c = cam((200, 400));
        c.bottom();
        assert_eq!(c.offset.1, 376, "End left the last screenful mostly empty");
        c.bottom();
        assert_eq!(c.offset.1, 376, "End is a place, not a step");
        c.center();
        assert_eq!(c.offset.1, 0, "Home left the first screenful mostly empty");

        // A diagram that fits has no end to jump to; both keys centre it.
        let mut small = cam((40, 10));
        small.bottom();
        assert_eq!(small.offset, (-20, -7));
    }

    /// Zooming is the one relayout where the picture is the same picture, so
    /// what you were reading has to stay where you were reading it.
    #[test]
    fn rescaling_holds_a_point_of_the_diagram_under_the_same_cell() {
        let mut c = cam((200, 400));
        c.scroll(0, 300);
        let screen = (40, 12);
        let point = c.at(screen.0, screen.1).expect("pointing at the diagram");
        assert!(point.1 > 300, "the test is about being a long way down");

        // Half the height, as a coarser zoom level would give.
        c.rescale((200, 200), screen, point);
        let now = c.at(screen.0, screen.1).expect("still on the diagram");
        assert!(
            now.1.abs_diff(point.1 / 2) <= 1,
            "the cell under the pointer moved: {now:?} against {point:?} halved"
        );
    }

    #[test]
    fn a_window_that_grows_re_centres_a_diagram_that_now_fits() {
        let mut c = cam((40, 400));
        c.scroll(0, 100);
        c.resize(Rect::new(0, 0, 120, 24));
        assert_eq!(c.offset.0, -40, "the diagram stayed off to one side");
        assert_eq!(c.offset.1, 100, "the axis it still overflows should hold its place");
    }

    #[test]
    fn panning_stops_before_the_last_of_the_picture_leaves() {
        let mut c = cam((200, 400));
        c.scroll(10_000, 10_000);
        assert_eq!(c.at(79, 23), None, "scrolled clean past the end");
        assert!(c.at(0, 0).is_some(), "some of the diagram has to stay on screen");

        c.scroll(-10_000, -10_000);
        assert!(c.at(79, 23).is_some(), "some of the diagram has to stay on screen");
        assert_eq!(c.at(0, 0), None);
    }

    /// Toggling something that does not change the layout -- taking the edge
    /// lines away, say -- must not also throw away where the user had
    /// scrolled to.
    #[test]
    fn relaying_out_the_same_size_picture_keeps_your_place() {
        let mut c = cam((200, 400));
        c.scroll(20, 120);
        let held = c.offset;
        c.fit((200, 400));
        assert_eq!(c.offset, held);

        c.fit((40, 10));
        assert_eq!(c.offset, (-20, -7), "a different picture opens centred");
    }

    #[test]
    fn revealing_a_node_scrolls_the_least_it_can() {
        let mut c = cam((200, 400));
        let before = c.offset;
        c.reveal(Rect::new(10, 5, 12, 3));
        assert_eq!(c.offset, before, "a node already on screen should not move the view");

        c.reveal(Rect::new(10, 380, 12, 3));
        assert!(c.at(0, 0).is_some());
        let (_, y) = c.at(11, 23).expect("the revealed row is on screen");
        assert!(y >= 380, "the node was not brought into view");
    }
}
