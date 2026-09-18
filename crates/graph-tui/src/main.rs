//! Terminal 2D graph view of a code entity graph.
//!
//! The diagram is rendered once per change to a buffer its own size and the
//! visible window blitted from it, so scrolling never re-routes an edge.
//! Height is the axis that grows as the graph is expanded — width wraps
//! against the viewport — which is why panning is vertical first.

use anyhow::{Context, Result};
use entity_graph::{EntityGraph, EntityId};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use graph_tui::camera::Camera;
use graph_tui::controls::{self, Control, NodeAction, State};
use graph_tui::editor::{self, Editor, Handled};
use graph_tui::label::Labels;
use graph_tui::placer::{self, Diagram};
use graph_tui::render::{self, Stats};
use graph_tui::view::{self, Settings};
use graph_tui::zoom::Zoom;

const HINT: &str =
    "? keys  ·  scroll pans, ctrl-scroll zooms  ·  ↵ expand  ·  ⌫ collapse  ·  o editor pane  ·  q quit";
/// The same line, while the pane has the keyboard: every other key on the
/// board belongs to nvim, so listing them would be a lie.
const PANE_HINT: &str = "ctrl-w h  back to the diagram  ·  ctrl-w < >  move the divider  ·                           everything else goes to nvim";

/// Wheel notches are small and terminals are large, so one notch moves more
/// than one cell. Sideways moves further because columns are narrower than
/// rows are tall.
const WHEEL_Y: i32 = 3;
const WHEEL_X: i32 = 6;

/// The status line has room for a handful of keys, which left most of these
/// undiscoverable. Everything that does something is listed here -- except the
/// switches, which say their own keys in the corner of the screen.
const KEYS: &[(&str, &str)] = &[
    ("↑ ↓ ← →  /  k j h l", "scroll"),
    ("PgUp PgDn  /  space", "scroll a half screen"),
    ("Home End  /  g G", "back to the start, or the bottom"),
    ("tab ⇧tab  /  n p", "select the next or previous node"),
    ("click", "select a node, or press a button on its frame"),
    ("scroll  ⇧scroll", "pan up and down, or left and right"),
    ("c", "centre the diagram (same as Home)"),
    ("↵  /  +", "expand the selected node into its children"),
    ("⌫  /  -", "collapse it back into its parent"),
    ("ctrl-scroll  /  z", "zoom: draw the same graph larger or smaller"),
    ("o  /  ctrl-w l", "open the editor pane, and go to it"),
    ("ctrl-w h", "from the pane, back to the diagram"),
    ("ctrl-w < >", "move the divider between them"),
    ("ctrl-w d", "in the pane: select what the word under the cursor refers to"),
    ("?", "this list"),
    ("q  /  esc  /  ctrl-c", "quit"),
];

/// Everything the loop waits on. Two producers -- the terminal and, once it
/// is open, the pane -- because nvim redraws when it is ready rather than
/// when a key is pressed, and a loop blocked on the keyboard would show a
/// screen from before the last thing nvim did.
enum Input {
    Term(Event),
    Pane(nvim_ui::Event),
}

struct App {
    graph: EntityGraph,
    cursor: coalesce::Cursor,
    settings: Settings,
    /// How large the same graph is drawn, which is a different question from
    /// how much of the graph is expanded.
    zoom: Zoom,
    diagram: Diagram,
    canvas: Buffer,
    stats: Stats,
    camera: Camera,
    /// Always a drawn leaf, never a box; see `rebuild`.
    selected: Option<EntityId>,
    /// Edges are laid out either way; this only decides whether they are
    /// drawn. Turning them off to read the names should not move the names.
    show_edges: bool,
    /// Where the switches landed last frame, so a click can find them.
    panel: controls::Panel,
    help: bool,
    quit: bool,

    /// The tree the diagram is of, for the pane to open files out of.
    root: std::path::PathBuf,
    /// Both panes' share of the screen, as of the last frame.
    body: Rect,
    /// Where the pane landed last frame, so a click can be aimed at it.
    pane_area: Rect,
    /// The size nvim was last told about, so it is only told again on a change.
    pane_was: Rect,
    /// Where the pane's cursor was when it was last followed. A redraw that
    /// left it where it was -- most of them, while typing -- is not worth
    /// asking nvim anything about.
    pane_cursor: (std::path::PathBuf, usize),
    editor: Option<Editor>,
    focus: Focus,
    /// A `ctrl-w` on the diagram side, waiting for the key that says what it
    /// meant.
    pending_window: bool,
    /// What went wrong that the user needs telling about -- no nvim on PATH,
    /// unsaved work standing in the way of a quit. Cleared by the next thing
    /// that works.
    trouble: Option<String>,
    /// Handed to the thread that pumps the pane's redraws into the loop.
    inputs: std::sync::mpsc::Sender<Input>,
    /// Extra arguments for the pane's `nvim`. Nothing, in a real run: the
    /// whole point is that it is the user's own editor.
    nvim_args: &'static [&'static str],
}

/// Who has the keyboard. Nvim wants every key on the board, so this is the
/// whole of the arbitration: whoever is in focus gets all of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Graph,
    Pane,
}

impl App {
    fn new(
        graph: EntityGraph,
        viewport: Rect,
        root: std::path::PathBuf,
        inputs: std::sync::mpsc::Sender<Input>,
    ) -> Self {
        let cursor = coalesce::Cursor::new(&graph);
        let mut app = App {
            graph,
            cursor,
            settings: Settings::default(),
            zoom: Zoom::Close,
            diagram: Diagram {
                nodes: Vec::new(),
                edges: Vec::new(),
                width: 0,
                height: 0,
                zoom: Zoom::Close,
            },
            canvas: Buffer::empty(Rect::new(0, 0, 1, 1)),
            stats: Stats { submitted: 0, unroutable: 0 },
            camera: Camera::new(viewport, (0, 0)),
            selected: None,
            show_edges: true,
            panel: controls::Panel::default(),
            help: false,
            quit: false,
            root,
            body: viewport,
            pane_area: Rect::ZERO,
            pane_was: Rect::ZERO,
            pane_cursor: (std::path::PathBuf::new(), usize::MAX),
            editor: None,
            focus: Focus::Graph,
            pending_window: false,
            trouble: None,
            inputs,
            nvim_args: &[],
        };
        app.rebuild();
        app
    }

    /// Where the selection goes when the node holding it stops being a leaf.
    ///
    /// Almost always that is because the user just expanded it, and the thing
    /// they are now looking at is what came out of it -- so the first of its
    /// children in reading order, not the first node in the diagram. Falling
    /// back to the diagram's first leaf sent every expand back to the top
    /// left, which is nowhere near what was expanded.
    fn inherit_selection(&self) -> Option<EntityId> {
        let mut leaves: Vec<(EntityId, Rect)> =
            self.diagram.nodes.iter().filter(|n| !n.is_box).map(|n| (n.id, n.rect)).collect();
        leaves.sort_by_key(|(_, r)| (r.y, r.x));
        self.selected
            .and_then(|was| {
                leaves.iter().find(|(id, _)| is_under(&self.graph, *id, was)).map(|(id, _)| *id)
            })
            // Hidden, scoped away, or filtered out: there is nothing of it
            // left to inherit, so start over.
            .or_else(|| leaves.first().map(|(id, _)| *id))
    }

    /// Start a Neovim beside the diagram, and give it the keyboard -- asking
    /// for the pane is asking to be in it.
    fn open_editor(&mut self) {
        if self.editor.is_some() {
            self.focus = Focus::Pane;
            return;
        }
        let Some(area) = editor::panes(self.body, Some(self.body.width / 2)).1 else { return };
        match Editor::open(&self.root, area, self.nvim_args) {
            Ok((editor, events)) => {
                // Nvim redraws on its own schedule, so its events have to
                // reach the same loop the keyboard does or the pane would
                // only repaint when something else happened to wake us.
                let inputs = self.inputs.clone();
                std::thread::spawn(move || {
                    for event in events {
                        if inputs.send(Input::Pane(event)).is_err() {
                            return;
                        }
                    }
                });
                self.editor = Some(editor);
                self.pane_area = area;
                self.focus = Focus::Pane;
                self.trouble = None;
                if let Some(selected) = self.selected {
                    self.show_in_pane(selected);
                }
            }
            Err(e) => self.trouble = Some(format!("{e:#}")),
        }
    }

    /// Closing kills the Neovim, so work it is holding would go with it.
    fn close_editor(&mut self) {
        if let Some(unsaved) = self.unsaved() {
            self.trouble = Some(unsaved);
            return;
        }
        self.editor = None;
        self.pane_area = Rect::ZERO;
        self.focus = Focus::Graph;
        self.trouble = None;
    }

    /// What to say instead of throwing away a buffer somebody is part way
    /// through, or `None` when there is nothing to lose.
    fn unsaved(&self) -> Option<String> {
        let editor = self.editor.as_ref()?;
        let modified = editor
            .nvim()
            .eval("len(filter(getbufinfo({'bufloaded': 1}), 'v:val.changed'))")
            .ok()
            .and_then(|n| n.as_i64())
            .unwrap_or(0);
        match modified {
            0 => None,
            1 => Some("the pane has an unwritten buffer; :w it, or :bd! it".into()),
            n => Some(format!("the pane has {n} unwritten buffers; write them, or :bd! them")),
        }
    }

    fn labels(&self) -> Labels<'_> {
        Labels::new(&self.graph)
    }

    fn rebuild(&mut self) {
        self.rebuild_holding(None);
    }

    /// `hold` is a screen cell and the diagram cell under it. Passing one says
    /// the picture is unchanged and only its size differs -- which is true of
    /// zooming and of nothing else here.
    fn rebuild_holding(&mut self, hold: Option<((u16, u16), (u16, u16))>) {
        let picture = view::apply(&self.graph, &self.cursor.coalesced(), &self.settings);
        self.diagram =
            placer::place(&self.labels(), &picture, self.camera.viewport.width.max(20), self.zoom);
        // Only a leaf can be selected: expanding acts on cursor leaves, and a
        // box is an ancestor of one. Keeping a selection that has become a box
        // leaves every later `step_selection` unable to find its own starting
        // point, so tab silently returns to the first leaf each press.
        let still_a_leaf =
            self.diagram.nodes.iter().any(|n| !n.is_box && Some(n.id) == self.selected);
        if !still_a_leaf {
            self.selected = self.inherit_selection();
        }
        // After placement, not before: the edges still rank the layout, so
        // turning them off reads the same picture with the lines taken away
        // rather than reshuffling every node on screen.
        if !self.show_edges {
            self.diagram.edges.clear();
        }
        let (canvas, stats) = render::render(&self.labels(), &self.diagram, self.selected);
        self.canvas = canvas;
        self.stats = stats;
        // A picture of a different size opens centred, but the node the user
        // was looking at is a better anchor than the coordinate they were at:
        // widening the terminal by one column re-lays-out the whole diagram,
        // and snapping back to the top on every column of a drag is unusable.
        match hold {
            Some((screen, point)) => self.camera.rescale(self.diagram.extent(), screen, point),
            None => {
                self.camera.fit(self.diagram.extent());
                if let Some(rect) = self.selected.and_then(|id| self.diagram.rect_of(id)) {
                    self.camera.reveal(rect);
                }
            }
        }
    }

    /// Two boxes change; nothing else on the canvas does.
    fn redraw_selection(&mut self, was: Option<EntityId>) {
        let changed: Vec<EntityId> = [was, self.selected].into_iter().flatten().collect();
        let labels = Labels::new(&self.graph);
        render::restyle(&labels, &self.diagram, &mut self.canvas, &changed, self.selected);
    }

    /// Move the selection, bringing the new node on screen. Both tab and a
    /// mouse click come through here so they cannot disagree about what
    /// selecting means.
    fn select(&mut self, id: EntityId) {
        self.select_and_tell(id, true);
    }

    /// Selecting because the pane's own cursor moved there. The pane is not
    /// told back: it is already showing the thing, and jumping it to the top
    /// of whatever the cursor happened to land in would move the text out
    /// from under the reader.
    fn select_from_pane(&mut self, id: EntityId) {
        self.select_and_tell(id, false);
    }

    fn select_and_tell(&mut self, id: EntityId, tell_pane: bool) {
        if self.selected == Some(id) {
            return;
        }
        let was = self.selected;
        self.selected = Some(id);
        if let Some(rect) = self.diagram.rect_of(id) {
            self.camera.reveal(rect);
        }
        self.redraw_selection(was);
        if tell_pane {
            self.show_in_pane(id);
        }
    }

    /// Open what was selected in the pane, at its first line, with its extent
    /// tinted. A folder has no file to open, so the pane is left showing
    /// whatever it was showing.
    fn show_in_pane(&mut self, id: EntityId) {
        let Some(editor) = self.editor.as_ref() else { return };
        let (Some(rel), Some(entity)) = (self.graph.file_path(id), self.graph.get(id)) else {
            return;
        };
        // A whole file's extent is every line of it, and tinting all of them
        // says nothing; only what sits inside one gets marked.
        let range = match entity.kind == entity_graph::EntityKind::File {
            true => None,
            false => Some(entity.line_range.clone()),
        };
        // Checked, not assumed: a graph can name a file that is not there --
        // built from a stale index, or from a tree that has moved on -- and
        // `:edit` on a directory hands the pane netrw instead of the code.
        let path = self.root.join(rel);
        if !path.is_file() {
            return;
        }
        editor.show(&path, entity.line_range.start, range);
    }

    /// Follow the pane's cursor: whatever the diagram is drawing for the
    /// entity it is sitting in becomes the selection.
    ///
    /// The entity itself is often not drawn -- the graph is collapsed above
    /// it -- so this selects the nearest drawn leaf standing for it, the same
    /// rule a click in the browser's code pane follows.
    fn follow_pane_cursor(&mut self) {
        let Some(editor) = self.editor.as_ref() else { return };
        // The line first, because it comes free with the redraw. Asking nvim
        // which file it is showing is a round trip, and a redraw that left
        // the cursor on the line it was on -- every keystroke of typing a
        // word -- is not worth one. The cost is that switching file and
        // landing on the same line is not noticed until the next move.
        let line = editor.nvim().viewport().curline;
        if line == self.pane_cursor.1 {
            return;
        }
        self.pane_cursor.1 = line;
        let Some(path) = editor.current_file() else { return };
        self.pane_cursor.0 = path.clone();
        let Ok(rel) = path.strip_prefix(&self.root) else { return };
        let Some(file) = self.graph.file_at_path(rel) else { return };
        let Some(inner) = self.graph.innermost_at(file, line) else { return };
        if let Some(drawn) = self.drawn_leaf_for(inner) {
            self.select_from_pane(drawn);
        }
    }

    /// Select whatever the word under the pane's cursor refers to.
    ///
    /// The pane deliberately stays where it is, so the calls in one function
    /// can be clicked through one after another -- the browser's code pane
    /// works the same way, for the same reason.
    fn go_to_definition(&mut self) {
        let Some(editor) = self.editor.as_ref() else { return };
        let Some((line, column, text)) = editor.cursor_site() else { return };
        let file = editor
            .current_file()
            .and_then(|path| Some(path.strip_prefix(&self.root).ok()?.to_path_buf()))
            .and_then(|rel| self.graph.file_at_path(&rel));
        let Some(file) = file else {
            self.trouble = Some("that file is not in the graph".into());
            return;
        };

        // Several targets are only a problem if the view still tells them
        // apart; folded into one drawn node they are one answer.
        let mut drawn: Vec<EntityId> =
            entity_graph::goto::reference_targets(&self.graph, file, line, &text, column)
                .into_iter()
                .filter_map(|id| self.drawn_leaf_for(id))
                .collect();
        drawn.sort();
        drawn.dedup();
        match drawn.as_slice() {
            [one] => {
                let one = *one;
                self.trouble = None;
                self.select_from_pane(one);
            }
            [] => self.trouble = Some("nothing here refers to anything on the diagram".into()),
            many => {
                let n = many.len();
                self.trouble = Some(format!("that goes to {n} different places"));
            }
        }
    }

    /// The drawn leaf standing for `id`: itself if it is one, else the
    /// nearest ancestor that is. `None` when nothing on its line is drawn --
    /// hidden, or scoped away.
    fn drawn_leaf_for(&self, id: EntityId) -> Option<EntityId> {
        let mut cur = Some(id);
        while let Some(c) = cur {
            if self.diagram.nodes.iter().any(|n| !n.is_box && n.id == c) {
                return Some(c);
            }
            cur = self.graph.get(c).and_then(|e| e.parent);
        }
        None
    }

    /// Move the selection through the drawn leaves in reading order, and scroll
    /// far enough to put the new one on screen.
    fn step_selection(&mut self, forward: bool) {
        let mut leaves: Vec<(EntityId, Rect)> =
            self.diagram.nodes.iter().filter(|n| !n.is_box).map(|n| (n.id, n.rect)).collect();
        leaves.sort_by_key(|(_, r)| (r.y, r.x));
        if leaves.is_empty() {
            return;
        }
        let at = leaves.iter().position(|(id, _)| Some(*id) == self.selected);
        let next = match (at, forward) {
            (Some(i), true) => (i + 1) % leaves.len(),
            (Some(i), false) => (i + leaves.len() - 1) % leaves.len(),
            (None, _) => 0,
        };
        self.select(leaves[next].0);
    }

    /// What each switch currently reads. Kept next to the code that acts on
    /// them so a control cannot say "on" and do nothing.
    fn state(&self, c: Control) -> State {
        match c {
            Control::Editor => State::Switch(self.editor.is_some()),
            Control::Zoom => State::Level(self.zoom.name()),
            Control::Tests => State::Switch(self.settings.show_tests),
            Control::OnePerPair => State::Switch(self.settings.one_per_pair),
            Control::Edges => State::Switch(self.show_edges),
            Control::Hide | Control::Scope => State::Action(self.selected.is_some()),
            Control::ShowAll => {
                State::Undo(self.settings.hidden.len() + usize::from(self.settings.scope.is_some()))
            }
            Control::CollapseAll => State::Action(
                self.cursor
                    .active()
                    .iter()
                    .any(|&l| self.graph.get(l).is_some_and(|e| e.parent.is_some())),
            ),
        }
    }

    fn control(&mut self, c: Control) {
        match c {
            Control::Editor => {
                match self.editor.is_some() {
                    true => self.close_editor(),
                    false => self.open_editor(),
                }
                return;
            }
            // Cycling is its own path: it holds the middle of the screen, and
            // the shared `rebuild` at the end of this function would re-centre.
            Control::Zoom => {
                let screen = self.viewport_middle();
                let hold = self.camera.at(screen.0, screen.1).map(|p| (screen, p));
                self.zoom = self.zoom.cycle();
                self.rebuild_holding(hold);
                return;
            }
            Control::Tests => self.settings.show_tests = !self.settings.show_tests,
            Control::OnePerPair => self.settings.one_per_pair = !self.settings.one_per_pair,
            Control::Edges => self.show_edges = !self.show_edges,
            // Hiding and scoping act on a whole entity, so they name the
            // selection itself rather than the leaf drawn for it.
            Control::Hide => match self.selected {
                Some(id) => {
                    self.settings.hidden.insert(id);
                }
                None => return,
            },
            Control::Scope => match self.selected {
                Some(id) => self.settings.scope = Some(id),
                None => return,
            },
            Control::ShowAll => {
                self.settings.hidden.clear();
                self.settings.scope = None;
            }
            // Up one level at a time until nothing moves: the cursor has no
            // "all the way out", and a leaf whose parent is already a leaf
            // has to be left where it is rather than skipped.
            Control::CollapseAll => {
                while self
                    .cursor
                    .active()
                    .to_vec()
                    .into_iter()
                    .filter(|&l| self.cursor.move_up(l, &self.graph))
                    .count()
                    > 0
                {}
            }
        }
        self.rebuild();
    }

    /// Swap the selected node for its children: what is *in* the picture
    /// changes. Not to be confused with zooming, which changes how big the
    /// same picture is drawn.
    fn expand(&mut self) {
        let Some(id) = self.selected else { return };
        if self.cursor.move_down(id, &self.graph) {
            self.rebuild();
        }
    }

    /// Fold the selected node back into its parent.
    fn collapse(&mut self) {
        let Some(id) = self.selected else {
            // Nothing is drawn -- every node at this level was filtered out.
            // Collapsing the whole cursor is the only way out that does not
            // require guessing which setting the user wants changed.
            let leaves = self.cursor.active().to_vec();
            if leaves.into_iter().filter(|&l| self.cursor.move_up(l, &self.graph)).count() > 0 {
                self.rebuild();
            }
            return;
        };
        if self.cursor.move_up(id, &self.graph) {
            // The node folds into its parent, which is what the user is now
            // looking at.
            self.selected = self.graph.get(id).and_then(|e| e.parent);
            self.rebuild();
        }
    }

    /// Fold a whole box back into one node: every active leaf under it moves
    /// up until the box itself is the leaf.
    fn collapse_into(&mut self, id: EntityId) {
        loop {
            let under: Vec<EntityId> = self
                .cursor
                .active()
                .iter()
                .copied()
                .filter(|&l| l != id && is_under(&self.graph, l, id))
                .collect();
            if under.is_empty() {
                break;
            }
            if under.into_iter().filter(|&l| self.cursor.move_up(l, &self.graph)).count() == 0 {
                break;
            }
        }
        self.selected = Some(id);
        self.rebuild();
    }

    /// Draw the same graph larger or smaller, holding `screen` still. Nothing
    /// enters or leaves the picture; the nodes are given fewer cells each.
    fn set_zoom(&mut self, in_: bool, screen: (u16, u16)) {
        let Some(next) = (if in_ { self.zoom.in_() } else { self.zoom.out() }) else { return };
        // Whatever is under that cell is what the user is reading, so it is
        // what the new size is measured around. Off the diagram -- in the
        // margin around a small one -- there is nothing to hold.
        let hold = self.camera.at(screen.0, screen.1).map(|p| (screen, p));
        self.zoom = next;
        self.rebuild_holding(hold);
    }

    /// The middle of the screen, for a zoom worked from the keyboard: there is
    /// no pointer, and the middle is what the reader is looking at.
    fn viewport_middle(&self) -> (u16, u16) {
        let v = self.camera.viewport;
        (v.x + v.width / 2, v.y + v.height / 2)
    }

    /// Route a mouse event. Scrolling pans, shift turns it sideways and ctrl
    /// zooms -- the same three gestures the browser view uses, so muscle
    /// memory carries over.
    fn mouse(&mut self, ev: MouseEvent) {
        // The key list owns the input while it is up, for the same reason it
        // owns the keyboard: nothing the user cannot see should move.
        if self.help {
            self.help = false;
            return;
        }
        // Pointing at something says which pane you mean, so the mouse needs
        // no focus rule of its own -- it sets one.
        if self.editor.is_some() && self.pane_area.contains(Position::new(ev.column, ev.row)) {
            self.focus = Focus::Pane;
            let area = self.pane_area;
            if let Some(editor) = self.editor.as_ref() {
                editor.mouse(ev.kind, ev.column, ev.row, area);
            }
            return;
        }
        self.focus = Focus::Graph;
        let shift = ev.modifiers.contains(KeyModifiers::SHIFT);
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        match ev.kind {
            MouseEventKind::ScrollUp if ctrl => self.zoom_at(ev.column, ev.row, true),
            MouseEventKind::ScrollDown if ctrl => self.zoom_at(ev.column, ev.row, false),

            MouseEventKind::ScrollUp if shift => self.camera.scroll(-WHEEL_X, 0),
            MouseEventKind::ScrollDown if shift => self.camera.scroll(WHEEL_X, 0),
            MouseEventKind::ScrollUp => self.camera.scroll(0, -WHEEL_Y),
            MouseEventKind::ScrollDown => self.camera.scroll(0, WHEEL_Y),
            // Terminals that report them natively, rather than as shift+wheel.
            MouseEventKind::ScrollLeft => self.camera.scroll(-WHEEL_X, 0),
            MouseEventKind::ScrollRight => self.camera.scroll(WHEEL_X, 0),
            // The panel is drawn over the diagram, so it gets the click
            // first -- including on a dead row, which would otherwise select
            // whatever node it is covering.
            MouseEventKind::Down(MouseButton::Left) if self.panel.contains(ev.column, ev.row) => {
                if let Some(c) = self.panel.hit(ev.column, ev.row) {
                    self.control(c);
                }
            }
            // A node's own buttons sit on its frame, which is inside its
            // rect, so they have to be tried before the rect selects.
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((id, action)) = self.button_under(ev.column, ev.row) {
                    self.node_action(id, action);
                } else if let Some(id) = self.leaf_under(ev.column, ev.row) {
                    self.select(id);
                }
            }
            _ => {}
        }
    }

    fn leaf_under(&self, col: u16, row: u16) -> Option<EntityId> {
        self.camera.at(col, row).and_then(|p| self.diagram.leaf_at(p))
    }

    /// Zoom about the pointer: the cell it is on stays the cell it is on, so
    /// the thing being read does not slide out from under it. The selection is
    /// deliberately untouched -- pointing at something is not choosing it.
    fn zoom_at(&mut self, col: u16, row: u16, in_: bool) {
        self.set_zoom(in_, (col, row));
    }

    /// The button under a screen cell, if the pointer is on one. Searched
    /// from the inside out, because a leaf is drawn over the box holding it.
    fn button_under(&self, col: u16, row: u16) -> Option<(EntityId, NodeAction)> {
        let p = self.camera.at(col, row)?;
        // The diagram's own level, not the app's: they agree today, and the
        // whole point of the diagram carrying one is that nothing has to
        // remember to keep them agreeing.
        let m = self.diagram.zoom.metrics();
        let extent = self.diagram.extent();
        self.diagram.nodes.iter().rev().find_map(|n| {
            // A node the renderer refused to draw has no buttons to press.
            if n.rect.right() > extent.0 || n.rect.bottom() > extent.1 {
                return None;
            }
            let expandable = self.graph.get(n.id).is_some_and(|e| !e.children.is_empty());
            let actions = controls::node_actions(n.is_box, expandable);
            let bordered = n.is_box || m.bordered;
            controls::node_action_at(n.rect, actions, bordered, p.0, p.1).map(|a| (n.id, a))
        })
    }

    fn node_action(&mut self, id: EntityId, action: NodeAction) {
        match action {
            NodeAction::Expand => {
                self.select(id);
                self.expand();
            }
            NodeAction::Collapse => self.collapse_into(id),
            NodeAction::Scope => {
                self.settings.scope = Some(id);
                self.rebuild();
            }
            NodeAction::Hide => {
                self.settings.hidden.insert(id);
                self.rebuild();
            }
        }
    }

    fn status(&self) -> Line<'static> {
        let leaves = self.diagram.nodes.iter().filter(|n| !n.is_box).count();
        let boxes = self.diagram.nodes.len() - leaves;
        let drawn = self.stats.submitted - self.stats.unroutable;
        let mut spans = vec![
            Span::styled(
                format!(" {leaves} nodes in {boxes} boxes "),
                Style::default().fg(Color::Black).bg(Color::Cyan),
            ),
            Span::raw(format!(" {drawn}/{} edges ", self.stats.submitted)),
        ];
        if self.stats.unroutable > 0 {
            spans.push(Span::styled(
                format!("({} unroutable) ", self.stats.unroutable),
                Style::default().fg(Color::Red),
            ));
        }
        spans.push(Span::styled(
            format!("· {} ", self.zoom.name()),
            Style::default().fg(Color::DarkGray),
        ));
        let (row, last) = self.camera.row();
        spans.push(Span::raw(format!(
            "· {}x{} · row {}/{} ",
            self.diagram.width,
            self.diagram.height,
            row.max(0),
            last.max(0),
        )));
        if let Some(scope) = self.settings.scope {
            spans.push(Span::styled(
                format!("· only {} ", self.labels().name(scope)),
                Style::default().fg(Color::Cyan),
            ));
        }
        if self.editor.is_some() {
            spans.push(Span::styled(
                match self.focus {
                    Focus::Pane => "· pane ".to_string(),
                    Focus::Graph => "· pane (ctrl-w l) ".to_string(),
                },
                Style::default().fg(Color::DarkGray),
            ));
        }
        // Last, and in red: it is the one thing on this line the user has to
        // do something about.
        if let Some(trouble) = &self.trouble {
            spans.push(Span::styled(
                format!("· {trouble} "),
                Style::default().fg(Color::Red),
            ));
        }
        Line::from(spans)
    }

    /// A press, to whoever has the keyboard. The pane is a whole editor, so
    /// when it is in focus everything goes to it except the `ctrl-w` that
    /// leads back out; see `editor`.
    fn press(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        if self.focus == Focus::Pane {
            match self.editor.as_mut().map(|e| e.key(key)) {
                Some(Handled::Pane) => {}
                Some(Handled::GiveUpFocus) => self.focus = Focus::Graph,
                Some(Handled::Resize(by)) => self.widen_pane(by),
                Some(Handled::GoToDefinition) => self.go_to_definition(),
                // Focus without a pane to hold it; put it back.
                None => self.focus = Focus::Graph,
            }
            return;
        }
        if std::mem::take(&mut self.pending_window) {
            match key.code {
                KeyCode::Char('l') => self.open_editor(),
                KeyCode::Char('<') => self.widen_pane(-4),
                KeyCode::Char('>') => self.widen_pane(4),
                _ => {}
            }
            return;
        }
        self.key(key.code, key.modifiers);
    }

    fn take(&mut self, input: Input) {
        match input {
            // Mouse capture reports every twitch of the pointer. Those change
            // nothing, so they neither reach the app -- where any event
            // dismisses the key list -- nor cost a repaint.
            Input::Term(Event::Key(key)) if key.kind == KeyEventKind::Press => self.press(key),
            Input::Term(Event::Mouse(ev)) if is_gesture(ev.kind) => self.mouse(ev),
            Input::Term(_) => {}
            // The pane repainted. The frame after this picks that up on its
            // own; what needs doing here is noticing whether its cursor moved
            // into a different piece of the graph.
            Input::Pane(nvim_ui::Event::Redraw) => self.follow_pane_cursor(),
            Input::Pane(nvim_ui::Event::Exited) => {
                self.editor = None;
                self.pane_area = Rect::ZERO;
                self.focus = Focus::Graph;
                self.trouble = Some("the pane's nvim exited".into());
            }
        }
    }

    fn widen_pane(&mut self, by: i32) {
        if let Some(editor) = self.editor.as_mut() {
            let width = (i32::from(editor.width()) + by).clamp(0, i32::from(u16::MAX));
            editor.set_width(width as u16);
        }
    }

    /// Quitting takes the pane's Neovim with it, so it is refused while that
    /// would lose something.
    fn leave(&mut self) {
        match self.unsaved() {
            Some(unsaved) => self.trouble = Some(unsaved),
            None => self.quit = true,
        }
    }

    fn key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let page = self.camera.viewport.height.max(1) as i32 / 2;
        match code {
            // While the key list is up it owns the keyboard, so a stray press
            // dismisses it rather than scrolling something the user cannot see.
            _ if self.help => self.help = false,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('q') | KeyCode::Esc => self.leave(),
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.leave(),
            KeyCode::Char('w') if mods.contains(KeyModifiers::CONTROL) => {
                self.pending_window = true
            }
            KeyCode::Up | KeyCode::Char('k') => self.camera.scroll(0, -1),
            KeyCode::Down | KeyCode::Char('j') => self.camera.scroll(0, 1),
            KeyCode::Left | KeyCode::Char('h') => self.camera.scroll(-4, 0),
            KeyCode::Right | KeyCode::Char('l') => self.camera.scroll(4, 0),
            KeyCode::PageUp => self.camera.scroll(0, -page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.camera.scroll(0, page),
            // Back to the beginning is the same place the view opens at, so
            // `g` and `c` land together rather than disagreeing about where
            // the start of a diagram narrower than the screen is.
            KeyCode::Home | KeyCode::Char('g') | KeyCode::Char('c') => self.camera.center(),
            KeyCode::End | KeyCode::Char('G') => self.camera.bottom(),
            KeyCode::Tab | KeyCode::Char('n') => self.step_selection(true),
            KeyCode::BackTab | KeyCode::Char('p') => self.step_selection(false),
            KeyCode::Enter | KeyCode::Char('+') => self.expand(),
            KeyCode::Backspace | KeyCode::Char('-') => self.collapse(),
            KeyCode::Char(c) if controls::for_key(c).is_some() => {
                let control = controls::for_key(c).expect("just checked");
                if self.state(control).enabled() {
                    self.control(control);
                }
            }
            _ => {}
        }
    }
}

/// Is `id` inside `ancestor`?
fn is_under(graph: &EntityGraph, id: EntityId, ancestor: EntityId) -> bool {
    let mut cur = Some(id);
    while let Some(c) = cur {
        if c == ancestor {
            return true;
        }
        cur = graph.get(c).and_then(|e| e.parent);
    }
    false
}

/// Where the graph comes from. SCIP by default: its references are resolved
/// by a real indexer rather than by name, and everything the diagram draws is
/// only as true as they are.
#[derive(clap::Parser)]
#[command(name = "cerebro", about = "Terminal 2D graph view of a code entity graph")]
struct Args {
    /// Build from this SCIP index instead of generating one.
    #[arg(long, value_name = "index.scip")]
    scip: Option<std::path::PathBuf>,
    /// Parse with tree-sitter instead. Starts in under a second on any tree,
    /// at the cost of references matched by name.
    #[arg(long, conflicts_with = "scip")]
    treesitter: bool,
    /// Project root (or single file) to load.
    #[arg(default_value = ".")]
    path: std::path::PathBuf,
}

fn load_graph(args: &Args, root: &std::path::Path) -> Result<EntityGraph> {
    match (&args.scip, args.treesitter) {
        (Some(index), _) => load_scip(index, root),
        (None, true) => {
            eprintln!("parsing {}...", root.display());
            load_treesitter(root)
        }
        // Not a cached path: the freshness rule inside `working_tree_index` is
        // what makes a stale index get rebuilt rather than silently reused.
        (None, false) => load_scip(&working_tree_index(root)?, root),
    }
}

#[cfg(feature = "scip")]
use scip_producer::index::working_tree_index;

#[cfg(not(feature = "scip"))]
fn working_tree_index(_root: &std::path::Path) -> Result<std::path::PathBuf> {
    anyhow::bail!("this binary was built without the `scip` feature; pass --treesitter")
}

#[cfg(feature = "scip")]
fn load_scip(index: &std::path::Path, root: &std::path::Path) -> Result<EntityGraph> {
    scip_producer::graph_from_index(index, root)
        .with_context(|| format!("loading SCIP index {}", index.display()))
}

#[cfg(not(feature = "scip"))]
fn load_scip(_index: &std::path::Path, _root: &std::path::Path) -> Result<EntityGraph> {
    anyhow::bail!("--scip requires a binary built with `--features scip`")
}

#[cfg(feature = "treesitter")]
fn load_treesitter(root: &std::path::Path) -> Result<EntityGraph> {
    treesitter_producer::graph_from_path(root)
        .with_context(|| format!("parsing {}", root.display()))
}

#[cfg(not(feature = "treesitter"))]
fn load_treesitter(_root: &std::path::Path) -> Result<EntityGraph> {
    anyhow::bail!("this binary was built without the `treesitter` feature")
}

fn main() -> Result<()> {
    let args = <Args as clap::Parser>::parse();
    let root = args
        .path
        .canonicalize()
        .with_context(|| format!("resolving {}", args.path.display()))?;
    let graph = load_graph(&args, &root)?;

    // A panic with the terminal in raw mode leaves the shell unusable, and the
    // backtrace unreadable on top of the diagram. Restore first, then report.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        hook(info);
    }));

    let mut terminal = ratatui::try_init().context(
        "opening the terminal (cerebro draws a diagram; it needs a real terminal, \
         not a pipe or redirect -- use `--example spike` for text output)",
    )?;
    // Everything from here to `restore` runs with the terminal in raw mode and
    // reporting the mouse. An early `?` out of the middle of it would leave a
    // shell that echoes nothing and prints escape codes when you move the
    // mouse, so the whole of it is one expression with one exit.
    let result = (|| {
        // Mouse reporting is not part of `try_init`.
        execute!(std::io::stdout(), EnableMouseCapture).context("turning on mouse reporting")?;
        let size = terminal.size()?;
        let viewport = Rect::new(0, 0, size.width, size.height.saturating_sub(2));

        let (inputs, queue) = std::sync::mpsc::channel();
        // The keyboard is read off the main thread so the main thread can wait
        // on the pane as well. It is never joined: it is parked inside
        // `event::read` on a terminal that only closes when the process does.
        let keyboard = inputs.clone();
        std::thread::spawn(move || {
            while let Ok(event) = event::read() {
                if keyboard.send(Input::Term(event)).is_err() {
                    return;
                }
            }
        });

        let mut app = App::new(graph, viewport, root, inputs);
        run(&mut terminal, &mut app, &queue)
    })();
    restore();
    result
}

fn restore() {
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
}

fn draw_keys(frame: &mut ratatui::Frame, area: Rect) {
    // Sized from the longest line rather than a guess: a fixed width silently
    // clipped the ends of the descriptions, which is the one thing this panel
    // exists to show.
    let width = KEYS
        .iter()
        .map(|(k, what)| k.chars().count().max(20) + what.chars().count() + 4)
        .max()
        .unwrap_or(40)
        .min(area.width as usize) as u16;
    let height = (KEYS.len() as u16 + 2).min(area.height);
    let panel = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    let lines: Vec<Line> = KEYS
        .iter()
        .map(|(keys, what)| {
            Line::from(vec![
                Span::styled(format!(" {keys:<20}"), Style::default().fg(Color::Yellow)),
                Span::raw(format!(" {what}")),
            ])
        })
        .collect();
    frame.render_widget(ratatui::widgets::Clear, panel);
    frame.render_widget(
        Paragraph::new(lines).block(
            ratatui::widgets::Block::bordered()
                .title(" keys ")
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        panel,
    );
}

/// Did the user mean something by it? Moving the pointer, letting a button up
/// and dragging are all reported and all mean nothing here.
fn is_gesture(kind: MouseEventKind) -> bool {
    matches!(
        kind,
        MouseEventKind::Down(_)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
    )
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    inputs: &std::sync::mpsc::Receiver<Input>,
) -> Result<()> {
    draw(terminal, app)?;
    while !app.quit {
        // One input at least, then whatever else is already queued behind it,
        // and one frame for the lot. A burst of nvim redraws costs one repaint
        // rather than one each.
        app.take(inputs.recv().context("the input channel closed")?);
        while let Ok(next) = inputs.try_recv() {
            app.take(next);
        }
        if !app.quit {
            draw(terminal, app)?;
        }
    }
    Ok(())
}

/// Generic over the backend so a test can draw a frame and read it back.
fn draw<B>(terminal: &mut ratatui::Terminal<B>, app: &mut App) -> Result<()>
where
    B: ratatui::backend::Backend,
    B::Error: Send + Sync + 'static,
{
    terminal.draw(|frame| {
        let area = frame.area();
        app.body = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2));
        let (graph, pane) = editor::panes(app.body, app.editor.as_ref().map(Editor::width));

        if graph.width > 0 && graph != app.camera.viewport {
            // Only width feeds the layout -- it is what a rank wraps
            // against. A change in height just shows more of the same
            // diagram, and re-routing every edge to learn that would cost
            // seconds on every drag of a window corner.
            let relaid = graph.width != app.camera.viewport.width;
            app.camera.resize(graph);
            if relaid {
                app.rebuild();
            }
        }
        if graph.width > 0 {
            render::blit(&app.canvas, frame.buffer_mut(), graph, app.camera.offset);
            // Drawn after the diagram and remembered, because the click that
            // works a switch arrives after the frame that showed it.
            app.panel = controls::draw(frame.buffer_mut(), graph, |c| app.state(c));
        } else {
            app.panel = controls::Panel::default();
        }

        app.pane_area = pane.unwrap_or(Rect::ZERO);
        if let (Some(editor), Some(pane)) = (app.editor.as_ref(), pane) {
            // Only on a change: nvim redraws its whole screen for a resize,
            // and asking every frame would have it doing that forever.
            if pane != app.pane_was {
                editor.resize(pane);
            }
            editor.draw(frame.buffer_mut(), pane);
            if app.focus == Focus::Pane {
                let (x, y) = editor.cursor(pane);
                frame.set_cursor_position((x, y));
            }
        }
        app.pane_was = app.pane_area;

        frame.render_widget(
            Paragraph::new(app.status()),
            Rect::new(area.x, area.bottom().saturating_sub(2), area.width, 1),
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                match app.focus {
                    Focus::Pane => PANE_HINT,
                    Focus::Graph => HINT,
                },
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            ))),
            Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
        );
        if app.help {
            draw_keys(frame, area);
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use entity_graph::EntityKind::{File, Folder};
    use entity_graph::ReferenceKind::Call;
    use entity_graph::test_support::graph_from_parents;

    /// A root holding a nested folder, some files and one test file, which is
    /// enough shape for scrolling, selection and the test filter to differ.
    fn fixture() -> EntityGraph {
        let names: Vec<String> = (0..8).map(|i| format!("file_{i:02}.rs")).collect();
        let mut rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> =
            vec![("root", Folder, None), ("inner", Folder, Some(0))];
        rows.extend(names.iter().map(|n| (n.as_str(), File, Some(1))));
        rows.push(("tests.rs", File, Some(1)));
        let refs: Vec<(usize, usize, entity_graph::ReferenceKind)> =
            (2..9).map(|i| (i, i + 1, Call)).collect();
        let mut graph = graph_from_parents(&rows, &refs);
        let last = graph.entities.len() - 1;
        graph.entities[last].is_test = true;
        graph
    }

    /// An app with no pane and nowhere for its inputs to go. Every test here
    /// drives it by calling straight into it, so the channel is only there to
    /// be owned.
    fn app_with(graph: EntityGraph, viewport: Rect) -> App {
        let (inputs, queue) = std::sync::mpsc::channel();
        // Kept alive for as long as the app is, so a send cannot fail and
        // change what is under test.
        std::mem::forget(queue);
        App::new(graph, viewport, ".".into(), inputs)
    }

    fn app() -> App {
        let mut app = app_with(fixture(), Rect::new(0, 0, 60, 20));
        // Expand to the files; the root alone has nothing to scroll or select.
        for _ in 0..2 {
            for leaf in app.cursor.coalesced().leaves {
                app.cursor.move_down(leaf, &app.graph);
            }
        }
        app.rebuild();
        app
    }

    fn wheel(kind: MouseEventKind, mods: KeyModifiers, col: u16, row: u16) -> MouseEvent {
        MouseEvent { kind, column: col, row, modifiers: mods }
    }

    /// Is any of the diagram still on a screen cell?
    fn in_sight(app: &App) -> bool {
        let v = app.camera.viewport;
        (v.y..v.bottom()).any(|row| (v.x..v.right()).any(|col| app.camera.at(col, row).is_some()))
    }

    /// Scrolling to any end has to leave some of the picture on screen; a
    /// blank terminal gives the user nothing to scroll back by.
    #[test]
    fn scrolling_stops_with_the_picture_still_in_sight() {
        let mut app = app();
        for (kind, mods) in [
            (MouseEventKind::ScrollDown, KeyModifiers::NONE),
            (MouseEventKind::ScrollUp, KeyModifiers::NONE),
            (MouseEventKind::ScrollDown, KeyModifiers::SHIFT),
            (MouseEventKind::ScrollUp, KeyModifiers::SHIFT),
        ] {
            for _ in 0..400 {
                app.mouse(wheel(kind, mods, 0, 0));
            }
            assert!(in_sight(&app), "scrolling {kind:?} with {mods:?} emptied the screen");
        }
    }

    /// Shift turns the wheel sideways. Worth pinning because the plain wheel
    /// and the shifted wheel arrive as the same event kind.
    #[test]
    fn shift_scrolling_pans_sideways_instead_of_down() {
        let mut app = app_with(fixture(), Rect::new(0, 0, 30, 20));
        let before = app.camera.offset;
        app.mouse(wheel(MouseEventKind::ScrollDown, KeyModifiers::SHIFT, 0, 0));
        assert_eq!(app.camera.offset.1, before.1, "a sideways pan moved the view down");
        assert!(app.camera.offset.0 > before.0, "shift-scroll did not pan sideways");
    }

    /// Clicking is the whole point of drawing boxes where they are: the cell
    /// the user clicked has to name the node drawn under it.
    #[test]
    fn clicking_a_node_selects_the_one_under_the_pointer() {
        let mut app = app();
        let target = app
            .diagram
            .nodes
            .iter()
            .filter(|n| !n.is_box)
            .find(|n| Some(n.id) != app.selected)
            .expect("the fixture draws more than one leaf");
        let (id, rect) = (target.id, target.rect);
        app.camera.reveal(rect);

        let col = (i32::from(rect.x + 1) - app.camera.offset.0) as u16;
        let row = (i32::from(rect.y + 1) - app.camera.offset.1) as u16;
        app.mouse(wheel(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE, col, row));
        assert_eq!(app.selected, Some(id), "clicking a node did not select it");

        let empty = app.camera.viewport.bottom() - 1;
        app.mouse(wheel(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE, 0, empty));
        assert_eq!(app.selected, Some(id), "clicking nothing cleared the selection");
    }

    /// Zooming out is about size, not content: the same nodes in fewer cells.
    /// If the node set changes, something expanded or collapsed instead.
    #[test]
    fn ctrl_scrolling_draws_the_same_graph_smaller() {
        let mut app = app();
        let before: Vec<EntityId> = app.diagram.nodes.iter().map(|n| n.id).collect();
        let tall = app.diagram.height;
        assert_eq!(app.zoom, Zoom::Close);

        app.mouse(wheel(MouseEventKind::ScrollDown, KeyModifiers::CONTROL, 1, 1));
        assert_eq!(app.zoom, Zoom::Mid);
        let after: Vec<EntityId> = app.diagram.nodes.iter().map(|n| n.id).collect();
        assert_eq!(before, after, "zooming changed what was in the picture");
        assert!(app.diagram.height < tall, "zooming out did not buy any room");

        // The far end of the range holds rather than wrapping round.
        app.mouse(wheel(MouseEventKind::ScrollDown, KeyModifiers::CONTROL, 1, 1));
        app.mouse(wheel(MouseEventKind::ScrollDown, KeyModifiers::CONTROL, 1, 1));
        assert_eq!(app.zoom, Zoom::Far);
    }

    /// Expanding a node is a step *into* it, so the selection has to come out
    /// the other side inside it. It did not: the node became a box, the
    /// invariant repair fell back to the diagram's first leaf, and every
    /// expand threw the user back to the top left.
    #[test]
    fn expanding_a_node_selects_something_inside_it() {
        let rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> = vec![
            ("root", Folder, None),
            ("left", Folder, Some(0)),
            ("right", Folder, Some(0)),
            ("a.rs", File, Some(1)),
            ("b.rs", File, Some(2)),
            ("c.rs", File, Some(2)),
        ];
        let graph = graph_from_parents(&rows, &[(3, 4, Call)]);
        let mut app = app_with(graph, Rect::new(0, 0, 80, 24));
        app.key(KeyCode::Enter, KeyModifiers::NONE); // root -> left, right

        // Pick the *second* folder, so "the first leaf in the diagram" and
        // "a child of what was expanded" cannot be the same answer.
        let right = EntityId(2);
        app.select(right);
        assert_eq!(app.selected, Some(right));

        app.key(KeyCode::Enter, KeyModifiers::NONE);
        let now = app.selected.expect("something is selected");
        assert!(
            is_under(&app.graph, now, right),
            "expanding `right` selected {now:?}, which is not inside it"
        );
        assert!(
            app.diagram.nodes.iter().any(|n| !n.is_box && n.id == now),
            "the selection is not a drawn leaf"
        );
    }

    /// Zooming is the one relayout where the picture does not change, so what
    /// the pointer is on has to stay under the pointer. Routing it through
    /// the ordinary "new picture" path sends a reader at the bottom of a long
    /// diagram back to the top, which is what this pins against.
    #[test]
    fn ctrl_scrolling_holds_the_cell_under_the_pointer() {
        let mut app = app();
        assert!(app.diagram.height > app.camera.viewport.height * 2, "needs a tall diagram");
        app.key(KeyCode::Char('G'), KeyModifiers::NONE);

        // The middle column: a diagram narrower than the terminal is centred,
        // so the left of the screen can be margin rather than diagram.
        let screen = (app.camera.viewport.width / 2, app.camera.viewport.height - 3);
        let (before, tall) = (
            app.camera.at(screen.0, screen.1).expect("pointing at the diagram"),
            app.diagram.height,
        );
        app.mouse(wheel(MouseEventKind::ScrollDown, KeyModifiers::CONTROL, screen.0, screen.1));

        let after = app.camera.at(screen.0, screen.1).expect("still on the diagram");
        let want = u32::from(before.1) * u32::from(app.diagram.height) / u32::from(tall);
        assert!(
            u32::from(after.1).abs_diff(want) <= 2,
            "zooming moved the ground under the pointer: row {} became {}, wanted about {want}",
            before.1,
            after.1,
        );
    }

    /// A node's own buttons are the browser's, and they have to do what the
    /// panel row of the same name does.
    #[test]
    fn a_nodes_own_buttons_hide_and_expand_it() {
        let rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> = vec![
            ("root", Folder, None),
            ("left", Folder, Some(0)),
            ("right", Folder, Some(0)),
            ("a.rs", File, Some(1)),
            ("b.rs", File, Some(2)),
        ];
        let mut app = app_with(graph_from_parents(&rows, &[(3, 4, Call)]), Rect::new(0, 0, 80, 24));
        app.key(KeyCode::Enter, KeyModifiers::NONE); // root -> left, right

        let leaf = app.diagram.nodes.iter().find(|n| !n.is_box).expect("a leaf is drawn");
        let (id, rect) = (leaf.id, leaf.rect);
        let actions = controls::node_actions(false, true);
        assert_eq!(actions, &[NodeAction::Expand, NodeAction::Hide]);
        let (bx, by) = controls::node_action_row(rect, actions.len(), true).expect("room for buttons");
        let screen = |app: &App, x: u16, y: u16| {
            (
                (i32::from(x) - app.camera.offset.0) as u16,
                (i32::from(y) - app.camera.offset.1) as u16,
            )
        };

        assert_eq!(
            app.canvas[(bx, by)].symbol(),
            "+",
            "the button was hit-testable but never drawn"
        );
        let (col, row) = screen(&app, bx, by);
        app.mouse(wheel(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE, col, row));
        assert!(
            app.diagram.nodes.iter().any(|n| n.id == id && n.is_box),
            "the + button did not expand the node"
        );

        // ... and the × button on the box it just became takes it away.
        let rect = app.diagram.rect_of(id).expect("still drawn");
        let box_actions = controls::node_actions(true, true);
        let (bx, by) = controls::node_action_row(rect, box_actions.len(), true).expect("room");
        let hide_at = bx + box_actions.iter().position(|a| *a == NodeAction::Hide).unwrap() as u16;
        let (col, row) = screen(&app, hide_at, by);
        app.mouse(wheel(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE, col, row));
        assert!(!app.diagram.nodes.iter().any(|n| n.id == id), "the × button did not hide the box");
    }

    #[test]
    fn tab_walks_every_leaf_and_comes_back_round() {
        let mut app = app();
        let leaves = app.diagram.nodes.iter().filter(|n| !n.is_box).count();
        assert!(leaves >= 2, "fixture should draw several leaves");
        let first = app.selected.expect("something is selected at rest");
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..leaves {
            seen.insert(app.selected.unwrap());
            app.key(KeyCode::Tab, KeyModifiers::NONE);
        }
        assert_eq!(seen.len(), leaves, "tab skipped a leaf");
        assert_eq!(app.selected, Some(first), "tab did not wrap around");
    }

    #[test]
    fn hiding_tests_takes_them_out_of_the_diagram() {
        let mut app = app();
        let test_id = app.graph.entities.iter().find(|e| e.is_test).unwrap().id;
        let shown = |a: &App| a.diagram.nodes.iter().any(|n| n.id == test_id);

        assert!(shown(&app), "tests are shown by default, as in the browser");
        app.key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(!shown(&app), "t should hide test code");
        app.key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(shown(&app), "t should bring it back");
    }

    /// The panel only exists once a frame has been drawn, so a test that
    /// clicks a switch has to draw one first.
    fn draw_panel(app: &mut App) {
        let area = app.camera.viewport;
        let mut buf = ratatui::buffer::Buffer::empty(area);
        let panel = controls::draw(&mut buf, area, |c| app.state(c));
        app.panel = panel;
    }

    fn drawn(app: &App, id: EntityId) -> bool {
        app.diagram.nodes.iter().any(|n| n.id == id)
    }

    /// Hiding is the switch a user reaches for most, and `show all` is the
    /// only way back from it -- so they are tested as the pair they are.
    #[test]
    fn hiding_the_selection_takes_it_out_until_show_all_brings_it_back() {
        let mut app = app();
        let gone = app.selected.expect("something is selected at rest");
        app.key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(!drawn(&app, gone), "x did not hide the selected node");
        assert!(app.diagram.nodes.iter().any(|n| !n.is_box), "x hid everything");

        app.key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(drawn(&app, gone), "show all did not bring the hidden node back");
    }

    #[test]
    fn scoping_keeps_only_the_selection_and_show_all_undoes_it() {
        let mut app = app();
        let kept = app.selected.expect("something is selected at rest");
        app.key(KeyCode::Char('s'), KeyModifiers::NONE);
        let leaves: Vec<EntityId> =
            app.diagram.nodes.iter().filter(|n| !n.is_box).map(|n| n.id).collect();
        assert_eq!(leaves, vec![kept], "scoping left other nodes in the picture");

        app.key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(app.diagram.nodes.iter().filter(|n| !n.is_box).count() > 1, "scope was not cleared");
    }

    /// Turning the lines off is for reading the names underneath them; if the
    /// names move at the same moment, that has not happened.
    #[test]
    fn turning_edges_off_takes_the_lines_away_without_moving_a_node() {
        let mut app = app();
        assert!(!app.diagram.edges.is_empty(), "the fixture should draw some edges");
        let before: Vec<(EntityId, Rect)> =
            app.diagram.nodes.iter().map(|n| (n.id, n.rect)).collect();

        app.key(KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(app.diagram.edges.is_empty(), "r did not take the edges away");
        let after: Vec<(EntityId, Rect)> =
            app.diagram.nodes.iter().map(|n| (n.id, n.rect)).collect();
        assert_eq!(before, after, "turning edges off moved the nodes");
    }

    fn press(code: KeyCode, mods: KeyModifiers) -> ratatui::crossterm::event::KeyEvent {
        ratatui::crossterm::event::KeyEvent::new(code, mods)
    }

    /// The pane is a real Neovim; without one there is nothing to test.
    fn have_nvim() -> bool {
        let found = std::process::Command::new("nvim")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !found {
            eprintln!("skipping: nvim is not on PATH");
        }
        found
    }

    fn app_with_pane() -> App {
        let mut app = app();
        // Wide enough that both panes are worth drawing.
        app.body = Rect::new(0, 0, 120, 20);
        // Not the user's own nvim: a config that opens a dashboard, or makes
        // a fresh buffer unmodifiable, would decide whether these pass.
        app.nvim_args = &["--clean"];
        app.control(Control::Editor);
        app
    }

    /// The whole of the arbitration. Nvim wants `q`, `tab`, `z` and the rest
    /// for itself, so while the pane has the keyboard the diagram must not
    /// act on any of them -- and `ctrl-w h`, the move the user's fingers
    /// already know, must hand it back.
    #[test]
    fn the_pane_takes_the_whole_keyboard_and_ctrl_w_h_gives_it_back() {
        if !have_nvim() {
            return;
        }
        let mut app = app_with_pane();
        assert_eq!(app.focus, Focus::Pane, "opening the pane goes to it");
        let selected = app.selected;

        app.press(press(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.quit, "q quit the viewer while the user was typing in nvim");
        app.press(press(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.selected, selected, "tab moved the diagram's selection");

        app.press(press(KeyCode::Char('w'), KeyModifiers::CONTROL));
        app.press(press(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Graph);

        app.press(press(KeyCode::Tab, KeyModifiers::NONE));
        assert_ne!(app.selected, selected, "the diagram never got the keyboard back");
    }

    /// Pointing at a pane says which one you mean, so the mouse sets focus
    /// rather than obeying it.
    #[test]
    fn clicking_a_pane_is_how_you_choose_it() {
        if !have_nvim() {
            return;
        }
        let mut app = app_with_pane();
        app.pane_area = Rect::new(60, 0, 60, 20);

        app.mouse(wheel(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE, 10, 5));
        assert_eq!(app.focus, Focus::Graph, "a click on the diagram did not take focus");

        app.mouse(wheel(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE, 80, 5));
        assert_eq!(app.focus, Focus::Pane, "a click on the pane did not take focus");
    }

    /// Closing the pane kills its Neovim, and quitting closes the pane. Doing
    /// either while a buffer is part-written would throw the work away with
    /// no way to get it back.
    #[test]
    fn work_left_unwritten_in_the_pane_stops_the_viewer_closing_it() {
        if !have_nvim() {
            return;
        }
        let mut app = app_with_pane();
        let nvim = app.editor.as_ref().expect("the pane opened").nvim();
        // Through a call rather than by typing: this has to have landed
        // before the question is asked.
        nvim.call(
            "nvim_buf_set_lines",
            vec![0.into(), 0.into(), (-1).into(), false.into(),
                 nvim_ui::Value::Array(vec!["half a thought".into()])],
        )
        .expect("editing the buffer");

        app.leave();
        assert!(!app.quit, "quitting threw away an unwritten buffer");
        assert!(app.trouble.as_deref().unwrap_or_default().contains("unwritten"));

        app.control(Control::Editor);
        assert!(app.editor.is_some(), "closing threw away an unwritten buffer");

        // Once it is no longer precious, both go through.
        app.editor
            .as_ref()
            .expect("still open")
            .nvim()
            .call("nvim_command", vec!["setlocal nomodified".into()])
            .expect("marking it saved");
        app.control(Control::Editor);
        assert!(app.editor.is_none(), "the pane would not close");
        app.leave();
        assert!(app.quit);
    }

    /// End to end: what nvim draws on its own screen has to arrive in the
    /// frame, in the pane's columns and nowhere else.
    #[test]
    fn what_nvim_draws_lands_in_the_panes_half_of_the_frame() {
        if !have_nvim() {
            return;
        }
        let mut app = app_with_pane();
        {
            let nvim = app.editor.as_ref().expect("the pane opened").nvim();
            nvim.call(
                "nvim_buf_set_lines",
                vec![0.into(), 0.into(), (-1).into(), false.into(),
                     nvim_ui::Value::Array(vec!["MARKER".into()])],
            )
            .expect("putting text in the buffer");
            // The grid is kept current whether or not anything is listening
            // for redraws, so polling it is enough.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !(0..20).any(|y| nvim.row(y).contains("MARKER")) {
                assert!(std::time::Instant::now() < deadline, "nvim never drew it");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 22)).expect("terminal");
        draw(&mut terminal, &mut app).expect("drawing a frame");

        let buf = terminal.backend().buffer();
        let row_text = |y: u16, from: u16, to: u16| -> String {
            (from..to).map(|x| buf[(x, y)].symbol().to_string()).collect()
        };
        let pane = app.pane_area;
        assert!(pane.width > 0, "the pane got no columns");
        let found = (pane.y..pane.bottom())
            .any(|y| row_text(y, pane.x, pane.right()).contains("MARKER"));
        assert!(found, "the pane drew nothing; its first row was {:?}", row_text(pane.y, pane.x, pane.right()));

        let leaked = (0..pane.y.max(20))
            .any(|y| row_text(y, 0, pane.x).contains("MARKER"));
        assert!(!leaked, "the pane painted over the diagram");
    }

    /// A real tree, parsed the way cerebro parses one, so the line numbers
    /// under test are the producer's rather than a fixture author's guess.
    fn project(source: &str) -> (tempfile::TempDir, EntityGraph) {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("lib.rs"), source).expect("writing the source");
        let graph = treesitter_producer::graph_from_path(dir.path()).expect("parsing");
        (dir, graph)
    }

    fn expanded_app(dir: &tempfile::TempDir, graph: EntityGraph) -> App {
        let (inputs, queue) = std::sync::mpsc::channel();
        std::mem::forget(queue);
        let mut app = App::new(graph, Rect::new(0, 0, 120, 20), dir.path().to_path_buf(), inputs);
        for _ in 0..4 {
            for leaf in app.cursor.coalesced().leaves {
                app.cursor.move_down(leaf, &app.graph);
            }
        }
        app.rebuild();
        app.body = Rect::new(0, 0, 120, 20);
        app.nvim_args = &["--clean"];
        app
    }

    fn entity_named<'a>(app: &'a App, name: &str) -> &'a entity_graph::Entity {
        app.graph.entities.iter().find(|e| e.name == name).unwrap_or_else(|| panic!("no {name}"))
    }

    fn settle(mut done: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !done() {
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        true
    }

    /// The pane and the diagram are two views of one thing, and this is the
    /// wire between them: choosing a node in the diagram opens its code, and
    /// moving through the code chooses the node.
    #[test]
    fn the_diagram_and_the_pane_follow_each_other() {
        if !have_nvim() {
            return;
        }
        let (dir, graph) = project("fn alpha() {\n    let x = 1;\n}\n\nfn beta() {\n    let y = 2;\n}\n");
        let mut app = expanded_app(&dir, graph);
        let (alpha, beta) = (entity_named(&app, "alpha").id, entity_named(&app, "beta").id);
        let beta_line = entity_named(&app, "beta").line_range.start;
        app.control(Control::Editor);

        // Diagram to pane: selecting opens the file at what was selected.
        app.select(alpha);
        let editor = app.editor.as_ref().expect("the pane is open");
        assert_eq!(
            editor.current_file().as_deref().and_then(|p| p.file_name()),
            Some(std::ffi::OsStr::new("lib.rs")),
            "selecting a function did not open its file"
        );
        assert!(
            settle(|| app.editor.as_ref().is_some_and(|e| {
                e.nvim().viewport().curline == entity_named(&app, "alpha").line_range.start
            })),
            "the pane did not land on alpha"
        );

        // Pane to diagram: moving the cursor into the other function picks it.
        app.editor
            .as_ref()
            .expect("still open")
            .nvim()
            .call(
                "nvim_win_set_cursor",
                vec![0.into(), nvim_ui::Value::Array(vec![(beta_line as i64 + 1).into(), 0.into()])],
            )
            .expect("moving the cursor");
        assert!(
            settle(|| app.editor.as_ref().is_some_and(|e| e.nvim().viewport().curline == beta_line)),
            "nvim never reported the new cursor line"
        );
        app.follow_pane_cursor();
        assert_eq!(app.selected, Some(beta), "the diagram did not follow the cursor");
    }

    /// The pane is not just a viewer: `ctrl-w d` on a call selects what it
    /// calls, which is the browser's click-an-identifier, done with a key.
    #[test]
    fn ctrl_w_d_selects_what_the_word_under_the_cursor_refers_to() {
        if !have_nvim() {
            return;
        }
        let (dir, graph) = project("fn target() {}\n\nfn caller() {\n    target();\n}\n");
        let mut app = expanded_app(&dir, graph);
        let (target, caller) =
            (entity_named(&app, "target").id, entity_named(&app, "caller").id);
        assert!(
            app.graph.references.iter().any(|r| r.from == caller && r.to == target),
            "the producer recorded no call to go to"
        );
        app.control(Control::Editor);
        app.select(caller);

        // On the `target` of `    target();` -- row 4 counting from one,
        // column 4 counting from zero.
        app.editor
            .as_ref()
            .expect("the pane is open")
            .nvim()
            .call(
                "nvim_win_set_cursor",
                vec![0.into(), nvim_ui::Value::Array(vec![4.into(), 4.into()])],
            )
            .expect("moving the cursor");

        app.press(press(KeyCode::Char('w'), KeyModifiers::CONTROL));
        app.press(press(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(app.selected, Some(target), "trouble was {:?}", app.trouble);

        // And off a name, it says so rather than selecting something random.
        app.select(caller);
        app.editor
            .as_ref()
            .expect("still open")
            .nvim()
            .call(
                "nvim_win_set_cursor",
                vec![0.into(), nvim_ui::Value::Array(vec![2.into(), 0.into()])],
            )
            .expect("moving the cursor");
        app.press(press(KeyCode::Char('w'), KeyModifiers::CONTROL));
        app.press(press(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(app.selected, Some(caller), "a blank line went somewhere");
        assert!(app.trouble.is_some(), "it went nowhere and said nothing");
    }

    /// The panel exists so the mouse can do what the keys do. A click on a
    /// switch has to reach the same code the key reaches.
    #[test]
    fn clicking_a_switch_does_what_its_key_does() {
        let mut app = app();
        draw_panel(&mut app);
        // Found, not counted to: a switch added above this one should not
        // quietly make the test click a different switch.
        let column = app.panel.rect.x + 2;
        let row = (app.panel.rect.y..app.panel.rect.bottom())
            .find(|&y| app.panel.hit(column, y) == Some(Control::Tests))
            .expect("the tests switch is on the panel");
        assert!(app.settings.show_tests);

        app.mouse(wheel(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
            column,
            row,
        ));
        assert!(!app.settings.show_tests, "clicking the tests switch did nothing");

        // A click landing on the panel must never fall through to the diagram.
        let was = app.selected;
        app.mouse(wheel(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
            app.panel.rect.x,
            app.panel.rect.bottom() - 1,
        ));
        assert_eq!(app.selected, was, "a click on the panel border reached the diagram");
    }

    /// While the key list is up it owns the keyboard: a press meant to dismiss
    /// it must not also scroll or zoom something the user cannot see.
    #[test]
    fn the_key_list_swallows_the_press_that_dismisses_it() {
        let mut app = app();
        let (before_offset, before_sel) = (app.camera.offset, app.selected);
        app.key(KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(app.help);

        app.key(KeyCode::Down, KeyModifiers::NONE);
        assert!(!app.help, "any key should dismiss the list");
        assert_eq!(app.camera.offset, before_offset, "the dismissing press also scrolled");
        assert_eq!(app.selected, before_sel);

        app.key(KeyCode::Char('?'), KeyModifiers::NONE);
        app.mouse(wheel(MouseEventKind::ScrollDown, KeyModifiers::NONE, 1, 1));
        assert!(!app.help, "the wheel should dismiss the list too");
        assert_eq!(app.camera.offset, before_offset, "the dismissing scroll also panned");

        app.key(KeyCode::Char('?'), KeyModifiers::NONE);
        app.key(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!app.quit, "the dismissing press also quit");
    }

    /// The selection has to survive a rebuild, or expanding leaves the user
    /// with nothing selected and no way back.
    #[test]
    fn expanding_and_collapsing_keep_something_selected() {
        let mut app = app_with(fixture(), Rect::new(0, 0, 60, 20));
        assert!(app.selected.is_some());
        for _ in 0..3 {
            app.key(KeyCode::Enter, KeyModifiers::NONE);
            assert!(app.selected.is_some(), "expanding lost the selection");
        }
        for _ in 0..3 {
            app.key(KeyCode::Backspace, KeyModifiers::NONE);
            assert!(app.selected.is_some(), "collapsing lost the selection");
        }
    }
}
