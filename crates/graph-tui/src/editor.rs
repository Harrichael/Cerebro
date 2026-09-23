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

use nvim_ui::{Mode, Nvim, Value};

/// Neither pane is worth having below these. A body too narrow for both is
/// given entirely to whichever one the user is looking at.
/// Columns one `ctrl-w <` or `ctrl-w >` moves the divider.
const RESIZE_STEP: i32 = 4;
/// Columns of code a pane opens with. Eighty is the width code is written
/// to; a pane wider than that shows margin the diagram could have had.
const CODE_COLUMNS: u16 = 80;
/// What nvim calls back with after `:w`.
pub const WROTE: &str = "cerebro_wrote";

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
    /// `ctrl-w d`: go to whatever the word under the cursor refers to.
    GoToDefinition,
}

pub struct Editor {
    nvim: Nvim,
    /// The extmark namespace the range tint is drawn in, so showing a new
    /// entity can clear the last one without touching anyone else's marks.
    marks: i64,
    /// A `ctrl-w` is waiting for the key that says what it meant.
    pending: bool,
}

impl Editor {
    /// `args` reaches the `nvim` command. Empty is the point of the whole
    /// exercise -- the user's own editor, their config, their colours -- and
    /// a test passes `--clean` so it is not at the mercy of them.
    pub fn open(
        root: &Path,
        area: Rect,
        args: &[&str],
    ) -> Result<(Editor, Receiver<nvim_ui::Event>)> {
        let (nvim, events) = Nvim::spawn(root, (area.width.max(1), area.height.max(1)), args)?;
        let marks = nvim
            .call("nvim_create_namespace", vec!["cerebro".into()])
            .ok()
            .and_then(|n| n.as_i64())
            .unwrap_or(0);
        // A group of its own, linked to something the colourscheme already
        // sets, so the tint suits whatever the user is using and they can
        // still override this one thing without touching CursorLine.
        let _ = nvim.call(
            "nvim_set_hl",
            vec![
                0.into(),
                "CerebroRange".into(),
                Value::Map(vec![("link".into(), "CursorLine".into()), ("default".into(), true.into())]),
            ],
        );
        // Nvim says when a file is written; the alternative is watching the
        // filesystem for edits we already know about.
        if let Ok(channel) = nvim.channel() {
            let _ = nvim.call(
                "nvim_create_autocmd",
                vec![
                    "BufWritePost".into(),
                    Value::Map(vec![(
                        "command".into(),
                        format!("call rpcnotify({channel}, '{WROTE}', expand('<afile>:p'))").into(),
                    )]),
                ],
            );
        }
        Ok((Editor { nvim, marks, pending: false }, events))
    }

    /// Put `file` on screen at `line`, with `range` tinted as the extent of
    /// whatever the diagram has selected.
    ///
    /// Does not take focus: showing you where something is, is not the same
    /// as asking you to go and edit it.
    pub fn show(&self, file: &Path, line: usize, range: Option<std::ops::Range<usize>>) {
        // Through `nvim_cmd` with the path as an argument rather than
        // `:edit <path>` as text: a path is not vim syntax, and one with a
        // space or a `%` in it would be read as something else entirely.
        let edited = self.nvim.call(
            "nvim_cmd",
            vec![
                Value::Map(vec![
                    ("cmd".into(), "edit".into()),
                    ("args".into(), Value::Array(vec![file.to_string_lossy().as_ref().into()])),
                ]),
                Value::Map(vec![]),
            ],
        );
        if edited.is_err() {
            return;
        }
        // Nvim counts rows from one and columns from zero, in the same call.
        let _ = self.nvim.call(
            "nvim_win_set_cursor",
            vec![0.into(), Value::Array(vec![(line as i64 + 1).into(), 0.into()])],
        );
        let _ = self.nvim.call("nvim_command", vec!["normal! zz".into()]);
        self.tint(range);
    }

    fn tint(&self, range: Option<std::ops::Range<usize>>) {
        let _ = self.nvim.call(
            "nvim_buf_clear_namespace",
            vec![0.into(), self.marks.into(), 0.into(), (-1).into()],
        );
        let Some(range) = range else { return };
        // `line_range` is inclusive at both ends; an extmark's is not.
        let _ = self.nvim.call(
            "nvim_buf_set_extmark",
            vec![
                0.into(),
                self.marks.into(),
                (range.start as i64).into(),
                0.into(),
                Value::Map(vec![
                    ("end_row".into(), (range.end as i64 + 1).into()),
                    ("hl_group".into(), "CerebroRange".into()),
                    ("hl_eol".into(), true.into()),
                ]),
            ],
        );
    }

    pub fn nvim(&self) -> &Nvim {
        &self.nvim
    }

    /// The width the pane would choose for itself: eighty columns of code
    /// plus whatever nvim draws before them -- line numbers, signs, folds --
    /// or `most` when the screen cannot spare that. Asked of nvim rather than
    /// worked out from its options, because the user's config sets those and
    /// may set them differently per file type.
    pub fn natural_width(&self, most: u16) -> u16 {
        let gutter = self
            .nvim
            .eval("getwininfo(win_getid())[0].textoff")
            .ok()
            .and_then(|v| v.as_u64())
            .and_then(|n| u16::try_from(n).ok())
            .unwrap_or(0);
        CODE_COLUMNS.saturating_add(gutter).min(most)
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
            // Nvim's own `ctrl-w d` jumps to the definition of the word under
            // the cursor, which is what this does; it just answers with the
            // graph rather than with a tags file.
            KeyCode::Char('d') => Handled::GoToDefinition,
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

    /// The file the pane is showing, absolute, or `None` for a buffer with no
    /// file behind it.
    pub fn current_file(&self) -> Option<std::path::PathBuf> {
        let name = self.nvim.eval("expand('%:p')").ok()?;
        let name = name.as_str().filter(|n| !n.is_empty())?;
        Some(std::path::PathBuf::from(name))
    }

    /// Where the cursor is in the file: the line (0-indexed), the byte
    /// column within it, and the text of that line.
    pub fn cursor_site(&self) -> Option<(usize, usize, String)> {
        let at = self.nvim.call("nvim_win_get_cursor", vec![0.into()]).ok()?;
        let at = at.as_array()?;
        // Nvim counts these rows from one and these columns from zero.
        let line = (at.first()?.as_u64()? as usize).checked_sub(1)?;
        let column = at.get(1)?.as_u64()? as usize;
        let text = self.nvim.call("nvim_get_current_line", vec![]).ok()?;
        Some((line, column, text.as_str()?.to_owned()))
    }

    pub fn mouse(&self, kind: MouseEventKind, column: u16, row: u16, area: Rect) {
        let (col, row) = (column.saturating_sub(area.x), row.saturating_sub(area.y));
        let (button, action) = match kind {
            MouseEventKind::Down(MouseButton::Left) => ("left", "press"),
            MouseEventKind::Down(MouseButton::Right) => ("right", "press"),
            MouseEventKind::Down(MouseButton::Middle) => ("middle", "press"),
            MouseEventKind::Drag(MouseButton::Left) => ("left", "drag"),
            MouseEventKind::Up(MouseButton::Left) => ("left", "release"),
            MouseEventKind::ScrollUp => ("wheel", "up"),
            MouseEventKind::ScrollDown => ("wheel", "down"),
            MouseEventKind::ScrollLeft => ("wheel", "left"),
            MouseEventKind::ScrollRight => ("wheel", "right"),
            _ => return,
        };
        self.nvim.mouse(button, action, "", row, col);
    }
}
