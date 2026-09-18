//! The screen nvim is describing, and how it lands in a ratatui buffer.
//!
//! Nvim sends edits, not frames: a batch of `grid_line` runs, a `grid_scroll`
//! that shifts a region, a `flush` when the screen is once again consistent.
//! This applies them in order and holds the result until someone paints it.
//!
//! The grid id every event carries is ignored, because `ext_linegrid` on its
//! own never makes a second one: nvim composites everything it draws --
//! windows, floats, the cmdline, messages, the popup menu -- into the global
//! grid, which is the whole reason a terminal-shaped buffer is enough to show
//! a Neovim.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use rmpv::Value;

/// Which mode nvim is in, to the resolution a caller needs to decide whether
/// a key is safe to take away from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Visual,
    /// Insert, replace, cmdline, operator-pending, terminal: everything where
    /// a keystroke is part of something the user is in the middle of saying.
    Busy,
}

/// What nvim's window is showing, as of the last `win_viewport`. Every
/// position is counted from zero, which is how nvim sends them and how the
/// entity graph's line ranges already read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Viewport {
    pub topline: usize,
    pub botline: usize,
    pub curline: usize,
    pub curcol: usize,
}

#[derive(Clone)]
struct Cell {
    symbol: String,
    hl: i64,
}

impl Default for Cell {
    /// A blank, not an empty string: an empty symbol means the right half of
    /// a double-width char, which is not what an untouched cell is.
    fn default() -> Self {
        Cell { symbol: " ".into(), hl: 0 }
    }
}

/// One entry of nvim's highlight table. Kept as nvim states it and turned
/// into a ratatui `Style` at paint time, because `hl_attr_define` for an id
/// can arrive after the cells that already use it.
#[derive(Clone, Copy, Default)]
struct Attrs {
    fg: Option<Color>,
    bg: Option<Color>,
    reverse: bool,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
}

pub struct Grid {
    width: u16,
    height: u16,
    cells: Vec<Cell>,
    attrs: Vec<Attrs>,
    default_fg: Color,
    default_bg: Color,
    cursor: (u16, u16),
    mode: Mode,
    viewport: Viewport,
}

impl Default for Grid {
    fn default() -> Self {
        Grid {
            width: 0,
            height: 0,
            cells: Vec::new(),
            attrs: Vec::new(),
            default_fg: Color::Reset,
            default_bg: Color::Reset,
            cursor: (0, 0),
            mode: Mode::Normal,
            viewport: Viewport::default(),
        }
    }
}

impl Grid {
    pub fn cursor(&self) -> (u16, u16) {
        self.cursor
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    /// The text of one row, for tests and for anything that wants to read the
    /// screen rather than show it.
    pub fn row(&self, y: u16) -> String {
        if y >= self.height {
            return String::new();
        }
        let start = y as usize * self.width as usize;
        self.cells[start..start + self.width as usize]
            .iter()
            .map(|c| c.symbol.as_str())
            .collect()
    }

    /// Apply one event from a `redraw` batch. Returns true for `flush`, which
    /// is nvim saying the screen is consistent and worth showing.
    pub fn apply(&mut self, name: &str, args: &[Value]) -> bool {
        match name {
            "flush" => return true,
            "grid_resize" => self.resize(u16_at(args, 1), u16_at(args, 2)),
            "grid_clear" => self.cells.iter_mut().for_each(|c| *c = Cell::default()),
            "grid_line" => self.line(u16_at(args, 1), u16_at(args, 2), array_at(args, 3)),
            "grid_scroll" => self.scroll(
                u16_at(args, 1),
                u16_at(args, 2),
                u16_at(args, 3),
                u16_at(args, 4),
                int_at(args, 5) as i32,
            ),
            "grid_cursor_goto" => self.cursor = (u16_at(args, 2), u16_at(args, 1)),
            "default_colors_set" => {
                self.default_fg = colour(args.first()).unwrap_or(Color::Reset);
                self.default_bg = colour(args.get(1)).unwrap_or(Color::Reset);
            }
            "hl_attr_define" => self.define(int_at(args, 0) as usize, args.get(1)),
            "mode_change" => self.mode = mode_of(args.first().and_then(Value::as_str)),
            "win_viewport" => {
                self.viewport = Viewport {
                    topline: int_at(args, 2) as usize,
                    botline: int_at(args, 3) as usize,
                    curline: int_at(args, 4) as usize,
                    curcol: int_at(args, 5) as usize,
                }
            }
            _ => {}
        }
        false
    }

    fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.cells = vec![Cell::default(); width as usize * height as usize];
    }

    /// A run of cells is `[text, hl_id?, repeat?]`: a missing id means the one
    /// before it, a missing repeat means once. Nvim leans on both hard -- a
    /// blank line arrives as a single cell repeated eighty times.
    fn line(&mut self, row: u16, col: u16, cells: &[Value]) {
        if row >= self.height {
            return;
        }
        let mut x = col;
        let mut hl = 0;
        for cell in cells {
            let Some(parts) = cell.as_array() else { continue };
            let symbol = parts.first().and_then(Value::as_str).unwrap_or(" ");
            if let Some(id) = parts.get(1).and_then(Value::as_i64) {
                hl = id;
            }
            let repeat = parts.get(2).and_then(Value::as_u64).unwrap_or(1);
            for _ in 0..repeat {
                if x >= self.width {
                    return;
                }
                let at = row as usize * self.width as usize + x as usize;
                self.cells[at] = Cell { symbol: symbol.to_owned(), hl };
                x += 1;
            }
        }
    }

    /// Shift the block of rows `top..bot` and columns `left..right` by `rows`
    /// -- up when positive.
    ///
    /// The columns matter. Nvim scrolls one window at a time, so a screen
    /// split down the middle sends `left=0 right=30` on a 60-column grid;
    /// shifting the whole row instead would drag the window next door up with
    /// it. The rows this leaves behind are not cleared: nvim sends
    /// `grid_line` for whatever belongs there, and clearing first would only
    /// make it flicker.
    fn scroll(&mut self, top: u16, bot: u16, left: u16, right: u16, rows: i32) {
        let (top, bot) = (top as usize, (bot as usize).min(self.height as usize));
        let width = self.width as usize;
        let (left, right) = (left as usize, (right as usize).min(width));
        let copy = |cells: &mut Vec<Cell>, from: usize, to: usize| {
            for x in left..right {
                cells[to * width + x] = cells[from * width + x].clone();
            }
        };
        match rows {
            rows if rows > 0 => {
                let rows = rows as usize;
                for y in top..bot.saturating_sub(rows) {
                    copy(&mut self.cells, y + rows, y);
                }
            }
            rows if rows < 0 => {
                let rows = rows.unsigned_abs() as usize;
                for y in (top + rows..bot).rev() {
                    copy(&mut self.cells, y - rows, y);
                }
            }
            _ => {}
        }
    }

    fn define(&mut self, id: usize, rgb: Option<&Value>) {
        let Some(map) = rgb.and_then(Value::as_map) else { return };
        let get = |key: &str| {
            map.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v)
        };
        let flag = |key: &str| get(key).and_then(Value::as_bool).unwrap_or(false);
        let attrs = Attrs {
            fg: colour(get("foreground")),
            bg: colour(get("background")),
            reverse: flag("reverse"),
            bold: flag("bold"),
            italic: flag("italic"),
            // Every flavour of underline reads the same in a terminal cell.
            underline: ["underline", "undercurl", "underdouble", "underdotted", "underdashed"]
                .iter()
                .any(|k| flag(k)),
            strikethrough: flag("strikethrough"),
        };
        if self.attrs.len() <= id {
            self.attrs.resize(id + 1, Attrs::default());
        }
        self.attrs[id] = attrs;
    }

    fn style(&self, hl: i64) -> Style {
        let attrs = usize::try_from(hl).ok().and_then(|id| self.attrs.get(id)).copied();
        let attrs = attrs.unwrap_or_default();
        let (mut fg, mut bg) =
            (attrs.fg.unwrap_or(self.default_fg), attrs.bg.unwrap_or(self.default_bg));
        // Swapped here rather than handed to the terminal as REVERSED: the
        // diagram next door draws its own selection that way, and two
        // different meanings for one attribute is how a screen starts lying.
        if attrs.reverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        let mut modifier = Modifier::empty();
        modifier.set(Modifier::BOLD, attrs.bold);
        modifier.set(Modifier::ITALIC, attrs.italic);
        modifier.set(Modifier::UNDERLINED, attrs.underline);
        modifier.set(Modifier::CROSSED_OUT, attrs.strikethrough);
        Style::default().fg(fg).bg(bg).add_modifier(modifier)
    }

    /// Paint into `area`. Cells are reset rather than restyled: a ratatui
    /// style patches what is already in the cell, and nvim's attributes are
    /// whole descriptions, not adjustments to whatever was there last frame.
    pub fn draw(&self, buf: &mut Buffer, area: Rect) {
        for y in 0..area.height.min(self.height) {
            for x in 0..area.width.min(self.width) {
                let cell = &self.cells[y as usize * self.width as usize + x as usize];
                let target = &mut buf[(area.x + x, area.y + y)];
                target.reset();
                // An empty symbol is the second half of a double-width glyph,
                // which is how ratatui spells it too.
                if !cell.symbol.is_empty() {
                    target.set_symbol(&cell.symbol);
                }
                target.set_style(self.style(cell.hl));
            }
        }
    }
}

fn int_at(args: &[Value], i: usize) -> i64 {
    args.get(i).and_then(Value::as_i64).unwrap_or(0)
}

fn u16_at(args: &[Value], i: usize) -> u16 {
    u16::try_from(int_at(args, i)).unwrap_or(0)
}

fn array_at(args: &[Value], i: usize) -> &[Value] {
    args.get(i).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// Nvim sends colours as one packed integer, and -1 (or nothing) for "use the
/// default".
fn colour(value: Option<&Value>) -> Option<Color> {
    let rgb = u32::try_from(value?.as_i64()?).ok()?;
    Some(Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8))
}

fn mode_of(name: Option<&str>) -> Mode {
    match name {
        // `cmdline_normal` is not normal mode; only an exact match is.
        Some("normal") => Mode::Normal,
        Some(m) if m.starts_with("visual") || m.starts_with("select") => Mode::Visual,
        _ => Mode::Busy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run as nvim sends it: text, and optionally the highlight it starts
    /// using and how many times it repeats.
    fn run(text: &str, hl: Option<i64>, repeat: Option<u64>) -> Value {
        let mut parts = vec![text.into()];
        if let Some(hl) = hl {
            parts.push(hl.into());
        }
        if let Some(repeat) = repeat {
            parts.push(repeat.into());
        }
        Value::Array(parts)
    }

    fn highlight(fg: i64, extra: &[(&str, bool)]) -> Value {
        let mut map = vec![("foreground".into(), fg.into())];
        for (key, on) in extra {
            map.push(((*key).into(), (*on).into()));
        }
        Value::Map(map)
    }

    fn grid(width: u16, height: u16) -> Grid {
        let mut grid = Grid::default();
        grid.apply("grid_resize", &[1.into(), width.into(), height.into()]);
        grid
    }

    fn painted(grid: &Grid, width: u16, height: u16) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        grid.draw(&mut buf, area);
        buf
    }

    /// The screen nvim describes is the screen that gets painted, down to
    /// which cells carry which of its highlights. Nvim leans hard on the
    /// short forms -- a cell with no highlight id keeps the one before it,
    /// and a run of blanks arrives as one cell with a repeat count -- so a
    /// reader that took them literally would paint a mostly empty line.
    #[test]
    fn a_line_of_runs_paints_the_text_and_colours_nvim_gave() {
        let mut grid = grid(8, 2);
        grid.apply("hl_attr_define", &[1.into(), highlight(0xff0000, &[("bold", true)])]);
        grid.apply(
            "grid_line",
            &[
                1.into(),
                0.into(),
                0.into(),
                Value::Array(vec![
                    run("h", Some(1), None),
                    run("i", None, None),
                    run(" ", Some(0), Some(6)),
                ]),
            ],
        );

        assert_eq!(grid.row(0), "hi      ");
        let buf = painted(&grid, 8, 2);
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(255, 0, 0));
        assert_eq!(buf[(1, 0)].fg, Color::Rgb(255, 0, 0), "an id carries on until it changes");
        assert!(buf[(1, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(2, 0)].fg, Color::Reset, "id 0 is the default highlight");
        assert!(!buf[(2, 0)].modifier.contains(Modifier::BOLD));
    }

    /// Reverse is spelled out in the colours rather than passed along as an
    /// attribute, because the caller draws its own selection reversed and one
    /// attribute cannot mean two things on one screen.
    #[test]
    fn a_reversed_highlight_arrives_as_swapped_colours() {
        let mut grid = grid(2, 1);
        grid.apply("default_colors_set", &[0xffffff.into(), 0x000000.into()]);
        grid.apply("hl_attr_define", &[7.into(), highlight(0x00ff00, &[("reverse", true)])]);
        grid.apply(
            "grid_line",
            &[1.into(), 0.into(), 0.into(), Value::Array(vec![run("x", Some(7), None)])],
        );

        let buf = painted(&grid, 2, 1);
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(0, 0, 0), "the default background became the text");
        assert_eq!(buf[(0, 0)].bg, Color::Rgb(0, 255, 0));
        assert!(!buf[(0, 0)].modifier.contains(Modifier::REVERSED));
    }

    /// Scrolling moves what is already on screen instead of redrawing it, so
    /// a grid that ignored it would keep showing the old lines wherever nvim
    /// did not bother to send replacements.
    #[test]
    fn scrolling_shifts_the_rows_that_stay_on_screen() {
        let mut grid = grid(1, 4);
        for (row, text) in ["a", "b", "c", "d"].iter().enumerate() {
            grid.apply(
                "grid_line",
                &[
                    1.into(),
                    row.into(),
                    0.into(),
                    Value::Array(vec![run(text, Some(0), None)]),
                ],
            );
        }

        // [grid, top, bot, left, right, rows, cols]
        grid.apply(
            "grid_scroll",
            &[1.into(), 0.into(), 4.into(), 0.into(), 1.into(), 1.into(), 0.into()],
        );
        assert_eq!([grid.row(0), grid.row(1), grid.row(2)], ["b", "c", "d"]);

        grid.apply(
            "grid_scroll",
            &[1.into(), 0.into(), 4.into(), 0.into(), 1.into(), (-2).into(), 0.into()],
        );
        assert_eq!([grid.row(2), grid.row(3)], ["b", "c"]);
    }

    /// Nvim scrolls one window at a time, and says which columns it means.
    /// A screen split down the middle sends `left 0 right 2` on a four-column
    /// grid; taking that to mean the whole row drags the window next door
    /// along with it.
    #[test]
    fn scrolling_one_window_leaves_the_columns_beside_it_alone() {
        let mut grid = grid(4, 3);
        for (row, text) in ["LLRR", "llrr", "____"].iter().enumerate() {
            let runs: Vec<Value> =
                text.chars().map(|c| run(&c.to_string(), Some(0), None)).collect();
            grid.apply(
                "grid_line",
                &[1.into(), row.into(), 0.into(), Value::Array(runs)],
            );
        }

        grid.apply(
            "grid_scroll",
            &[1.into(), 0.into(), 3.into(), 0.into(), 2.into(), 1.into(), 0.into()],
        );

        assert_eq!(grid.row(0), "llRR", "only the left half moved up");
        assert_eq!(grid.row(1), "__rr");
    }

    /// Only a bare `normal` is safe to take a key away from: the caller uses
    /// this to decide whether ctrl-w is a pane move or a word being deleted.
    #[test]
    fn the_modes_that_are_mid_sentence_are_all_busy() {
        assert_eq!(mode_of(Some("normal")), Mode::Normal);
        assert_eq!(mode_of(Some("visual")), Mode::Visual);
        assert_eq!(mode_of(Some("select")), Mode::Visual);
        for busy in ["insert", "cmdline_normal", "operator", "replace", "terminal"] {
            assert_eq!(mode_of(Some(busy)), Mode::Busy, "{busy}");
        }
    }
}
