//! A Neovim, drawn into a ratatui `Buffer`.
//!
//! `nvim --embed --headless` is a Neovim with no terminal of its own. It
//! talks msgpack-rpc over its pipes, and once you call `nvim_ui_attach` it
//! describes its screen to you as a stream of grid edits -- which is already
//! the shape of a ratatui buffer. So a terminal program can show a real
//! Neovim, with the user's own config and colours, without a pty, without a
//! terminal emulator, and without fighting anyone for raw mode.
//!
//! ```no_run
//! # fn main() -> anyhow::Result<()> {
//! use nvim_ui::{Event, Nvim};
//!
//! let (nvim, events) = Nvim::spawn(std::path::Path::new("."), (80, 24), &[])?;
//! nvim.input("ihello<Esc>");
//! for event in events {
//!     match event {
//!         Event::Redraw => { /* nvim.draw(frame.buffer_mut(), area) */ }
//!         Event::Exited => break,
//!     }
//! }
//! # Ok(()) }
//! ```
//!
//! **`--headless` is not optional.** With `--embed` alone nvim holds its
//! startup until a UI attaches, so anything sent straight after the attach
//! runs before the filetype autocmds exist: files open with no filetype, no
//! syntax and no colour, and nothing says why.

mod grid;
mod keys;
mod rpc;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
pub use grid::{Mode, Viewport};
/// Re-exported so a caller can name what `call` hands back without taking a
/// msgpack dependency of its own.
pub use rmpv::Value;

/// Why a caller is being woken up.
pub enum Event {
    /// Nvim has finished a redraw. Whatever is on screen is now stale.
    Redraw,
    /// Nvim is gone -- `:qa`, a crash, or a kill. Nothing else will arrive.
    Exited,
}

pub struct Nvim {
    client: Arc<rpc::Client>,
    grid: Arc<Mutex<grid::Grid>>,
    child: Child,
}

impl Nvim {
    /// Start a Neovim of `size` cells with `cwd` as its working directory.
    /// `args` is passed through to the command, which is how a test asks for
    /// `--clean`; a caller that wants the user's own Neovim passes nothing.
    pub fn spawn(cwd: &Path, size: (u16, u16), args: &[&str]) -> Result<(Nvim, Receiver<Event>)> {
        let mut child = Command::new("nvim")
            .args(["--embed", "--headless"])
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Nvim's own diagnostics would land on top of whatever the
            // caller has drawn on the real terminal.
            .stderr(Stdio::null())
            .spawn()
            .context("starting nvim (is it on PATH?)")?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let (client, notifications) = rpc::Client::start(stdin, stdout);

        let grid = Arc::new(Mutex::new(grid::Grid::default()));
        let (tx, rx) = channel();
        let applying = Arc::clone(&grid);
        std::thread::spawn(move || {
            // The grid is kept up to date whether or not anyone still wants to
            // be told about it, so a caller that polls the screen instead of
            // waiting on events reads something current rather than the last
            // frame before it stopped listening.
            let mut listening = true;
            for notification in notifications {
                if notification.method != "redraw" {
                    continue;
                }
                let mut grid = applying.lock().expect("grid");
                let mut flushed = false;
                for event in notification.params.iter().filter_map(Value::as_array) {
                    let Some(name) = event.first().and_then(Value::as_str) else { continue };
                    // One event name carries many tuples: `grid_line` for a
                    // whole screen arrives as a single entry.
                    for args in event[1..].iter().filter_map(Value::as_array) {
                        flushed |= grid.apply(name, args);
                    }
                    // `flush` has no arguments, so it has no tuples either.
                    flushed |= name == "flush";
                }
                drop(grid);
                if flushed && listening {
                    listening = tx.send(Event::Redraw).is_ok();
                }
            }
            let _ = tx.send(Event::Exited);
        });

        let nvim = Nvim { client, grid, child };
        nvim.client
            .call(
                "nvim_ui_attach",
                vec![
                    size.0.into(),
                    size.1.into(),
                    Value::Map(vec![
                        ("ext_linegrid".into(), true.into()),
                        ("rgb".into(), true.into()),
                    ]),
                ],
            )
            .context("attaching to nvim as a UI")?;
        Ok((nvim, rx))
    }

    pub fn resize(&self, width: u16, height: u16) {
        let _ = self.client.send("nvim_ui_try_resize", vec![width.into(), height.into()]);
    }

    /// Type at nvim, in its own notation: `"ihello<Esc>"`, `"<C-w>"`.
    pub fn input(&self, keys: &str) {
        let _ = self.client.send("nvim_input", vec![keys.into()]);
    }

    /// Type one key press at nvim. Presses it has no name for are dropped
    /// rather than guessed at.
    pub fn key(&self, key: ratatui::crossterm::event::KeyEvent) {
        if let Some(keys) = keys::notation(key) {
            self.input(&keys);
        }
    }

    /// `button` is one of left/right/middle/wheel/move, `action` press/drag/
    /// release for a button or up/down/left/right for the wheel, and
    /// `modifiers` a string like `"c"` or `"cs"`. Row and column are cells
    /// within nvim's own grid, counted from zero.
    pub fn mouse(&self, button: &str, action: &str, modifiers: &str, row: u16, col: u16) {
        let _ = self.client.send(
            "nvim_input_mouse",
            vec![
                button.into(),
                action.into(),
                modifiers.into(),
                0.into(),
                row.into(),
                col.into(),
            ],
        );
    }

    /// Evaluate a vimscript expression and wait for the answer. The short
    /// way to ask nvim about itself -- `eval("winnr()")`, `eval("&filetype")`
    /// -- without the caller having to know what msgpack is.
    pub fn eval(&self, expression: &str) -> Result<Value> {
        self.call("nvim_eval", vec![expression.into()])
    }

    /// Any API method, for the ones `eval` cannot reach.
    pub fn call(&self, method: &str, params: Vec<Value>) -> Result<Value> {
        self.client.call(method, params)
    }

    pub fn draw(&self, buf: &mut Buffer, area: Rect) {
        self.grid.lock().expect("grid").draw(buf, area);
    }

    /// Where nvim's cursor is, in cells from the top left of its grid.
    pub fn cursor(&self) -> (u16, u16) {
        self.grid.lock().expect("grid").cursor()
    }

    pub fn mode(&self) -> Mode {
        self.grid.lock().expect("grid").mode()
    }

    /// What nvim's window is showing, as of its last redraw. The cursor's
    /// line and column arrive here without being asked for, which is what
    /// lets something else follow along as the user moves around a file.
    pub fn viewport(&self) -> Viewport {
        self.grid.lock().expect("grid").viewport()
    }

    /// One row of the screen as text. For reading what nvim is showing --
    /// tests, and anything that needs the screen rather than a buffer.
    pub fn row(&self, y: u16) -> String {
        self.grid.lock().expect("grid").row(y)
    }
}

impl Drop for Nvim {
    fn drop(&mut self) {
        // Killed, not asked to quit: `:qa` on a modified buffer stops to ask
        // a question nobody is there to answer. Deciding what to do about
        // unsaved work belongs to whoever put the user in front of an editor.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
