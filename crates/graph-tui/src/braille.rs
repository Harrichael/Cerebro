//! Fine lines on a cell grid: a canvas of braille dots (2 wide x 4 tall per
//! terminal cell) that polylines are drawn onto, one colour per cell, and that
//! is then written into a `Buffer` only where the caller permits.
//!
//! Cell coordinates are `f32`; cell `(x, y)` spans `[x, x+1) x [y, y+1)`.

use ratatui::buffer::Buffer;
use ratatui::style::Color;

const BITS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];

pub fn dot_of(cell: (f32, f32)) -> (i32, i32) {
    ((cell.0 * 2.0).floor() as i32, (cell.1 * 4.0).floor() as i32)
}

pub struct Canvas {
    width: u16,
    height: u16,
    dots: Vec<u8>,
    colour: Vec<Option<Color>>,
}

impl Canvas {
    pub fn new(width: u16, height: u16) -> Self {
        let cells = width as usize * height as usize;
        Self {
            width,
            height,
            dots: vec![0; cells],
            colour: vec![None; cells],
        }
    }

    pub fn clear(&mut self) {
        self.dots.fill(0);
        self.colour.fill(None);
    }

    /// Straight segment between two points in cell coordinates. Every dot along
    /// the way is set and the cell takes `colour` (last writer wins, so callers
    /// draw the lines they care about most last). Off-canvas parts are clipped.
    pub fn line(&mut self, from: (f32, f32), to: (f32, f32), colour: Color) {
        if self.width == 0 || self.height == 0 {
            return;
        }
        let dot_w = f32::from(self.width) * 2.0;
        let dot_h = f32::from(self.height) * 4.0;
        let a = (from.0 * 2.0, from.1 * 4.0);
        let b = (to.0 * 2.0, to.1 * 4.0);
        let Some((a, b)) = clip(a, b, dot_w, dot_h) else {
            return;
        };

        // Bresenham over all octants in dot space; the dot grid is anisotropic
        // (a cell-space 45° is a 1:2 slope here), so slope handling is general.
        let (mut x0, mut y0) = (a.0.floor() as i32, a.1.floor() as i32);
        let (x1, y1) = (b.0.floor() as i32, b.1.floor() as i32);
        let dx = (x1 - x0).abs();
        let dy = (y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx - dy;
        loop {
            self.plot(x0, y0, colour);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 > -dy {
                err -= dy;
                x0 += sx;
            }
            if e2 < dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    pub fn polyline(&mut self, points: &[(f32, f32)], colour: Color) {
        for pair in points.windows(2) {
            self.line(pair[0], pair[1], colour);
        }
    }

    pub fn glyph(&self, x: u16, y: u16) -> Option<(char, Color)> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let idx = y as usize * self.width as usize + x as usize;
        let bits = self.dots[idx];
        if bits == 0 {
            return None;
        }
        let ch = char::from_u32(0x2800 + u32::from(bits))?;
        Some((ch, self.colour[idx].unwrap_or(Color::Reset)))
    }

    /// Only the symbol and foreground are written: containers are tinted with a
    /// background that must show through the lines drawn over them.
    pub fn composite(&self, buf: &mut Buffer, allow: impl Fn(u16, u16) -> bool) {
        for y in 0..self.height {
            for x in 0..self.width {
                let Some((ch, colour)) = self.glyph(x, y) else {
                    continue;
                };
                if !allow(x, y) {
                    continue;
                }
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char(ch).set_fg(colour);
                }
            }
        }
    }

    fn plot(&mut self, dx: i32, dy: i32, colour: Color) {
        if dx < 0 || dy < 0 {
            return;
        }
        let (cx, cy) = ((dx / 2) as usize, (dy / 4) as usize);
        if cx >= self.width as usize || cy >= self.height as usize {
            return;
        }
        let idx = cy * self.width as usize + cx;
        self.dots[idx] |= BITS[(dx & 1) as usize][(dy & 3) as usize];
        self.colour[idx] = Some(colour);
    }
}

/// Liang-Barsky against `[0, w] x [0, h]`, so a segment reaching far off-canvas
/// costs only its visible length. Non-finite input clips to nothing.
fn clip(a: (f32, f32), b: (f32, f32), w: f32, h: f32) -> Option<((f32, f32), (f32, f32))> {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let (mut t0, mut t1) = (0.0f32, 1.0f32);
    for (p, q) in [(-dx, a.0), (dx, w - a.0), (-dy, a.1), (dy, h - a.1)] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else {
            let t = q / p;
            if p < 0.0 {
                t0 = t0.max(t);
            } else {
                t1 = t1.min(t);
            }
        }
    }
    if t0.is_nan() || t1.is_nan() || t0 > t1 {
        return None;
    }
    let at = |t: f32| (a.0 + t * dx, a.1 + t * dy);
    let (p, q) = (at(t0), at(t1));
    if !(p.0.is_finite() && p.1.is_finite() && q.0.is_finite() && q.1.is_finite()) {
        return None;
    }
    Some((p, q))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use std::collections::HashSet;

    const HALF_ROW: char = '\u{2824}'; // both dots of row 2
    const LEFT_COLUMN: char = '\u{2847}'; // all four dots of column 0

    fn set_dots(canvas: &Canvas) -> HashSet<(i32, i32)> {
        let mut out = HashSet::new();
        for cy in 0..canvas.height {
            for cx in 0..canvas.width {
                let bits = canvas.dots[cy as usize * canvas.width as usize + cx as usize];
                for (i, column) in BITS.iter().enumerate() {
                    for (j, bit) in column.iter().enumerate() {
                        if bits & bit != 0 {
                            out.insert((
                                i32::from(cx) * 2 + i as i32,
                                i32::from(cy) * 4 + j as i32,
                            ));
                        }
                    }
                }
            }
        }
        out
    }

    /// Walks the dot set from `start` over king-moves and demands it reaches
    /// `end` and leaves nothing behind: one unbroken stroke, no strays.
    fn assert_one_stroke(dots: &HashSet<(i32, i32)>, start: (i32, i32), end: (i32, i32)) {
        assert!(dots.contains(&start), "missing start {start:?}");
        assert!(dots.contains(&end), "missing end {end:?}");
        let mut seen = HashSet::from([start]);
        let mut frontier = vec![start];
        while let Some((x, y)) = frontier.pop() {
            for nx in x - 1..=x + 1 {
                for ny in y - 1..=y + 1 {
                    if dots.contains(&(nx, ny)) && seen.insert((nx, ny)) {
                        frontier.push((nx, ny));
                    }
                }
            }
        }
        assert!(seen.contains(&end), "end not reachable from start");
        assert_eq!(
            seen.len(),
            dots.len(),
            "stray dots not connected to the stroke"
        );
    }

    #[test]
    fn dot_of_floors_into_the_two_by_four_grid() {
        assert_eq!(dot_of((0.0, 0.0)), (0, 0));
        assert_eq!(dot_of((0.5, 0.5)), (1, 2));
        assert_eq!(dot_of((2.75, 1.875)), (5, 7));
        assert_eq!(dot_of((-0.25, -0.1)), (-1, -1));
    }

    #[test]
    fn horizontal_line_at_cell_centre_lands_on_dot_row_two() {
        let mut canvas = Canvas::new(4, 1);
        canvas.line((0.25, 0.5), (2.75, 0.5), Color::Red);
        for x in 0..3 {
            assert_eq!(canvas.glyph(x, 0), Some((HALF_ROW, Color::Red)), "cell {x}");
        }
        assert_eq!(canvas.glyph(3, 0), None);
        assert_eq!(canvas.glyph(0, 5), None);
    }

    #[test]
    fn vertical_line_at_quarter_x_uses_column_zero() {
        let mut canvas = Canvas::new(2, 2);
        canvas.line((0.25, 0.125), (0.25, 1.875), Color::Green);
        assert_eq!(canvas.glyph(0, 0), Some((LEFT_COLUMN, Color::Green)));
        assert_eq!(canvas.glyph(0, 1), Some((LEFT_COLUMN, Color::Green)));
        assert_eq!(canvas.glyph(1, 0), None);
        assert_eq!(canvas.glyph(1, 1), None);
    }

    #[test]
    fn cell_space_diagonal_has_a_dot_on_every_row() {
        let mut canvas = Canvas::new(5, 5);
        canvas.line((0.0, 0.0), (4.0, 4.0), Color::White);
        let dots = set_dots(&canvas);
        // A 1:2 slope in dot space: 17 rows, one dot each, x never jumping.
        assert_eq!(dots.len(), 17);
        let mut by_row: Vec<(i32, i32)> = dots.iter().copied().collect();
        by_row.sort_by_key(|&(_, y)| y);
        for (row, &(x, y)) in by_row.iter().enumerate() {
            assert_eq!(y, row as i32);
            assert_eq!(x, row as i32 / 2);
        }
        assert_one_stroke(&dots, (0, 0), (8, 16));
    }

    #[test]
    fn any_slope_is_one_unbroken_stroke() {
        let cases = [
            ((0.25, 0.25), (9.75, 1.0)),    // shallow
            ((0.25, 0.25), (1.0, 4.75)),    // steep
            ((9.5, 0.125), (0.125, 4.875)), // downhill-left
            ((3.3, 2.2), (3.3, 2.2)),       // degenerate: a single dot
        ];
        for (from, to) in cases {
            let mut canvas = Canvas::new(10, 5);
            canvas.line(from, to, Color::Yellow);
            assert_one_stroke(&set_dots(&canvas), dot_of(from), dot_of(to));
        }
    }

    #[test]
    fn polyline_draws_each_consecutive_pair() {
        let mut canvas = Canvas::new(3, 3);
        canvas.polyline(&[(0.25, 0.5), (2.75, 0.5), (2.75, 2.5)], Color::Cyan);
        assert_eq!(canvas.glyph(0, 0), Some((HALF_ROW, Color::Cyan)));
        assert!(canvas.glyph(2, 1).is_some());
        assert!(canvas.glyph(2, 2).is_some());
        assert_eq!(canvas.glyph(0, 2), None);
    }

    #[test]
    fn later_line_takes_the_cell_colour_but_keeps_earlier_dots() {
        let mut canvas = Canvas::new(1, 1);
        canvas.line((0.25, 0.5), (0.75, 0.5), Color::Red);
        canvas.line((0.25, 0.125), (0.25, 0.875), Color::Blue);
        let (ch, colour) = canvas.glyph(0, 0).unwrap();
        assert_eq!(colour, Color::Blue);
        assert_eq!(ch as u32, 0x2800 | 0x47 | 0x24);
    }

    #[test]
    fn off_canvas_segments_are_clipped_not_panicked_on() {
        let mut canvas = Canvas::new(4, 2);
        canvas.line((-5.0, -5.0), (-1.0, -1.0), Color::Red);
        canvas.line((10.0, 0.5), (20.0, 0.5), Color::Red);
        canvas.line((f32::NAN, 0.5), (1.0, 0.5), Color::Red);
        canvas.line((f32::INFINITY, 0.5), (1.0, 0.5), Color::Red);
        assert!(set_dots(&canvas).is_empty());

        canvas.line((-1000.0, 0.5), (1000.0, 0.5), Color::Red);
        for x in 0..4 {
            assert_eq!(canvas.glyph(x, 0), Some((HALF_ROW, Color::Red)), "cell {x}");
            assert_eq!(canvas.glyph(x, 1), None);
        }
    }

    #[test]
    fn clear_forgets_everything() {
        let mut canvas = Canvas::new(2, 1);
        canvas.line((0.25, 0.5), (1.75, 0.5), Color::Red);
        canvas.clear();
        assert_eq!(canvas.glyph(0, 0), None);
        assert_eq!(canvas.glyph(1, 0), None);
    }

    #[test]
    fn composite_honours_allow_and_preserves_background() {
        let mut canvas = Canvas::new(3, 1);
        canvas.line((0.25, 0.5), (2.75, 0.5), Color::Red);
        let mut buf = Buffer::empty(Rect::new(0, 0, 3, 1));
        buf[(1, 0)].set_bg(Color::Blue);

        let untouched = buf.clone();
        canvas.composite(&mut buf, |_, _| false);
        assert_eq!(buf, untouched);

        canvas.composite(&mut buf, |x, _| x != 0);
        assert_eq!(buf[(0, 0)], untouched[(0, 0)]);
        assert_eq!(buf[(1, 0)].symbol(), HALF_ROW.to_string());
        assert_eq!(buf[(1, 0)].fg, Color::Red);
        assert_eq!(buf[(1, 0)].bg, Color::Blue);
        assert_eq!(buf[(2, 0)].symbol(), HALF_ROW.to_string());
        assert_eq!(buf[(2, 0)].fg, Color::Red);
        assert_eq!(buf[(2, 0)].bg, Color::Reset);
    }

    #[test]
    fn composite_skips_cells_outside_the_buffer() {
        let mut canvas = Canvas::new(4, 1);
        canvas.line((0.25, 0.5), (3.75, 0.5), Color::Red);
        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 1));
        canvas.composite(&mut buf, |_, _| true);
        assert_eq!(buf[(0, 0)].symbol(), HALF_ROW.to_string());
        assert_eq!(buf[(1, 0)].symbol(), HALF_ROW.to_string());
    }

    #[test]
    fn many_long_polylines_on_a_large_canvas_stay_fast() {
        let mut canvas = Canvas::new(300, 1000);
        let mut points = Vec::with_capacity(300);
        let start = std::time::Instant::now();
        for k in 0..300u32 {
            points.clear();
            for i in 0..300u32 {
                let t = i as f32;
                points.push((
                    t + 0.5,
                    (k as f32 * 3.3 + t * 0.7 + (t * 0.1).sin() * 20.0) % 1000.0,
                ));
            }
            canvas.polyline(&points, Color::Indexed(k as u8));
        }
        let elapsed = start.elapsed();
        assert!(set_dots(&canvas).len() > 10_000);
        // Debug builds are several times slower than release; keep a wide margin.
        assert!(elapsed.as_millis() < 500, "took {elapsed:?}");
    }
}
