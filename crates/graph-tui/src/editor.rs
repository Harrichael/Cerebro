//! The pane beside the diagram, and the rules for sharing a screen and a
//! keyboard with it.
//!
//! The pane is a real Neovim (see the `nvim-ui` crate), which means it wants
//! every key on the board -- `q`, `z`, `x`, `c`, `g` and `?` all mean
//! something to it and something else to the diagram. So the split is by
//! focus, not by key: whoever has the keyboard gets all of it, and `ctrl-w h`
//! / `ctrl-w l` move between them, which is the verb the user's fingers
//! already know for moving between windows.
//!
//! `ctrl-w` is claimed only while nvim is in normal or visual mode. In insert
//! and cmdline mode it deletes a word, and taking it there would break typing
//! in an editor, which is the one thing the pane exists to allow.

use std::path::Path;
use std::sync::mpsc::Receiver;

use anyhow::Result;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::Rect;

use nvim_ui::{Mode, Nvim};

/// Neither pane is worth having below these. A body too narrow for both is
/// given entirely to whichever one the user is looking at.
const PANE_MIN: u16 = 40;
const GRAPH_MIN: u16 = 24;
/// Columns one `ctrl-w <` or `ctrl-w >` moves the divider.
const RESIZE_STEP: i32 = 4;

/// What the pane did with a key press, for the app that handed it over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// Dealt with -- typed at nvim, or swallowed as half of a `ctrl-w`.
    Pane,
    /// `ctrl-w h` with no nvim window to the left: the diagram is what is
    /// left of here.
    GiveUpFocus,
    /// `ctrl-w <` or `ctrl-w >`.
    Resize(i32),
}

pub struct Editor {
    nvim: Nvim,
    /// Columns of the body the pane takes, before any clamping. Held as a
    /// wish rather than a measurement so a window that grows and shrinks
    /// again comes back to the split the user chose.
    width: u16,
    /// A `ctrl-w` is waiting for the key that says what it meant.
    pending: bool,
}

impl Editor {
    pub fn open(root: &Path, area: Rect) -> Result<(Editor, Receiver<nvim_ui::Event>)> {
        let (nvim, events) = Nvim::spawn(root, (area.width.max(1), area.height.max(1)), &[])?;
        Ok((Editor { nvim, width: area.width, pending: false }, events))
    }

    pub fn nvim(&self) -> &Nvim {
        &self.nvim
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn set_width(&mut self, width: u16) {
        self.width = width;
    }

    pub fn resize(&self, area: Rect) {
        self.nvim.resize(area.width.max(1), area.height.max(1));
    }

    pub fn draw(&self, buf: &mut Buffer, area: Rect) {
        self.nvim.draw(buf, area);
    }

    /// Where the terminal's own cursor belongs while the pane has the
    /// keyboard, so the user can see where they are typing.
    pub fn cursor(&self, area: Rect) -> (u16, u16) {
        let (col, row) = self.nvim.cursor();
        (area.x + col.min(area.width.saturating_sub(1)), area.y + row.min(area.height.saturating_sub(1)))
    }

    pub fn key(&mut self, key: KeyEvent) -> Handled {
        if std::mem::take(&mut self.pending) {
            return self.window_key(key);
        }
        let ctrl_w = key.code == KeyCode::Char('w')
            && key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl_w && matches!(self.nvim.mode(), Mode::Normal | Mode::Visual) {
            self.pending = true;
            return Handled::Pane;
        }
        self.nvim.key(key);
        Handled::Pane
    }

    /// The key after a `ctrl-w`. Only the moves that would fall off the edge
    /// of nvim's own windows belong to cerebro; the rest -- `ctrl-w v`,
    /// `ctrl-w s`, `ctrl-w o` -- are nvim's and are passed on whole.
    fn window_key(&self, key: KeyEvent) -> Handled {
        match key.code {
            KeyCode::Char('h') if !self.has_window_toward('h') => Handled::GiveUpFocus,
            KeyCode::Char('<') => Handled::Resize(-RESIZE_STEP),
            KeyCode::Char('>') => Handled::Resize(RESIZE_STEP),
            _ => {
                self.nvim.input("<C-w>");
                self.nvim.key(key);
                Handled::Pane
            }
        }
    }

    /// Has nvim a window of its own in that direction? `winnr('1h')` answers
    /// with the window to the left, or with this one when there is none --
    /// so the user who splits the pane keeps `ctrl-w h` for moving inside it,
    /// and only falls out to the diagram from the leftmost window.
    fn has_window_toward(&self, direction: char) -> bool {
        let winnr = |of: &str| self.nvim.eval(of).ok().and_then(|n| n.as_i64());
        match (winnr("winnr()"), winnr(&format!("winnr('1{direction}')"))) {
            (Some(here), Some(there)) => here != there,
            // No answer means no split worth honouring.
            _ => false,
        }
    }

    pub fn mouse(&self, kind: MouseEventKind, column: u16, row: u16, area: Rect) {
        let (col, row) = (column.saturating_sub(area.x), row.saturating_sub(area.y));
        let (button, action) = match kind {
            MouseEventKind::Down(MouseButton::Left) => ("left", "press"),
            MouseEventKind::Down(MouseButton::Right) => ("right", "press"),
            MouseEventKind::Down(MouseButton::Middle) => ("middle", "press"),
            MouseEventKind::ScrollUp => ("wheel", "up"),
            MouseEventKind::ScrollDown => ("wheel", "down"),
            MouseEventKind::ScrollLeft => ("wheel", "left"),
            MouseEventKind::ScrollRight => ("wheel", "right"),
            _ => return,
        };
        self.nvim.mouse(button, action, "", row, col);
    }
}

/// How the body is divided. The pane is on the right; `None` means there is
/// no pane and the diagram has the lot.
///
/// A body too narrow for both is not split at all -- two useless columns of
/// nothing help nobody -- and the pane, being the one you asked for, takes it.
pub fn panes(body: Rect, pane_width: Option<u16>) -> (Rect, Option<Rect>) {
    let Some(wanted) = pane_width else {
        return (body, None);
    };
    if body.width < GRAPH_MIN + PANE_MIN {
        return (Rect { width: 0, ..body }, Some(body));
    }
    let pane = wanted.clamp(PANE_MIN, body.width - GRAPH_MIN);
    let graph = Rect { width: body.width - pane, ..body };
    (graph, Some(Rect { x: body.x + graph.width, width: pane, ..body }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(width: u16) -> Rect {
        Rect::new(0, 0, width, 20)
    }

    /// The divider is a wish, not a measurement: it is clamped to what the
    /// terminal can spare without being forgotten, so widening a window that
    /// had been squeezed gives back the split the user asked for.
    #[test]
    fn the_split_leaves_both_panes_worth_looking_at() {
        let (graph, pane) = panes(body(100), Some(50));
        assert_eq!((graph.width, pane.unwrap().width), (50, 50));
        assert_eq!(pane.unwrap().x, 50, "the pane is on the right");

        let (graph, pane) = panes(body(100), Some(95));
        assert_eq!(graph.width, GRAPH_MIN, "the diagram keeps a usable strip");
        assert_eq!(pane.unwrap().width, 100 - GRAPH_MIN);

        let (graph, pane) = panes(body(100), Some(2));
        assert_eq!(pane.unwrap().width, PANE_MIN, "so does the pane");
        assert_eq!(graph.width, 100 - PANE_MIN);
    }

    /// Below the width the two of them need, splitting produces two things
    /// too narrow to read instead of one that works.
    #[test]
    fn a_narrow_terminal_gives_the_whole_body_to_one_of_them() {
        let (graph, pane) = panes(body(50), Some(25));
        assert_eq!(pane.unwrap().width, 50, "the pane you opened gets it");
        assert_eq!(graph.width, 0);

        let (graph, pane) = panes(body(50), None);
        assert_eq!((graph.width, pane), (50, None), "and with no pane, the diagram does");
    }
}
