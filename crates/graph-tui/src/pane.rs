//! The right-hand side of the screen: what sits beside the diagram, and how
//! much of the body it takes.
//!
//! The divider lives here rather than on whatever is currently in the pane,
//! because the pane outlives any one thing in it: a pane showing search
//! results is the same pane, the same width, as one showing a file.

use std::path::Path;

use entity_graph::search::Excerpt;
use nvim_ui::{Nvim, Value};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::editor::Editor;

const PANE_MIN: u16 = 40;
const GRAPH_MIN: u16 = 24;

/// A result costs its lines plus the bar naming the file and the status line
/// under it -- nvim draws one window per result, and a window is never just
/// its text.
const CHROME: u16 = 2;

/// Code is written eighty columns wide, and results are code. Mirrors
/// `editor::CODE_COLUMNS`, which is the same decision about the same pane.
const CODE_COLUMNS: u16 = 80;

/// Which side of the pane is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Search,
    Editor,
}

/// What is beside the diagram, if anything.
#[derive(Default)]
pub struct Pane {
    tab: Tab,
    editor: Option<Editor>,
    results: Option<Results>,
    /// The search tab's own Neovim: a window per result, so every result is
    /// shown by the thing that knows how to draw that language.
    nvim: Option<Nvim>,
    /// What the box holds. Not the query that was run -- that is on
    /// `results` -- so an abandoned edit does not rewrite history.
    query: String,
    typing: bool,
    /// The page and area the windows were last built for, so they are built
    /// again when either changes and not on every frame.
    laid: Option<(usize, Rect)>,
    /// Whether the divider has been set from what the results actually need.
    /// Asked once per Neovim, since it is a question about the gutter its
    /// config draws, not about any one result.
    measured: bool,
    /// Columns of the body the pane asks for, before any clamping. Held as a
    /// wish rather than a measurement so a window that grows and shrinks
    /// again comes back to the split the user chose.
    width: u16,
}

impl Pane {
    /// A pane with nothing to show takes no columns: no file open, no search
    /// run, nobody typing one.
    pub fn is_open(&self) -> bool {
        self.editor.is_some() || self.results.is_some() || self.typing
    }

    pub fn tab(&self) -> Tab {
        self.tab
    }

    pub fn show(&mut self, tab: Tab) {
        self.tab = tab;
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn typing(&self) -> bool {
        self.typing
    }

    /// Start typing a query, from whatever the box already held.
    pub fn begin_typing(&mut self) {
        self.tab = Tab::Search;
        self.typing = true;
    }

    /// Stop typing and put back the query the results are actually of, so an
    /// abandoned edit leaves no trace in the box.
    pub fn cancel_typing(&mut self) {
        self.typing = false;
        self.query = self.results.as_ref().map(|r| r.query.clone()).unwrap_or_default();
    }

    pub fn edit_query(&mut self, c: char) {
        self.query.push(c);
    }

    pub fn backspace_query(&mut self) {
        self.query.pop();
    }

    /// Back over the last word, and the run of spaces before it, the way
    /// `ctrl-w` does everywhere else a line is typed.
    pub fn delete_word(&mut self) {
        while self.query.ends_with(' ') {
            self.query.pop();
        }
        while !self.query.is_empty() && !self.query.ends_with(' ') {
            self.query.pop();
        }
    }

    /// Move to the next result or the previous one. They are windows, so
    /// this is `ctrl-w j` and `ctrl-w k` -- and nvim stops at the ends
    /// rather than wrapping, which is what a list should do too.
    pub fn step_result(&mut self, forward: bool) {
        if let Some(nvim) = &self.nvim {
            nvim.input(if forward { "<C-w>j" } else { "<C-w>k" });
        }
    }

    /// Put the cursor in the `nth` result window of the page.
    pub fn focus_result(&mut self, nth: usize) {
        if let Some(nvim) = &self.nvim {
            nvim.input(&format!("{}<C-w>w", nth + 1));
        }
    }

    /// The page moved, so the windows have to be built again.
    pub fn relay_out(&mut self) {
        self.laid = None;
    }

    /// The file and line of the result the cursor is in. Which result that
    /// is comes from nvim -- the cursor lives in one of the windows, and the
    /// window it lives in *is* the selection, so there is no second copy of
    /// it here to disagree.
    pub fn current_result(&self) -> Option<(entity_graph::EntityId, usize)> {
        let results = self.results.as_ref()?;
        let nvim = self.nvim.as_ref()?;
        let win = nvim.eval("winnr()").ok()?.as_u64()? as usize;
        let excerpt = results.excerpts.get(results.page_start() + win.checked_sub(1)?)?;
        let line = nvim
            .call("nvim_win_get_cursor", vec![0.into()])
            .ok()
            .and_then(|at| at.as_array()?.first()?.as_u64())
            .map(|n| (n as usize).saturating_sub(1))
            .unwrap_or(excerpt.first_line);
        Some((excerpt.file, line))
    }

    pub fn results(&self) -> Option<&Results> {
        self.results.as_ref()
    }

    pub fn results_mut(&mut self) -> Option<&mut Results> {
        self.results.as_mut()
    }

    pub fn editor(&self) -> Option<&Editor> {
        self.editor.as_ref()
    }

    pub fn editor_mut(&mut self) -> Option<&mut Editor> {
        self.editor.as_mut()
    }

    pub fn attach(&mut self, editor: Editor, width: u16) {
        self.editor = Some(editor);
        self.width = width;
    }

    pub fn close(&mut self) {
        self.editor = None;
    }

    /// The results Neovim went away. The results themselves survive, so the
    /// pane can still say what was found and open it; only the drawing of
    /// them is lost, and the next search starts a fresh one.
    pub fn drop_search(&mut self) {
        self.nvim = None;
        self.laid = None;
    }

    /// What to hand [`panes`]: `None` when there is nothing to show, which is
    /// the same thing as the diagram having the whole body.
    pub fn width(&self) -> Option<u16> {
        self.is_open().then_some(self.width)
    }

    pub fn set_width(&mut self, width: u16) {
        self.width = width;
    }

    pub fn widen(&mut self, by: i32) {
        self.width = (i32::from(self.width) + by).clamp(0, i32::from(u16::MAX)) as u16;
    }

    /// Hand the pane a search to show. The Neovim that draws it is started
    /// here and kept: results arrive a query at a time, and paying nvim's
    /// startup on every one of them would be felt.
    pub fn attach_results(
        &mut self,
        query: String,
        excerpts: Vec<Excerpt>,
        root: &Path,
        area: Rect,
        args: &[&str],
    ) -> Option<std::sync::mpsc::Receiver<nvim_ui::Event>> {
        self.tab = Tab::Search;
        self.typing = false;
        self.query = query.clone();
        self.results = Some(Results::new(query, excerpts));
        self.laid = None;
        if self.nvim.is_some() {
            return None;
        }
        let body = self.body(area);
        match Nvim::spawn(root, (body.width.max(1), body.height.max(1)), args) {
            Ok((nvim, events)) => {
                self.nvim = Some(nvim);
                Some(events)
            }
            Err(_) => None,
        }
    }

    /// Where the tab's own content goes: below the tab bar, and below the
    /// box as well when the box is showing.
    pub fn body(&self, area: Rect) -> Rect {
        let chrome = match self.tab {
            Tab::Search => 2,
            Tab::Editor => 1,
        };
        Rect {
            y: area.y.saturating_add(chrome),
            height: area.height.saturating_sub(chrome),
            ..area
        }
    }

    /// Build the windows, when the page or the space for it has changed.
    /// Separate from drawing because nvim redraws its whole screen for a
    /// resize, and doing that every frame would have it doing nothing else.
    pub fn render(&mut self, root: &Path, area: Rect) {
        if self.tab != Tab::Search {
            return;
        }
        let body = self.body(area);
        let (Some(nvim), Some(results)) = (&self.nvim, &self.results) else { return };
        let key = (results.page_start(), body);
        if self.laid == Some(key) {
            return;
        }
        nvim.resize(body.width.max(1), body.height.max(1));
        lay_out(nvim, root, results, body.height);
        self.laid = Some(key);
        if !self.measured {
            // The same width a file gets, because these are files. Asked of
            // nvim once the windows exist, since that is when it knows what
            // gutter this config draws before the code.
            let gutter = nvim
                .eval("getwininfo(win_getid())[0].textoff")
                .ok()
                .and_then(|v| v.as_u64())
                .and_then(|n| u16::try_from(n).ok())
                .unwrap_or(0);
            self.width = CODE_COLUMNS.saturating_add(gutter);
            self.measured = true;
            self.laid = None;
        }
    }

    pub fn draw(&self, buf: &mut Buffer, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.draw_bar(buf, area);
        let body = self.body(area);
        match self.tab {
            Tab::Search => {
                self.draw_box(buf, Rect { y: area.y + 1, height: 1, ..area }, body.height);
                if let Some(nvim) = &self.nvim {
                    nvim.draw(buf, body);
                }
            }
            Tab::Editor => {
                if let Some(editor) = &self.editor {
                    editor.draw(buf, body);
                }
            }
        }
    }

    fn draw_bar(&self, buf: &mut Buffer, area: Rect) {
        let on = Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD);
        let off = Style::default().fg(Color::DarkGray);
        let mut x = area.x;
        for (tab, label) in [(Tab::Search, " search "), (Tab::Editor, " nvim ")] {
            let style = if tab == self.tab { on } else { off };
            for ch in label.chars() {
                if x >= area.right() {
                    return;
                }
                buf[(x, area.y)].set_char(ch).set_style(style);
                x += 1;
            }
        }
        while x < area.right() {
            buf[(x, area.y)].set_char(' ').set_style(off);
            x += 1;
        }
    }

    fn draw_box(&self, buf: &mut Buffer, area: Rect, body_height: u16) {
        let count = self
            .results
            .as_ref()
            .map(|r| {
                let (first, last, all) = r.counted(body_height);
                if all == 0 { " no matches".to_string() } else { format!(" {first}-{last} of {all}") }
            })
            .unwrap_or_default();
        let line = format!("/{}{}", self.query, if self.typing { "_" } else { "" });
        let style = if self.typing {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let mut x = area.x;
        for ch in line.chars() {
            if x >= area.right() {
                break;
            }
            buf[(x, area.y)].set_char(ch).set_style(style);
            x += 1;
        }
        let at = area.right().saturating_sub(count.chars().count() as u16);
        let mut x = x.max(at);
        for ch in count.chars() {
            if x >= area.right() {
                break;
            }
            buf[(x, area.y)].set_char(ch).set_style(Style::default().fg(Color::DarkGray));
            x += 1;
        }
    }
}

/// No tildes past the end of this window's buffer: down an empty pane they
/// read as a file that failed to open. Window-local, and only after the
/// buffer exists, because `fillchars` is both.
fn blank_below(nvim: &Nvim) {
    let _ = nvim.call(
        "nvim_set_option_value",
        vec!["fillchars".into(), "eob: ".into(), Value::Map(vec![("scope".into(), "local".into())])],
    );
}

/// One window per result on this page, each showing the real file at the
/// match. A real buffer brings its own filetype, so every result is coloured
/// by whatever draws that language -- which is the whole reason nvim draws
/// this list rather than cerebro.
fn lay_out(nvim: &Nvim, root: &Path, results: &Results, height: u16) {
    let exec = |s: &str| {
        let _ = nvim.call("nvim_exec2", vec![s.into(), Value::Map(vec![])]);
    };
    exec("silent! only");
    // `noequalalways` or nvim redivides the space whenever a window is added,
    // and the heights set below would not survive the next split.
    // `laststatus=2` so every window has a status line and each result costs
    // the same rows -- with 0, the last window has none and the row map is
    // wrong for exactly one result.
    // `scrolloff` globally as well as per window: the user's own value is
    // what decides where a cursor move leaves the top of the window, and a
    // results list that is two lines off is one that shows the wrong lines.
    exec("set noequalalways splitbelow laststatus=2 winminheight=0 scrolloff=0");

    let page = results.page(height);
    if page.is_empty() {
        // Nothing found, so nothing to show -- not the last file that was,
        // and not an empty buffer's tildes either.
        exec("silent! enew | setlocal buftype=nofile bufhidden=wipe nonumber winbar=");
        blank_below(nvim);
        return;
    }
    let mut wins: Vec<Value> = Vec::new();
    for (n, i) in page.clone().enumerate() {
        let e = &results.excerpts[i];
        if n > 0 {
            exec("botright split");
        }
        let path = root.join(&e.path);
        let _ = nvim.call(
            "nvim_cmd",
            vec![
                Value::Map(vec![
                    ("cmd".into(), "edit".into()),
                    ("args".into(), Value::Array(vec![path.to_string_lossy().as_ref().into()])),
                ]),
                Value::Map(vec![]),
            ],
        );
        // Folds and scrolloff both move lines away from where they were put,
        // which is what makes a result stop matching the rows it was given.
        exec("setlocal scrolloff=0 nowrap nofoldenable number");
        // The status line is the gap between one result and the next, and
        // nothing else: nvim's default puts the file's absolute path there,
        // which the bar above the window has already said, relatively.
        let _ = nvim.call(
            "nvim_set_option_value",
            vec![
                "statusline".into(),
                " ".into(),
                Value::Map(vec![("scope".into(), "local".into())]),
            ],
        );
        // Asked for as each window is made. `nvim_list_wins` gives no order
        // worth relying on, and a list in the wrong order silently gives one
        // result another's size and scroll position.
        if let Ok(w) = nvim.call("nvim_get_current_win", vec![]) {
            wins.push(w);
        }
        let at = e.matched.first().map_or(e.first_line, |m| m.line) + 1;
        let _ = nvim.call(
            "nvim_set_option_value",
            vec![
                "winbar".into(),
                format!("%#Directory#{}:{}", e.path, at).into(),
                Value::Map(vec![("scope".into(), "local".into())]),
            ],
        );
    }

    // A window below everything, to hold the slack. Vim windows tile the
    // whole pane, so rows a result gives up are taken by its neighbour: with
    // two results in a tall pane, shrinking the second one grew the first
    // until it showed forty lines of a one-line match. The filler is the
    // neighbour instead.
    exec("botright split");
    exec("silent! enew | setlocal buftype=nofile bufhidden=wipe nonumber winbar= statusline=\\ ");
    blank_below(nvim);

    // Heights and scrolling last, and in that order: a split takes its space
    // from the window above, so a height set before the next split is a
    // height the next split spends -- and scrolling a window that is about to
    // be resized puts the wrong lines at the top of it.
    for (w, i) in wins.iter().zip(page.clone()) {
        // One more than the lines, because the winbar is drawn inside the
        // window and takes a row of it. The status line is not, which is why
        // a result costs `CHROME` rows rather than one.
        let rows = u16::try_from(results.excerpts[i].text.len()).unwrap_or(u16::MAX).max(1) + 1;
        let _ = nvim.call("nvim_win_set_height", vec![w.clone(), (rows as i64).into()]);
    }
    for (w, i) in wins.iter().zip(page) {
        let e = &results.excerpts[i];
        let (line, col) = e.matched.first().map_or((e.first_line, 0), |m| (m.line, m.start));
        // Made current, then `winrestview`, and both parts matter. Setting a
        // background window's cursor moves the cursor and leaves the view
        // where it was, and `win_execute` on another window runs the call
        // and changes nothing -- measured, both of them. Only the window you
        // are in can be told where its top line is.
        let _ = nvim.call("nvim_set_current_win", vec![w.clone()]);
        let _ = nvim.call(
            "nvim_exec2",
            vec![
                format!(
                    "call winrestview({{'topline': {}, 'lnum': {}, 'col': {}, 'leftcol': 0}})",
                    e.first_line + 1,
                    line + 1,
                    col,
                )
                .into(),
                Value::Map(vec![]),
            ],
        );
    }
    // Back to the top, so the first result is the one selected.
    if let Some(first) = wins.first() {
        let _ = nvim.call("nvim_set_current_win", vec![first.clone()]);
    }
}

/// One search's worth of answer, and which part of it is on screen.
///
/// Nvim shows a result per window, and windows tile rather than scroll: what
/// does not fit is not below the fold, it is not drawn at all. So the list
/// pages, and this is what decides where a page starts and ends.
pub struct Results {
    pub query: String,
    pub excerpts: Vec<Excerpt>,
    /// The first result of the page on screen.
    from: usize,
}

impl Results {
    pub fn new(query: String, excerpts: Vec<Excerpt>) -> Self {
        Results { query, excerpts, from: 0 }
    }

    /// The results that fit in `height` rows, starting from the current page.
    /// Always at least one, however tall it is: a result too big for the pane
    /// is better shown cut off than silently dropped.
    pub fn page(&self, height: u16) -> std::ops::Range<usize> {
        let mut used = 0;
        let mut end = self.from;
        while end < self.excerpts.len() {
            let cost = self.rows(end);
            if used + cost > height && end > self.from {
                break;
            }
            used += cost;
            end += 1;
        }
        self.from..end
    }

    pub fn page_start(&self) -> usize {
        self.from
    }

    fn rows(&self, i: usize) -> u16 {
        u16::try_from(self.excerpts[i].text.len()).unwrap_or(u16::MAX).saturating_add(CHROME)
    }

    /// Show the next page, or the previous one; `false` when there is no such
    /// page and nothing moved.
    pub fn turn(&mut self, forward: bool, height: u16) -> bool {
        if forward {
            let end = self.page(height).end;
            if end >= self.excerpts.len() {
                return false;
            }
            self.from = end;
            return true;
        }
        if self.from == 0 {
            return false;
        }
        // Walk back by what fits, so paging back lands where paging forward
        // came from rather than on an arbitrary boundary.
        let (mut used, mut first) = (0, self.from);
        while first > 0 {
            let cost = self.rows(first - 1);
            if used + cost > height {
                break;
            }
            used += cost;
            first -= 1;
        }
        self.from = first;
        true
    }

    /// Which result is drawn at `row`, counting from the top of the results
    /// area. `None` between the page's end and the bottom of the pane.
    pub fn at_row(&self, row: u16, height: u16) -> Option<usize> {
        let mut top = 0;
        for i in self.page(height) {
            let next = top + self.rows(i);
            if row < next {
                return Some(i);
            }
            top = next;
        }
        None
    }

    /// `first shown, last shown, total`, one-based, for the box row.
    pub fn counted(&self, height: u16) -> (usize, usize, usize) {
        let page = self.page(height);
        (page.start + 1, page.end, self.excerpts.len())
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
    use entity_graph::EntityId;
    use entity_graph::search::Mark;

    fn body(width: u16) -> Rect {
        Rect::new(0, 0, width, 20)
    }

    /// Results of `lines` text lines each, in order.
    fn results(lines: &[usize]) -> Results {
        let excerpts = lines
            .iter()
            .enumerate()
            .map(|(i, &n)| Excerpt {
                file: EntityId(i),
                path: format!("{i}.rs"),
                first_line: 0,
                text: vec!["x".to_string(); n],
                matched: vec![Mark { line: 0, start: 0, end: 1 }],
            })
            .collect();
        Results::new("needle".into(), excerpts)
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
        assert_eq!(graph.width, 50, "with no pane the diagram has the body");
        assert!(pane.is_none());
    }

    /// A pane with nothing in it asks for no columns at all, however wide it
    /// was last time -- which is what `panes` reads as "there is no pane".
    #[test]
    fn a_pane_with_nothing_in_it_takes_no_columns() {
        let mut pane = Pane::default();
        assert_eq!(pane.width(), None);
        pane.set_width(60);
        assert_eq!(pane.width(), None, "a width is not a reason to draw a pane");
        assert_eq!(panes(body(100), pane.width()).0.width, 100);
    }

    /// Typing a query is itself a reason for the pane to be there: the box
    /// has to be visible before there is anything to show in it.
    #[test]
    fn a_query_being_typed_is_enough_to_open_the_pane() {
        let mut pane = Pane::default();
        pane.set_width(60);
        pane.begin_typing();
        assert!(pane.is_open());
        assert_eq!(pane.tab(), Tab::Search);
        pane.edit_query('a');
        pane.edit_query('b');
        pane.edit_query(' ');
        pane.edit_query('c');
        pane.delete_word();
        assert_eq!(pane.query(), "ab ", "the space the word sat after is not the word");
        pane.cancel_typing();
        assert!(!pane.is_open(), "an abandoned query leaves nothing behind");
        assert_eq!(pane.query(), "");
    }

    /// Windows tile, so a page is whatever fits whole. Paging forward and
    /// back again lands where it started rather than drifting, and the last
    /// page stops instead of running off the end.
    #[test]
    fn a_page_is_what_fits_and_paging_back_undoes_paging_forward() {
        // Five results of five rows each (three lines plus the file bar and
        // the status line) in a pane twelve rows tall: two fit.
        let mut r = results(&[3, 3, 3, 3, 3]);
        assert_eq!(r.page(12), 0..2);
        assert_eq!(r.counted(12), (1, 2, 5));

        assert!(r.turn(true, 12));
        assert_eq!(r.page(12), 2..4);
        assert!(r.turn(true, 12));
        assert_eq!(r.page(12), 4..5, "the last page is short, not wrapped");
        assert!(!r.turn(true, 12), "there is nothing after the end");

        assert!(r.turn(false, 12));
        assert_eq!(r.page(12), 2..4, "back is where forward came from");
        assert!(r.turn(false, 12));
        assert_eq!(r.page(12), 0..2);
        assert!(!r.turn(false, 12));
    }

    /// A result taller than the pane is still shown -- cut off beats absent,
    /// since the alternative is a search that silently finds nothing.
    #[test]
    fn a_result_too_tall_for_the_pane_is_still_a_page() {
        let r = results(&[40, 3]);
        assert_eq!(r.page(10), 0..1);
    }

    /// The row map, which is what a click lands on. Boundaries both ways:
    /// the last row of one result and the first of the next.
    #[test]
    fn the_row_you_click_names_the_result_drawn_there() {
        let r = results(&[3, 1, 3]);
        // Five rows for the first result (bar, three lines, status), three
        // for the second, five for the third: 0-4, 5-7, 8-12.
        assert_eq!(r.at_row(0, 20), Some(0));
        assert_eq!(r.at_row(4, 20), Some(0), "the status line belongs to its own result");
        assert_eq!(r.at_row(5, 20), Some(1), "the next result starts on the next row");
        assert_eq!(r.at_row(7, 20), Some(1), "even a one-line result costs three rows");
        assert_eq!(r.at_row(8, 20), Some(2));
        assert_eq!(r.at_row(12, 20), Some(2));
        assert_eq!(r.at_row(13, 20), None, "below the last result is nothing");

        // On a later page the rows start over at the top of the pane: two
        // five-row results fill a ten-row pane, so page two begins at the
        // third and draws it against row zero.
        let mut r = results(&[3, 3, 3]);
        assert!(r.turn(true, 10));
        assert_eq!(r.at_row(0, 10), Some(2), "the page starts at the top, whatever its number");
    }
}
