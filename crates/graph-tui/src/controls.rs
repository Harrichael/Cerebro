//! The switches, where they sit on screen, and what each one currently reads.
//!
//! One list drives three things that otherwise drift apart: the panel drawn in
//! the corner, the key that works it, and the cell a click lands on. The
//! browser view keeps its toolbar and its keyboard shortcuts in separate
//! places and they disagree; there is no reason to repeat that here.
//!
//! What a control *does* is not here. This module knows that `x` hides the
//! selection and where the word "hide" is painted; only the app knows what
//! hiding means.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Tests,
    OnePerPair,
    Edges,
    Hide,
    Scope,
    ShowAll,
    Reset,
}

/// Key, control, label — in the order they are drawn.
const ROWS: &[(char, Control, &str)] = &[
    ('t', Control::Tests, "tests"),
    ('e', Control::OnePerPair, "one edge/pair"),
    ('r', Control::Edges, "edges"),
    ('x', Control::Hide, "hide node"),
    ('s', Control::Scope, "scope to node"),
    ('a', Control::ShowAll, "show all"),
    ('0', Control::Reset, "zoom all out"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// A switch, drawn with the position it is in.
    Switch(bool),
    /// A one-shot action, greyed out and unclickable when there is nothing for
    /// it to act on.
    Action(bool),
    /// An action that also says how much it would undo. Zero disables it.
    Undo(usize),
}

impl State {
    /// A dead control does nothing, by key or by click.
    pub fn enabled(self) -> bool {
        match self {
            State::Switch(_) => true,
            State::Action(on) => on,
            State::Undo(n) => n > 0,
        }
    }

    fn reads(self) -> String {
        match self {
            State::Switch(true) => "on".into(),
            State::Switch(false) => "off".into(),
            State::Action(_) => String::new(),
            State::Undo(n) => n.to_string(),
        }
    }
}

pub fn for_key(c: char) -> Option<Control> {
    ROWS.iter().find(|(k, ..)| *k == c).map(|(_, ctl, _)| *ctl)
}

/// Where the panel landed, so a click can be traced back to a control.
#[derive(Debug, Clone, Default)]
pub struct Panel {
    pub rect: Rect,
    hits: Vec<(Control, Rect, bool)>,
}

impl Panel {
    pub fn contains(&self, col: u16, row: u16) -> bool {
        self.rect.contains((col, row).into())
    }

    /// The control clicked, if the click landed on a live one. A disabled row
    /// still swallows the click -- it is part of the panel, and letting it
    /// through would select whatever node the panel is covering.
    pub fn hit(&self, col: u16, row: u16) -> Option<Control> {
        self.hits
            .iter()
            .find(|(_, r, live)| *live && r.contains((col, row).into()))
            .map(|(c, ..)| *c)
    }
}

const TITLE: &str = " view ";

/// Draw the panel in the top-right of `area` and report where it went.
///
/// `states` is looked up per control rather than being a parallel array, so a
/// caller cannot silently mis-pair a state with the wrong row.
pub fn draw(buf: &mut Buffer, area: Rect, states: impl Fn(Control) -> State) -> Panel {
    let label_w = ROWS.iter().map(|(_, _, l)| l.len()).max().unwrap_or(0) as u16;
    // key + space + label + gap + the widest reading a switch has.
    let inner_w = 2 + label_w + 1 + 3;
    let (w, h) = (inner_w + 2, ROWS.len() as u16 + 2);
    if area.width < w || area.height < h {
        // No room. Better no panel than one drawn over the diagram in pieces;
        // every control still has its key.
        return Panel::default();
    }
    let rect = Rect::new(area.right() - w, area.y, w, h);
    let frame = Style::default().fg(Color::DarkGray);

    for x in rect.x..rect.right() {
        for y in [rect.y, rect.bottom() - 1] {
            buf[(x, y)].set_symbol("─").set_style(frame);
        }
    }
    for y in rect.y..rect.bottom() {
        for x in [rect.x, rect.right() - 1] {
            buf[(x, y)].set_symbol("│").set_style(frame);
        }
    }
    for (pos, c) in [
        ((rect.x, rect.y), "┌"),
        ((rect.right() - 1, rect.y), "┐"),
        ((rect.x, rect.bottom() - 1), "└"),
        ((rect.right() - 1, rect.bottom() - 1), "┘"),
    ] {
        buf[pos].set_symbol(c).set_style(frame);
    }
    for (i, c) in TITLE.chars().enumerate() {
        buf[(rect.x + 1 + i as u16, rect.y)].set_symbol(&c.to_string()).set_style(frame);
    }

    let mut hits = Vec::new();
    for (i, (key, control, label)) in ROWS.iter().enumerate() {
        let state = states(*control);
        let live = state.enabled();
        let y = rect.y + 1 + i as u16;
        let row = Rect::new(rect.x + 1, y, inner_w, 1);
        let dim = Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM);
        let (key_style, text_style) = if live {
            (Style::default().fg(Color::Yellow), Style::default().fg(Color::Gray))
        } else {
            (dim, dim)
        };
        write(buf, row.x, y, &format!("{key} "), key_style);
        write(buf, row.x + 2, y, label, text_style);
        let reads = state.reads();
        let on = matches!(state, State::Switch(true));
        write(
            buf,
            row.right() - reads.chars().count() as u16,
            y,
            &reads,
            if on {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                text_style
            },
        );
        hits.push((*control, row, live));
    }
    Panel { rect, hits }
}

fn write(buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style) {
    for (i, c) in text.chars().enumerate() {
        let cx = x + i as u16;
        if cx >= buf.area().width || y >= buf.area().height {
            return;
        }
        buf[(cx, y)].set_symbol(&c.to_string()).set_style(style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel(w: u16, h: u16, states: impl Fn(Control) -> State) -> (Buffer, Panel) {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        let p = draw(&mut buf, area, states);
        (buf, p)
    }

    fn text(buf: &Buffer, r: Rect) -> String {
        (r.y..r.bottom())
            .map(|y| (r.x..r.right()).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The point of the panel is that clicking a row does what its key does.
    /// Every row is checked, because an off-by-one in the layout would be
    /// invisible in a test that only clicked the first.
    #[test]
    fn clicking_any_row_names_the_control_its_key_names() {
        let (_, p) = panel(80, 24, |_| State::Switch(true));
        for (i, (key, control, _)) in ROWS.iter().enumerate() {
            let y = p.rect.y + 1 + i as u16;
            assert_eq!(p.hit(p.rect.x + 2, y), Some(*control), "row {i} clicked wrong");
            assert_eq!(for_key(*key), Some(*control), "key {key} names a different control");
        }
        assert_eq!(p.hit(p.rect.x, p.rect.y), None, "the border is not a control");
    }

    /// A control with nothing to act on must not fire, and must still swallow
    /// the click -- letting it through would reach the diagram underneath.
    #[test]
    fn a_dead_control_neither_fires_nor_lets_the_click_through() {
        let (buf, p) = panel(80, 24, |c| match c {
            Control::ShowAll => State::Undo(0),
            _ => State::Switch(true),
        });
        let i = ROWS.iter().position(|(_, c, _)| *c == Control::ShowAll).unwrap();
        let y = p.rect.y + 1 + i as u16;
        assert_eq!(p.hit(p.rect.x + 2, y), None, "a dead control fired");
        assert!(p.contains(p.rect.x + 2, y), "the click has to stop at the panel");
        assert!(text(&buf, Rect::new(p.rect.x, y, p.rect.width, 1)).contains("show all"));
    }

    /// A terminal too small for the panel gets no panel, not a broken one.
    #[test]
    fn there_is_no_panel_when_there_is_no_room() {
        let (_, p) = panel(12, 24, |_| State::Switch(true));
        assert_eq!(p.rect, Rect::default());
        assert_eq!(p.hit(0, 0), None);
        assert!(!p.contains(0, 0), "an absent panel must not swallow clicks");
    }
}
