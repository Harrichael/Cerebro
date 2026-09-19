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
    Editor,
    Zoom,
    Tests,
    OnePerPair,
    Edges,
    Hide,
    Scope,
    ShowAll,
    CollapseAll,
    Relayout,
}

/// Key, control, label — in the order they are drawn.
const ROWS: &[(char, Control, &str)] = &[
    ('o', Control::Editor, "editor pane"),
    ('z', Control::Zoom, "zoom"),
    ('t', Control::Tests, "tests"),
    ('e', Control::OnePerPair, "one edge/pair"),
    ('r', Control::Edges, "edges"),
    ('x', Control::Hide, "hide node"),
    ('s', Control::Scope, "scope to node"),
    ('a', Control::ShowAll, "show all"),
    ('0', Control::CollapseAll, "collapse all"),
    ('L', Control::Relayout, "re-layout"),
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
    /// A setting with more than two positions, which says which one it is in.
    Level(&'static str),
}

impl State {
    /// A dead control does nothing, by key or by click.
    pub fn enabled(self) -> bool {
        match self {
            State::Switch(_) => true,
            State::Action(on) => on,
            State::Undo(n) => n > 0,
            State::Level(_) => true,
        }
    }

    fn reads(self) -> String {
        match self {
            State::Switch(true) => "on".into(),
            State::Switch(false) => "off".into(),
            State::Action(_) => String::new(),
            State::Undo(n) => n.to_string(),
            State::Level(l) => l.into(),
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

/// A button drawn in a node's own top-right corner, as the browser view draws
/// them. The glyph is the key that does the same thing from the keyboard, so
/// there is nothing extra to learn from seeing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeAction {
    Expand,
    Collapse,
    Scope,
    Hide,
}

impl NodeAction {
    pub fn glyph(self) -> char {
        match self {
            NodeAction::Expand => '+',
            NodeAction::Collapse => '-',
            NodeAction::Scope => 's',
            NodeAction::Hide => '×',
        }
    }
}

/// Which buttons a node offers, left to right. A box can be folded back up
/// and singled out; a leaf can only be opened, and only if it holds anything.
pub fn node_actions(is_box: bool, expandable: bool) -> &'static [NodeAction] {
    use NodeAction::{Collapse, Expand, Hide, Scope};
    match (is_box, expandable) {
        (true, _) => &[Scope, Collapse, Hide],
        (false, true) => &[Expand, Hide],
        (false, false) => &[Hide],
    }
}

/// Where the buttons sit: one cell each at the right of the node's frame,
/// stopping short of the corner. A framed node carries them on its top
/// border; an unframed one has only the rule under its name, so they go
/// there rather than over the name itself.
///
/// `None` when the node is too narrow to give them room. A node whose whole
/// edge is buttons has none left to read as a frame, and its name is the
/// thing the user came for.
pub fn node_action_row(rect: Rect, count: usize, bordered: bool) -> Option<(u16, u16)> {
    let count = count as u16;
    if rect.height < 2 {
        return None;
    }
    // A frame has a corner to keep clear at the end of the run; a rule has
    // none, so its buttons sit flush and do not leave a stray cell of rule
    // hanging past them.
    let (y, pad) = if bordered { (rect.y, 1) } else { (rect.bottom() - 1, 0) };
    (rect.width >= count + 2 + 2 * pad).then(|| (rect.right() - pad - count, y))
}

/// The button at a cell of a node's frame, if any.
pub fn node_action_at(
    rect: Rect,
    actions: &[NodeAction],
    bordered: bool,
    col: u16,
    row: u16,
) -> Option<NodeAction> {
    let (x0, y) = node_action_row(rect, actions.len(), bordered)?;
    (row == y && col >= x0 && col < x0 + actions.len() as u16)
        .then(|| actions[(col - x0) as usize])
}

const TITLE: &str = " view ";

/// Draw the panel in the top-right of `area` and report where it went.
///
/// `states` is looked up per control rather than being a parallel array, so a
/// caller cannot silently mis-pair a state with the wrong row.
pub fn draw(buf: &mut Buffer, area: Rect, states: impl Fn(Control) -> State) -> Panel {
    let label_w = ROWS.iter().map(|(_, _, l)| l.len()).max().unwrap_or(0) as u16;
    // key + space + label + gap + the widest reading a control has.
    let inner_w = 2 + label_w + 1 + 5;
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
        // Blanked first. The panel is drawn over the diagram, and a label
        // shorter than the row left the gap between it and its reading
        // showing whatever box or edge happened to be underneath.
        for x in row.x..row.right() {
            buf[(x, y)].reset();
        }
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
            if on || matches!(state, State::Level(_)) {
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

    /// The buttons have to be where they are drawn, and a narrow node has to
    /// keep its border rather than becoming a row of buttons.
    #[test]
    fn a_nodes_buttons_are_where_the_clicks_land() {
        let rect = Rect::new(10, 4, 20, 4);
        let acts = node_actions(false, true);
        assert_eq!(acts.len(), 2, "an expandable leaf offers expand and hide");
        let (x0, y) = node_action_row(rect, acts.len(), true).expect("room for two buttons");
        assert_eq!((x0, y), (27, 4), "buttons sit at the right of the top border");

        assert_eq!(node_action_at(rect, acts, true, 27, 4), Some(NodeAction::Expand));
        assert_eq!(node_action_at(rect, acts, true, 28, 4), Some(NodeAction::Hide));
        assert_eq!(node_action_at(rect, acts, true, 29, 4), None, "the corner is not a button");
        assert_eq!(node_action_at(rect, acts, true, 27, 5), None, "only one row carries them");

        // Unframed, the same buttons move to the rule under the name so they
        // do not eat the last two characters of it.
        let bare = Rect::new(10, 4, 20, 2);
        assert_eq!(node_action_row(bare, acts.len(), false), Some((28, 5)));
        assert_eq!(node_action_at(bare, acts, false, 29, 5), Some(NodeAction::Hide));
        assert_eq!(node_action_at(bare, acts, false, 29, 4), None, "not over the name");
        assert_eq!(node_action_at(bare, acts, false, 27, 5), None, "the rule is not a button");

        let narrow = Rect::new(0, 0, 5, 4);
        assert_eq!(node_action_row(narrow, acts.len(), true), None);
        assert_eq!(node_action_at(narrow, acts, true, 2, 0), None, "a narrow node kept its frame");
    }

    /// A leaf that holds nothing cannot be expanded, and offering the button
    /// anyway would be a control that does nothing.
    #[test]
    fn only_what_can_be_done_gets_a_button() {
        assert_eq!(node_actions(false, false), &[NodeAction::Hide]);
        assert!(node_actions(true, false).contains(&NodeAction::Collapse));
        assert!(!node_actions(true, false).contains(&NodeAction::Expand));
    }

    /// A terminal too small for the panel gets no panel, not a broken one.
    #[test]
    fn there_is_no_panel_when_there_is_no_room() {
        let (_, p) = panel(12, 24, |_| State::Switch(true));
        assert_eq!(p.rect, Rect::default());
        assert_eq!(p.hit(0, 0), None);
        assert!(!p.contains(0, 0), "an absent panel must not swallow clicks");
    }

    /// The panel sits over the diagram, so it has to be opaque. A label
    /// shorter than its row used to leave the gap before its reading showing
    /// whatever box or edge was underneath, which made the panel look like
    /// the diagram had been drawn through it.
    #[test]
    fn the_panel_covers_what_it_is_drawn_over() {
        let area = Rect::new(0, 0, 60, 20);
        let mut buf = Buffer::empty(area);
        for y in 0..area.height {
            for x in 0..area.width {
                buf[(x, y)].set_symbol("━");
            }
        }
        let panel = draw(&mut buf, area, |_| State::Switch(true));

        let mut showing = Vec::new();
        for y in panel.rect.y..panel.rect.bottom() {
            for x in panel.rect.x..panel.rect.right() {
                if buf[(x, y)].symbol() == "━" {
                    showing.push((x, y));
                }
            }
        }
        assert!(showing.is_empty(), "the diagram shows through the panel at {showing:?}");
    }
}
