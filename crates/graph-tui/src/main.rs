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
use graph_tui::layout::{Diagram, Layout, Options};
use graph_tui::render::{self, Lit, Rendered};
use graph_tui::scene::Scene;
use graph_tui::trail::{Tier, Trail};
use graph_tui::view::{self, Settings};
use graph_tui::zoom::Zoom;

const HINT: &str =
    "? keys  ·  scroll pans, ctrl-scroll zooms  ·  ↵ expand  ·  ⌫ collapse  ·  o editor pane  ·  q quit";
/// The same line, while the pane has the keyboard: every other key on the
/// board belongs to nvim, so listing them would be a lie.
const PANE_HINT: &str = "ctrl-w h  back to the diagram  ·  ctrl-w d  go to what this refers to  ·  ctrl-w < >  move the divider  ·  everything else goes to nvim";

/// Wheel notches are small and terminals are large, so one notch moves more
/// than one cell. Sideways moves further because columns are narrower than
/// rows are tall.
const WHEEL_Y: i32 = 3;
const WHEEL_X: i32 = 6;

/// The status line has room for a handful of keys, which left most of these
/// undiscoverable. Everything that does something is listed here -- except the
/// switches, which say their own keys in the corner of the screen.
const KEYS: &[(&str, &str)] = &[
    ("↑ ↓ ← →", "move the focus to the nearest node that way, among its siblings"),
    ("tab  ⇧tab", "focus into a box -- opening it if it is shut -- or out to the box around"),
    ("⌥↑ ↓ ← →  /  ctrl-↑ ↓ ← →", "move the focused node -- or its group -- a step"),
    ("⇧↑ ↓ ← →  /  k j h l", "scroll"),
    ("PgUp PgDn  /  space", "scroll a half screen"),
    ("Home End  /  g G", "back to the start, or the bottom"),
    ("n p", "focus the next or previous node, reading order"),
    ("click", "select a node, or press a button on its frame"),
    ("⇧click", "add a node to the group, or take it out"),
    ("drag", "move a node -- or the group it is in; drag a box by its frame"),
    ("drag on nothing", "sweep out a group"),
    ("L", "lay the whole picture out afresh"),
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
    /// A rebuild finished. `Err` is what to tell the user instead.
    Reloaded(Box<Result<EntityGraph>>),
}

/// How long the rebuild thread waits for the writing to stop. Saving three
/// files in a row is one edit, and re-indexing after each of them would mean
/// waiting three times for an answer only the last one can give.
const QUIET: std::time::Duration = std::time::Duration::from_millis(300);

/// Re-read the tree the same way it was read at startup.
type Loader = std::sync::Arc<dyn Fn() -> Result<EntityGraph> + Send + Sync>;

/// The one thread that reloads. Requests only wake it; it decides when the
/// writing has stopped, does the work off the main thread -- with SCIP that
/// is an indexer run, not a parse -- and posts the result back into the loop.
fn spawn_reloader(loader: Loader, inputs: std::sync::mpsc::Sender<Input>) -> std::sync::mpsc::Sender<()> {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        while rx.recv().is_ok() {
            // Everything that arrives during the quiet window, and during the
            // reload itself, folds into this one.
            while rx.recv_timeout(QUIET).is_ok() {}
            if inputs.send(Input::Reloaded(Box::new(loader()))).is_err() {
                return;
            }
        }
    });
    tx
}

struct App {
    graph: EntityGraph,
    cursor: coalesce::Cursor,
    settings: Settings,
    /// How large the same graph is drawn, which is a different question from
    /// how much of the graph is expanded.
    zoom: Zoom,
    /// Where every node logically sits, kept across rebuilds. What a drag edits.
    layout: Layout,
    scene: Scene,
    diagram: Diagram,
    /// The painted nodes and routed edges of `diagram`.
    rendered: Rendered,
    /// `rendered` composed for the current selection: what gets blitted.
    canvas: Buffer,
    camera: Camera,
    /// Always a drawn leaf, never a box; see `rebuild`.
    selected: Option<EntityId>,
    /// Nodes gathered by a marquee or shift-clicks, which move as one. The
    /// primary selection is not necessarily among them.
    group: std::collections::BTreeSet<EntityId>,
    drag: Drag,
    /// Where the focus has been, so a key that undoes a step goes back the
    /// way it came rather than wherever the geometry now points.
    trail: Trail,
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
    /// Nudged when a file is written; the thread on the other end decides
    /// when the writing has stopped.
    reload: std::sync::mpsc::Sender<()>,
    /// A reload is in flight. Worth saying, because with SCIP it is an
    /// indexer run rather than a parse.
    reloading: bool,
}

/// Who has the keyboard. Nvim wants every key on the board, so this is the
/// whole of the arbitration: whoever is in focus gets all of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Graph,
    Pane,
}

/// What the left button is holding.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Drag {
    Idle,
    /// Pressed, not yet moved. A click and a drag start the same way, and
    /// which one it was is only known when the pointer moves or lets go.
    Pending { at: (u16, u16), grab: Grab },
    /// Nodes in flight. `levels` are the levels left alone while they move
    /// and committed when they land. `held` is the node under the pointer
    /// and how far its corner is from the pointer, which is what keeps the
    /// screen still: the canvas is normalised to its own top-left, so when
    /// the top-most node moves the whole picture would otherwise slide.
    Moving {
        last: (u16, u16),
        moved: Vec<EntityId>,
        levels: std::collections::BTreeSet<Option<EntityId>>,
        held: (EntityId, (i32, i32)),
    },
    /// Sweeping out a group, in screen cells.
    Marquee { from: (u16, u16), to: (u16, u16) },
}

/// What the camera keeps still while the picture is rebuilt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Anchor {
    /// Nothing in particular: fit the new picture and bring the selection on.
    Fit,
    /// A diagram point stays under a screen cell, scaled with the picture.
    /// A zoom, and nothing else: the picture is the same, drawn larger.
    Scaled { screen: (u16, u16), point: (u16, u16) },
    /// A node's corner, offset by `corner`, stays under a screen cell. A
    /// drag holds the grabbed cell under the pointer; an expand holds the
    /// opened node where it was.
    Node { id: EntityId, corner: (i32, i32), screen: (i32, i32) },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grab {
    Leaf(EntityId),
    Box(EntityId),
    Nothing,
}

impl App {
    fn new(
        graph: EntityGraph,
        viewport: Rect,
        root: std::path::PathBuf,
        inputs: std::sync::mpsc::Sender<Input>,
        loader: Loader,
    ) -> Self {
        let cursor = coalesce::Cursor::new(&graph);
        let reload = spawn_reloader(loader, inputs.clone());
        let rendered = render::render(&Labels::new(&graph), &Scene::default(), &Diagram::empty(), Lit::none());
        let mut app = App {
            graph,
            cursor,
            settings: Settings::default(),
            zoom: Zoom::Close,
            layout: Layout::new(),
            scene: Scene::default(),
            diagram: Diagram::empty(),
            rendered,
            canvas: Buffer::empty(Rect::new(0, 0, 1, 1)),
            camera: Camera::new(viewport, (0, 0)),
            selected: None,
            group: Default::default(),
            drag: Drag::Idle,
            trail: Trail::default(),
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
            reload,
            reloading: false,
        };
        app.rebuild();
        app
    }

    /// Swap in a freshly read graph, carrying everything that named the old
    /// one across: the expansion, the selection, what was hidden and what was
    /// scoped to. Ids are arena indices, so all of them changed.
    fn reloaded(&mut self, result: Result<EntityGraph>) {
        self.reloading = false;
        let new = match result {
            Ok(graph) => graph,
            Err(e) => {
                self.trouble = Some(format!("{e:#}"));
                return;
            }
        };
        let old = std::mem::replace(&mut self.graph, new);
        let map = coalesce::migrate::id_map(&old, &self.graph);
        let moved = |id: EntityId| map.get(id.0).copied().flatten();
        self.cursor =
            coalesce::migrate::migrate_cursor(&old, &self.cursor.leaves, &map, &self.graph);
        self.selected = self.selected.and_then(moved);
        self.group = self.group.iter().copied().filter_map(moved).collect();
        self.layout.migrate(&map);
        self.trail.migrate(&map);
        self.settings.hidden = self.settings.hidden.iter().copied().filter_map(moved).collect();
        self.settings.scope = self.settings.scope.and_then(moved);
        // The pane is showing a file, not an id, so it needs nothing said to
        // it -- but what the diagram thinks its cursor is in has changed.
        self.pane_cursor = (std::path::PathBuf::new(), usize::MAX);
        self.rebuild();
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
                // Asked once the file is up, since that is when nvim knows
                // what gutter this file type gets.
                let most = self.body.width / 2;
                if let Some(editor) = self.editor.as_mut() {
                    let wanted = editor.natural_width(most);
                    editor.set_width(wanted);
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
        self.rebuild_with(Anchor::Fit, &Options::default());
    }

    /// Rebuild keeping the node the user is acting on where it is on screen.
    fn rebuild_around(&mut self, id: EntityId, opts: &Options) {
        let anchor = match self.diagram.rect_of(id) {
            Some(r) => Anchor::Node { id, corner: (0, 0), screen: self.camera.screen_of((r.x, r.y)) },
            None => Anchor::Fit,
        };
        self.rebuild_with(anchor, opts);
    }

    fn lit(&self) -> Lit<'_> {
        Lit { primary: self.selected, group: &self.group }
    }

    /// `anchor` is what the camera keeps still through the change; `opts` is
    /// what a drag in flight asks: its nodes anchored, their levels left alone.
    fn rebuild_with(&mut self, anchor: Anchor, opts: &Options) {
        let picture = view::apply(&self.graph, &self.cursor.coalesced(&self.graph), &self.settings);
        self.scene = Scene::new(&self.graph, &picture);
        let labels = Labels::new(&self.graph);
        // Nodes the layout has never seen are ranked into place; everything
        // else stays exactly where it was, which is what lets a drag mean
        // anything.
        self.layout.settle(&self.scene, &labels, self.camera.viewport.width.max(20));
        self.diagram = self.layout.materialize(&self.scene, &labels, self.zoom, opts);
        // What the push moved is kept, except mid-drag -- the siblings make
        // way at the drop, not before -- and when only the zoom changed:
        // coarser text overlaps where the close text did not, and writing
        // that back would spread the close picture a little on every zoom.
        if !matches!(anchor, Anchor::Scaled { .. }) && !matches!(self.drag, Drag::Moving { .. }) {
            self.layout.commit(&self.diagram, &self.scene);
        }
        // A box is as selectable as a leaf -- the keyboard walks into and
        // out of boxes -- so the selection only moves when what it named is
        // no longer drawn at all.
        let still_drawn = self.diagram.nodes.iter().any(|n| Some(n.id) == self.selected);
        if !still_drawn {
            self.selected = self.inherit_selection();
        }
        let drawn: std::collections::BTreeSet<EntityId> =
            self.diagram.nodes.iter().filter(|n| !n.is_box).map(|n| n.id).collect();
        self.group.retain(|id| drawn.contains(id));
        // After placement, not before: the edges still rank the layout, so
        // turning them off reads the same picture with the lines taken away
        // rather than reshuffling every node on screen.
        if !self.show_edges {
            self.diagram.edges.clear();
        }
        self.rendered = render::render(&labels, &self.scene, &self.diagram, self.lit());
        self.canvas = self.rendered.compose(self.lit());
        match anchor {
            Anchor::Scaled { screen, point } => self.camera.rescale(self.diagram.extent(), screen, point),
            Anchor::Node { id, corner, screen } => {
                self.camera.extent = self.diagram.extent();
                self.hold_under_pointer((id, corner), screen);
            }
            Anchor::Fit => {
                self.camera.fit(self.diagram.extent());
                if let Some(rect) = self.selected.and_then(|id| self.diagram.rect_of(id)) {
                    self.camera.reveal(rect);
                }
            }
        }
    }

    /// Some nodes changed how they are lit; nothing moved.
    fn redraw_selection(&mut self, changed: &[EntityId]) {
        let labels = Labels::new(&self.graph);
        let lit = Lit { primary: self.selected, group: &self.group };
        self.rendered.restyle(&labels, &self.diagram, changed, lit);
        self.canvas = self.rendered.compose(lit);
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
        let changed: Vec<EntityId> = [was, Some(id)].into_iter().flatten().collect();
        self.redraw_selection(&changed);
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
        let mut nodes: Vec<(EntityId, Rect)> = self.diagram.nodes.iter().map(|n| (n.id, n.rect)).collect();
        nodes.sort_by_key(|(_, r)| (r.y, r.x));
        if nodes.is_empty() {
            return;
        }
        let at = nodes.iter().position(|(id, _)| Some(*id) == self.selected);
        let next = match (at, forward) {
            (Some(i), true) => (i + 1) % nodes.len(),
            (Some(i), false) => (i + nodes.len() - 1) % nodes.len(),
            (None, _) => 0,
        };
        self.select(nodes[next].0);
    }

    /// Focus the nearest sibling in a direction: the one closest along it
    /// with the least sideways offset, in the proportions the eye sees them
    /// (a cell is twice as tall as it is wide). Siblings are the children
    /// of the same box, so the arrows never leave the level; tab does that.
    /// Nothing squarely that way falls back to anything at all that way,
    /// so a wrapped row's next row is still "down" from its end.
    ///
    /// Where two are equally good answers, the one the focus last arrived
    /// from wins, so walking back retraces walking out. That is only ever a
    /// tie-break within one of the two passes: a remembered neighbour the
    /// layout has since moved is no longer among what that pass offers, and
    /// distance decides again.
    fn move_focus(&mut self, dir: (i32, i32)) {
        let Some(sel) = self.selected else {
            self.focus_in();
            return;
        };
        let Some(from) = self.diagram.rect_of(sel) else { return };
        let centre = |r: Rect| (f32::from(r.x) + f32::from(r.width) / 2.0, f32::from(r.y) + f32::from(r.height) / 2.0);
        let (fx, fy) = centre(from);
        let level = self.scene.parent.get(&sel).copied();
        // Collected in the order the level lists its children and never
        // sorted, so that candidates the cost cannot separate are still
        // settled by `min_by` taking the first of them.
        let ways: Vec<(EntityId, Tier, f32)> = self
            .scene
            .children(level)
            .iter()
            .filter(|&&id| id != sel)
            .filter_map(|&id| Some((id, centre(self.diagram.rect_of(id)?))))
            .filter_map(|(id, (x, y))| {
                let (dx, dy) = ((x - fx) / 2.0, y - fy);
                let ahead = dx * dir.0 as f32 + dy * dir.1 as f32;
                let aside = (dx * dir.1 as f32 - dy * dir.0 as f32).abs();
                let tier = match aside > ahead {
                    true => Tier::Far,
                    false => Tier::Near,
                };
                (ahead > 0.0).then_some((id, tier, ahead + 2.0 * aside))
            })
            .collect();

        for tier in [Tier::Near, Tier::Far] {
            let here = || ways.iter().filter(|w| w.1 == tier);
            let Some(nearest) = here().min_by(|a, b| a.2.total_cmp(&b.2)) else { continue };
            let id = self
                .trail
                .back(sel, dir, tier)
                .filter(|id| here().any(|w| w.0 == *id))
                .unwrap_or(nearest.0);
            self.trail.stepped(sel, dir, tier, id);
            self.select(id);
            return;
        }
    }

    /// Into the focused box: the child the focus last rose out of, or its
    /// first in reading order. With nothing focused, the first root.
    ///
    /// A leaf that can open is opened on the way in, since asking to go
    /// inside something is asking for it to be open, and stopping to press
    /// `↵` first serves nobody. One that cannot open has nowhere to go.
    ///
    /// The remembered child is checked against the children the box has now,
    /// because expanding, hiding and scoping all change those without the
    /// focus going anywhere near the box.
    fn focus_in(&mut self) {
        if self.selected.is_some_and(|id| !self.scene.is_box(id)) {
            self.expand();
        }
        let level = match self.selected {
            None => None,
            Some(id) if self.scene.is_box(id) => Some(id),
            Some(_) => return,
        };
        let kids = self.scene.children(level);
        let first = self
            .trail
            .inward(level)
            .filter(|id| kids.contains(id))
            .or_else(|| {
                kids.iter()
                    .filter_map(|&id| Some((id, self.diagram.rect_of(id)?)))
                    .min_by_key(|(_, r)| (r.y, r.x))
                    .map(|(id, _)| id)
            });
        if let Some(id) = first {
            self.select(id);
        }
    }

    /// Out to the box around the focus, which is told where the focus was so
    /// that tab comes back to it.
    fn focus_out(&mut self) {
        let Some(sel) = self.selected else { return };
        if let Some(parent) = self.scene.parent.get(&sel).copied() {
            self.trail.rose(sel, Some(parent));
            self.select(parent);
        }
    }

    /// Read the order back off the picture for nodes that have just been put
    /// somewhere, and remember it.
    ///
    /// Only these nodes are re-read. The picture as a whole is not a thing
    /// the order can be recovered from -- a level packed across centres its
    /// columns, so its ranks are not its rows -- but *one* node's place among
    /// siblings that are already in order is unambiguous, and that is all a
    /// drop needs to say.
    fn remember_order(&mut self, moved: &[EntityId]) {
        let mut levels: std::collections::BTreeSet<Option<EntityId>> = Default::default();
        for id in moved {
            levels.insert(self.scene.parent.get(id).copied());
        }
        for level in levels {
            let mut siblings: Vec<(EntityId, Rect)> = self
                .scene
                .children(level)
                .iter()
                .filter_map(|&id| Some((id, self.diagram.rect_of(id)?)))
                .collect();
            siblings.sort_by_key(|(_, r)| (r.y, r.x));
            let order: Vec<EntityId> = siblings.into_iter().map(|(id, _)| id).collect();
            for id in moved.iter().filter(|id| order.contains(id)) {
                self.layout.reranked(*id, &order);
            }
        }
    }

    /// Move the focused node -- or the group it is in -- a step, the way a
    /// drag would: it lands there, its siblings make way, and it holds its
    /// place on screen while the canvas re-normalises underneath.
    fn shove(&mut self, by: (i32, i32)) {
        let Some(sel) = self.selected else { return };
        let Some(rect) = self.diagram.rect_of(sel) else { return };
        let ids: Vec<EntityId> =
            if self.group.contains(&sel) { self.group.iter().copied().collect() } else { vec![sel] };
        let screen = self.camera.screen_of((rect.x, rect.y));
        self.layout.nudge(&ids, by, self.zoom);
        let opts = Options { anchored: ids.iter().copied().collect(), loose: Default::default() };
        let anchor = Anchor::Node { id: sel, corner: (0, 0), screen: (screen.0 + by.0, screen.1 + by.1) };
        self.rebuild_with(anchor, &opts);
        self.remember_order(&ids);
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
            Control::Relayout => State::Action(!self.diagram.nodes.is_empty()),
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
                let anchor = self.zoom_anchor(self.viewport_middle());
                self.zoom = self.zoom.cycle();
                self.rebuild_with(anchor, &Options::default());
                return;
            }
            // Every position is forgotten, so the next rebuild ranks the whole
            // picture as if it had just been opened.
            Control::Relayout => self.layout.clear(),
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
            // The node the user opened stays where it is, on the canvas and
            // on the screen; its siblings make way.
            let opts = Options { anchored: [id].into_iter().collect(), loose: Default::default() };
            self.rebuild_around(id, &opts);
        }
    }

    /// Fold the selected node back into its parent; a selected box folds
    /// into itself.
    fn collapse(&mut self) {
        if let Some(id) = self.selected.filter(|&id| self.scene.is_box(id)) {
            self.collapse_into(id);
            return;
        }
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
            // looking at; the parent shrinks in place.
            self.selected = self.graph.get(id).and_then(|e| e.parent);
            match self.selected {
                Some(parent) => self.rebuild_around(parent, &Options::default()),
                None => self.rebuild(),
            }
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
        self.rebuild_around(id, &Options::default());
    }

    /// Zoom about a screen cell: the diagram point under it stays under it.
    /// Off the diagram -- a small picture, or one scrolled aside -- the
    /// nearest point of it is held instead. Falling back to `Anchor::Fit`
    /// would commit the coarser arrangement, which a zoom must never do.
    fn zoom_anchor(&self, screen: (u16, u16)) -> Anchor {
        let (x, y) = self.camera.unclamped_at(screen.0, screen.1);
        let (w, h) = self.diagram.extent();
        let last = |n: u16| i32::from(n.saturating_sub(1));
        let point = (x.clamp(0, last(w)) as u16, y.clamp(0, last(h)) as u16);
        Anchor::Scaled { screen, point }
    }

    /// Draw the same graph larger or smaller, holding `screen` still. Nothing
    /// enters or leaves the picture; the nodes are given fewer cells each.
    fn set_zoom(&mut self, in_: bool, screen: (u16, u16)) {
        let Some(next) = (if in_ { self.zoom.in_() } else { self.zoom.out() }) else { return };
        // Whatever is under that cell is what the user is reading, so it is
        // what the new size is measured around. Off the diagram -- in the
        // margin around a small one -- there is nothing to hold.
        let anchor = self.zoom_anchor(screen);
        self.zoom = next;
        self.rebuild_with(anchor, &Options::default());
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
            MouseEventKind::Drag(MouseButton::Left) => self.drag_to((ev.column, ev.row)),
            MouseEventKind::Up(MouseButton::Left) => self.drop_at((ev.column, ev.row)),
            MouseEventKind::Drag(_) | MouseEventKind::Up(_) | MouseEventKind::Moved => {}
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
                let at = (ev.column, ev.row);
                if let Some((id, action)) = self.button_under(ev.column, ev.row) {
                    self.node_action(id, action);
                } else if let Some(id) = self.leaf_under(ev.column, ev.row) {
                    if shift {
                        self.toggle_in_group(id);
                    } else {
                        // Pressing a node outside the group means that node
                        // alone; pressing one inside it means the group.
                        if !self.group.contains(&id) {
                            self.clear_group();
                        }
                        self.select(id);
                    }
                    self.drag = Drag::Pending { at, grab: Grab::Leaf(id) };
                } else if let Some(id) = self.camera.at(at.0, at.1).and_then(|p| self.diagram.handle_at(p)) {
                    self.clear_group();
                    self.select(id);
                    self.drag = Drag::Pending { at, grab: Grab::Box(id) };
                } else {
                    self.drag = Drag::Pending { at, grab: Grab::Nothing };
                }
            }
            _ => {}
        }
    }

    fn toggle_in_group(&mut self, id: EntityId) {
        if !self.group.remove(&id) {
            self.group.insert(id);
        }
        self.redraw_selection(&[id]);
    }

    fn clear_group(&mut self) {
        let was: Vec<EntityId> = std::mem::take(&mut self.group).into_iter().collect();
        if !was.is_empty() {
            self.redraw_selection(&was);
        }
    }

    /// The pointer moved with the button down.
    fn drag_to(&mut self, to: (u16, u16)) {
        if let Drag::Pending { at, grab } = self.drag.clone() {
            if at == to {
                return;
            }
            self.drag = match grab {
                Grab::Nothing => Drag::Marquee { from: at, to },
                Grab::Leaf(id) | Grab::Box(id) => {
                    let moved: Vec<EntityId> = match grab {
                        Grab::Leaf(id) if self.group.contains(&id) => {
                            self.group.iter().copied().collect()
                        }
                        _ => vec![id],
                    };
                    // The level of every moved node, and every level above
                    // it: the boxes that grow to hold the move are at those.
                    let mut levels = std::collections::BTreeSet::new();
                    for &id in &moved {
                        let mut level = self.scene.parent.get(&id).copied();
                        loop {
                            levels.insert(level);
                            match level {
                                Some(c) => level = self.scene.parent.get(&c).copied(),
                                None => break,
                            }
                        }
                    }
                    let corner = self.camera.unclamped_at(at.0, at.1);
                    let rect = self.diagram.rect_of(id).unwrap_or_default();
                    let held = (id, (corner.0 - i32::from(rect.x), corner.1 - i32::from(rect.y)));
                    Drag::Moving { last: at, moved, levels, held }
                }
            };
        }
        match &mut self.drag {
            Drag::Marquee { to: end, .. } => *end = to,
            Drag::Moving { last, moved, levels, held } => {
                let by = (i32::from(to.0) - i32::from(last.0), i32::from(to.1) - i32::from(last.1));
                *last = to;
                let (moved, levels, held) = (moved.clone(), levels.clone(), *held);
                self.layout.nudge(&moved, by, self.zoom);
                let opts = Options { anchored: moved.into_iter().collect(), loose: levels };
                let anchor = Anchor::Node { id: held.0, corner: held.1, screen: (i32::from(to.0), i32::from(to.1)) };
                self.rebuild_with(anchor, &opts);
            }
            _ => {}
        }
    }

    /// Scroll so the held node's corner sits where it was relative to the
    /// pointer. Everything that did not move then stays where it was on
    /// screen, whatever the canvas did to its own origin.
    fn hold_under_pointer(&mut self, held: (EntityId, (i32, i32)), pointer: (i32, i32)) {
        let Some(rect) = self.diagram.rect_of(held.0) else { return };
        let v = self.camera.viewport;
        self.camera.offset = (
            i32::from(rect.x) + held.1.0 - (pointer.0 - i32::from(v.x)),
            i32::from(rect.y) + held.1.1 - (pointer.1 - i32::from(v.y)),
        );
        self.camera.clamp();
    }

    /// The button came up.
    fn drop_at(&mut self, at: (u16, u16)) {
        match std::mem::replace(&mut self.drag, Drag::Idle) {
            // The siblings make way now, and where everything lands is the
            // arrangement from here on.
            Drag::Moving { moved, held, .. } => {
                let opts =
                    Options { anchored: moved.iter().copied().collect(), loose: Default::default() };
                let anchor = Anchor::Node { id: held.0, corner: held.1, screen: (i32::from(at.0), i32::from(at.1)) };
                self.rebuild_with(anchor, &opts);
                // The picture is what the drop changed; the order is then
                // read back off it, for the nodes that moved and no others.
                self.remember_order(&moved);
            }
            Drag::Marquee { from, .. } => {
                let (a, b) = (self.camera.unclamped_at(from.0, from.1), self.camera.unclamped_at(at.0, at.1));
                let (x0, x1) = (a.0.min(b.0).max(0), a.0.max(b.0).max(0));
                let (y0, y1) = (a.1.min(b.1).max(0), a.1.max(b.1).max(0));
                let swept = Rect::new(x0 as u16, y0 as u16, (x1 - x0 + 1) as u16, (y1 - y0 + 1) as u16);
                let mut chosen: Vec<(EntityId, Rect)> = self
                    .diagram
                    .leaves_in(swept)
                    .into_iter()
                    .filter_map(|id| Some((id, self.diagram.rect_of(id)?)))
                    .collect();
                chosen.sort_by_key(|(_, r)| (r.y, r.x));
                let was: Vec<EntityId> = std::mem::take(&mut self.group).into_iter().collect();
                self.group = chosen.iter().map(|(id, _)| *id).collect();
                let mut changed: Vec<EntityId> = was;
                changed.extend(self.group.iter().copied());
                self.redraw_selection(&changed);
                if let Some((first, _)) = chosen.first() {
                    self.select(*first);
                }
            }
            Drag::Pending { .. } | Drag::Idle => {}
        }
    }

    /// Where the marquee is on screen, while one is being swept.
    fn marquee(&self) -> Option<Rect> {
        let Drag::Marquee { from, to } = self.drag else { return None };
        let (x0, x1) = (from.0.min(to.0), from.0.max(to.0));
        let (y0, y1) = (from.1.min(to.1), from.1.max(to.1));
        Some(Rect::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1))
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
        let stats = &self.rendered.stats;
        let mut spans = vec![
            Span::styled(
                format!(" {leaves} nodes in {boxes} boxes "),
                Style::default().fg(Color::Black).bg(Color::Cyan),
            ),
            Span::raw(format!(" {} edges ", stats.submitted)),
        ];
        if stats.unroutable > 0 {
            spans.push(Span::styled(
                format!("({} cross something) ", stats.unroutable),
                Style::default().fg(Color::Yellow),
            ));
        }
        if !self.group.is_empty() {
            spans.push(Span::styled(
                format!("· {} grouped ", self.group.len()),
                Style::default().fg(Color::Cyan),
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
        if self.reloading {
            spans.push(Span::styled(
                "· re-reading the tree ".to_string(),
                Style::default().fg(Color::Yellow),
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
            Input::Pane(nvim_ui::Event::Notify(method, _)) if method == editor::WROTE => {
                self.reloading = true;
                self.trouble = None;
                let _ = self.reload.send(());
            }
            Input::Pane(nvim_ui::Event::Notify(..)) => {}
            Input::Reloaded(result) => self.reloaded(*result),
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
            // The arrows are the focus; with shift they pan, with option or
            // control they carry the focused node along. Both, because macOS
            // keeps ctrl-arrows for switching spaces and no terminal there
            // ever sees them. A step is one row or two columns, the same
            // distance on screen either way.
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => {
                let dir = match code {
                    KeyCode::Up => (0, -1),
                    KeyCode::Down => (0, 1),
                    KeyCode::Left => (-1, 0),
                    _ => (1, 0),
                };
                if mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                    self.shove((dir.0 * 2, dir.1));
                } else if mods.contains(KeyModifiers::SHIFT) {
                    self.camera.scroll(dir.0 * 4, dir.1);
                } else {
                    self.move_focus(dir);
                }
            }
            KeyCode::Char('k') => self.camera.scroll(0, -1),
            KeyCode::Char('j') => self.camera.scroll(0, 1),
            KeyCode::Char('h') => self.camera.scroll(-4, 0),
            KeyCode::Char('l') => self.camera.scroll(4, 0),
            KeyCode::PageUp => self.camera.scroll(0, -page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.camera.scroll(0, page),
            // Back to the beginning is the same place the view opens at, so
            // `g` and `c` land together rather than disagreeing about where
            // the start of a diagram narrower than the screen is.
            KeyCode::Home | KeyCode::Char('g') | KeyCode::Char('c') => self.camera.center(),
            KeyCode::End | KeyCode::Char('G') => self.camera.bottom(),
            KeyCode::Tab => self.focus_in(),
            KeyCode::BackTab => self.focus_out(),
            KeyCode::Char('n') => self.step_selection(true),
            KeyCode::Char('p') => self.step_selection(false),
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

/// Reading the tree, and whether anything may be said about it while it
/// happens. At startup the terminal is still the shell's and an indexer run
/// worth watching; once the diagram is on screen, a word from the indexer
/// would be painted over the top of it.
fn load_graph(args: &Args, root: &std::path::Path, progress: Progress) -> Result<EntityGraph> {
    match (&args.scip, args.treesitter) {
        (Some(index), _) => load_scip(index, root),
        (None, true) => {
            if progress == Progress::Show {
                eprintln!("parsing {}...", root.display());
            }
            load_treesitter(root)
        }
        // Not a cached path: the freshness rule inside `working_tree_index` is
        // what makes a stale index get rebuilt rather than silently reused.
        (None, false) => load_scip(&working_tree_index(root, progress)?, root),
    }
}

#[cfg(feature = "scip")]
use scip_producer::index::{Progress, working_tree_index};

/// Stands in for the real one, so the rest of this file does not have to know
/// which producers were built in.
#[cfg(not(feature = "scip"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Progress {
    Show,
    Quiet,
}

#[cfg(not(feature = "scip"))]
fn working_tree_index(
    _root: &std::path::Path,
    _progress: Progress,
) -> Result<std::path::PathBuf> {
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
    let graph = load_graph(&args, &root, Progress::Show)?;
    // The same read, ready to be done again when the pane says a file was
    // written. With SCIP that re-indexes, which is why it is never on the
    // thread that draws -- and why it is done without a word: the terminal
    // now has a diagram on it.
    let loader: Loader = {
        let (args, root) = (args, root.clone());
        std::sync::Arc::new(move || load_graph(&args, &root, Progress::Quiet))
    };

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

        let mut app = App::new(graph, viewport, root, inputs, loader);
        run(&mut terminal, &mut app, &queue)
    })();
    restore();
    result
}

fn restore() {
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
}

/// The sweep in progress: a dotted outline, so what it will take in is
/// visible before the button comes up.
fn draw_marquee(buf: &mut Buffer, r: Rect) {
    if r.width == 0 || r.height == 0 {
        return;
    }
    let st = Style::default().fg(Color::Cyan);
    for x in r.x..r.right() {
        for y in [r.y, r.bottom() - 1] {
            buf[(x, y)].set_symbol("┄").set_style(st);
        }
    }
    for y in r.y..r.bottom() {
        for x in [r.x, r.right() - 1] {
            buf[(x, y)].set_symbol("┆").set_style(st);
        }
    }
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

/// Did the user mean something by it? Moving the pointer with no button down
/// is reported too, and means nothing here.
fn is_gesture(kind: MouseEventKind) -> bool {
    matches!(
        kind,
        MouseEventKind::Down(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Up(_)
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
            // Positions persist, so a window that changed size shows more
            // or less of the same picture rather than a new one.
            app.camera.resize(graph);
        }
        if graph.width > 0 {
            render::blit(&app.canvas, frame.buffer_mut(), graph, app.camera.offset);
            if let Some(m) = app.marquee() {
                draw_marquee(frame.buffer_mut(), m.intersection(graph));
            }
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
    use entity_graph::EntityKind::{File, Folder, Function};
    use graph_tui::layout::INSET;
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
        // Nothing on disk stands behind this graph, so a reload could only
        // lie about it.
        let loader: Loader =
            std::sync::Arc::new(|| anyhow::bail!("this fixture has no tree to re-read"));
        App::new(graph, viewport, ".".into(), inputs, loader)
    }

    fn app() -> App {
        let mut app = app_with(fixture(), Rect::new(0, 0, 60, 20));
        // Expand to the files; the root alone has nothing to scroll or select.
        for _ in 0..2 {
            for leaf in app.cursor.coalesced(&app.graph).leaves {
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

    /// Expanding changes what is in the picture, not where the focus is: the
    /// node opens into a box and stays focused, and tab is the step inside.
    /// It once threw the focus back to the diagram's first leaf instead.
    #[test]
    fn expanding_a_node_keeps_it_focused_and_tab_goes_inside() {
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
        assert_eq!(app.selected, Some(right), "expanding moved the focus off the node that was opened");
        assert!(
            app.diagram.nodes.iter().any(|n| n.is_box && n.id == right),
            "the opened node is not drawn as a box"
        );
        app.key(KeyCode::Tab, KeyModifiers::NONE);
        let now = app.selected.expect("something is selected");
        assert!(is_under(&app.graph, now, right) && now != right, "tab did not go into the opened box");
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

    fn press_at(app: &mut App, col: u16, row: u16) {
        app.mouse(wheel(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE, col, row));
    }

    fn drag_to(app: &mut App, col: u16, row: u16) {
        app.mouse(wheel(MouseEventKind::Drag(MouseButton::Left), KeyModifiers::NONE, col, row));
    }

    fn release_at(app: &mut App, col: u16, row: u16) {
        app.mouse(wheel(MouseEventKind::Up(MouseButton::Left), KeyModifiers::NONE, col, row));
    }

    /// The screen cell over a diagram cell.
    fn screen(app: &App, x: u16, y: u16) -> (u16, u16) {
        (
            (i32::from(x) - app.camera.offset.0 + i32::from(app.camera.viewport.x)) as u16,
            (i32::from(y) - app.camera.offset.1 + i32::from(app.camera.viewport.y)) as u16,
        )
    }

    /// Opening a node grows it down and right from where it is, on the
    /// canvas and on the screen alike. What is beside it makes way along
    /// the row; what is left of it does not move at all. Then closing it
    /// shrinks it in place and moves nothing else.
    #[test]
    fn expanding_a_node_keeps_it_and_what_is_left_of_it_where_they_were() {
        let rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> = vec![
            ("root", Folder, None),
            ("a.rs", File, Some(0)),
            ("b.rs", File, Some(0)),
            ("c.rs", File, Some(0)),
            ("f", Function, Some(2)),
            ("g", Function, Some(2)),
            ("h", Function, Some(2)),
        ];
        let mut app = app_with(graph_from_parents(&rows, &[]), Rect::new(0, 0, 120, 40));
        app.key(KeyCode::Enter, KeyModifiers::NONE);
        let (a, b, c) = (EntityId(1), EntityId(2), EntityId(3));
        app.select(b);
        let rect = |app: &App, id| app.diagram.rect_of(id).unwrap();
        let (a_was, b_was, c_was) = (rect(&app, a), rect(&app, b), rect(&app, c));
        assert!(a_was.y == b_was.y && b_was.y == c_was.y, "fixture: a row");
        assert!(a_was.right() < b_was.x && b_was.right() < c_was.x, "fixture: a, b, c in order");
        let b_screen = screen(&app, b_was.x, b_was.y);

        app.key(KeyCode::Enter, KeyModifiers::NONE);
        let (a_now, b_now, c_now) = (rect(&app, a), rect(&app, b), rect(&app, c));
        assert!(app.diagram.nodes.iter().any(|n| n.id == b && n.is_box), "b should be a box");
        assert!(b_now.width > b_was.width && b_now.height > b_was.height, "b did not grow");
        assert_eq!((b_now.x, b_now.y), (b_was.x, b_was.y), "the opened node moved on the canvas");
        assert_eq!(screen(&app, b_now.x, b_now.y), b_screen, "the opened node moved on the screen");
        assert_eq!(a_now, a_was, "a, left of the opened node, moved");
        assert_eq!(c_now.y, c_was.y, "c was pushed out of its row");
        assert!(c_now.x >= b_now.right(), "c did not make way");

        app.select(EntityId(4));
        app.key(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(rect(&app, b), b_was, "closing did not put b back as it was");
        assert_eq!(rect(&app, a), a_was);
        assert_eq!(rect(&app, c), c_now, "closing moved c, which the user never touched");
    }

    /// Dragging a node moves it by what the pointer moved, the edges come
    /// with it, and it stays there afterwards -- through a zoom and back.
    /// The node under the pointer holds still on screen while everything
    /// else does too, which is the whole point of a drag.
    #[test]
    fn dragging_a_leaf_moves_it_and_keeps_it_there() {
        let rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> = vec![
            ("root", Folder, None),
            ("a.rs", File, Some(0)),
            ("b.rs", File, Some(0)),
            ("c.rs", File, Some(0)),
        ];
        let mut app = app_with(graph_from_parents(&rows, &[(1, 2, Call)]), Rect::new(0, 0, 80, 40));
        app.key(KeyCode::Enter, KeyModifiers::NONE); // root -> a, b, c
        let (a, c) = (EntityId(1), EntityId(3));
        let a_was = app.diagram.rect_of(a).unwrap();
        let c_was = app.diagram.rect_of(c).unwrap();
        let grab = screen(&app, a_was.x + 1, a_was.y + 1);

        press_at(&mut app, grab.0, grab.1);
        assert_eq!(app.selected, Some(a), "pressing a node selects it");
        let c_screen_was = screen(&app, c_was.x, c_was.y);
        drag_to(&mut app, grab.0 + 5, grab.1 + 12);
        assert!(matches!(app.drag, Drag::Moving { .. }));
        // In flight, the node is under the pointer where it was grabbed, and
        // the one that was not touched has not moved on screen -- whatever
        // the canvas did to its own origin underneath.
        let a_now = app.diagram.rect_of(a).unwrap();
        assert_eq!(screen(&app, a_now.x + 1, a_now.y + 1), (grab.0 + 5, grab.1 + 12), "the node left the pointer");
        let c_now = app.diagram.rect_of(c).unwrap();
        assert_eq!(screen(&app, c_now.x, c_now.y), c_screen_was, "an unmoved node moved on screen");

        release_at(&mut app, grab.0 + 5, grab.1 + 12);
        assert_eq!(app.drag, Drag::Idle);
        let a_dropped = app.diagram.rect_of(a).unwrap();
        let c_dropped = app.diagram.rect_of(c).unwrap();
        let apart = |p: Rect, q: Rect| (i32::from(p.x) - i32::from(q.x), i32::from(p.y) - i32::from(q.y));
        let was = apart(a_was, c_was);
        assert_eq!(apart(a_dropped, c_dropped), (was.0 + 5, was.1 + 12), "the drop did not land where the pointer let go");
        // The edge a -> b came along: an arrowhead still touches b's frame.
        // Which side depends on where a landed, so any side counts.
        let b = app.diagram.rect_of(EntityId(2)).unwrap();
        let ring = (b.x - 1..=b.right())
            .flat_map(|x| [(x, b.y - 1), (x, b.bottom())])
            .chain((b.y..b.bottom()).flat_map(|y| [(b.x - 1, y), (b.right(), y)]));
        let heads = ring.filter(|&p| matches!(app.canvas[p].symbol(), "▼" | "▲" | "◀" | "▶")).count();
        assert_eq!(heads, 1, "the edge did not follow the drag");

        // Zoom out and back in: the arrangement is the user's now, and it holds.
        app.control(Control::Zoom);
        app.control(Control::Zoom);
        app.control(Control::Zoom);
        assert_eq!(app.zoom, Zoom::Close);
        assert_eq!(apart(app.diagram.rect_of(a).unwrap(), app.diagram.rect_of(c).unwrap()), (was.0 + 5, was.1 + 12));

        // Control: with nothing dragged, a click does not move anything.
        let before: Vec<_> = app.diagram.nodes.iter().map(|n| (n.id, n.rect)).collect();
        let cs = screen(&app, c_dropped.x + 1, c_dropped.y + 1);
        press_at(&mut app, cs.0, cs.1);
        release_at(&mut app, cs.0, cs.1);
        let after: Vec<_> = app.diagram.nodes.iter().map(|n| (n.id, n.rect)).collect();
        assert_eq!(before, after);
    }

    /// Dropping a node on a sibling does not leave them on top of each
    /// other: the sibling makes way, and the dropped one stays where it
    /// was put.
    #[test]
    fn a_node_dropped_on_another_pushes_it_aside() {
        let rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> = vec![
            ("root", Folder, None),
            ("a.rs", File, Some(0)),
            ("b.rs", File, Some(0)),
        ];
        let mut app = app_with(graph_from_parents(&rows, &[]), Rect::new(0, 0, 80, 40));
        app.key(KeyCode::Enter, KeyModifiers::NONE);
        let (a, b) = (EntityId(1), EntityId(2));
        let (a_was, b_was) = (app.diagram.rect_of(a).unwrap(), app.diagram.rect_of(b).unwrap());
        assert_eq!(a_was.y, b_was.y, "fixture: side by side");
        let grab = screen(&app, a_was.x + 1, a_was.y + 1);
        let dx = i32::from(b_was.x) - i32::from(a_was.x);
        press_at(&mut app, grab.0, grab.1);
        drag_to(&mut app, (i32::from(grab.0) + dx) as u16, grab.1);
        let a_flight = app.diagram.rect_of(a).unwrap();
        let b_flight = app.diagram.rect_of(b).unwrap();
        assert!(a_flight.intersects(b_flight), "in flight the two may overlap; nothing is pushed yet");
        release_at(&mut app, (i32::from(grab.0) + dx) as u16, grab.1);
        let a_now = app.diagram.rect_of(a).unwrap();
        let b_now = app.diagram.rect_of(b).unwrap();
        assert!(!a_now.intersects(b_now), "the drop left two nodes on top of each other");
        assert_eq!(screen(&app, a_now.x + 1, a_now.y + 1), ((i32::from(grab.0) + dx) as u16, grab.1), "the dropped node was the one pushed");
    }

    /// Sweeping a rectangle over some leaves makes them a group; dragging
    /// one of them then moves them all, and shift-click takes one out.
    #[test]
    fn a_marquee_gathers_a_group_that_moves_together() {
        let mut app = app();
        let mut leaves: Vec<Rect> = app.diagram.nodes.iter().filter(|n| !n.is_box).map(|n| n.rect).collect();
        leaves.sort_by_key(|r| (r.y, r.x));
        assert!(leaves.len() >= 3);
        // Sweep from the empty row under the first row up through it. The
        // row above is the box's title, which is a handle, not nothing.
        let row_y = leaves[0].y;
        let in_row: Vec<Rect> = leaves.iter().copied().filter(|r| r.y == row_y).collect();
        assert!(in_row.len() >= 2, "fixture: a row with several leaves");
        let (first, last) = (in_row[0], in_row[in_row.len() - 1]);
        assert_eq!(app.diagram.leaf_at((first.x, first.bottom())), None, "fixture: a free row under the leaves");
        let from = screen(&app, first.x, first.bottom());
        let to = screen(&app, last.right() - 1, last.y + 1);
        press_at(&mut app, from.0, from.1);
        drag_to(&mut app, to.0, to.1);
        assert!(matches!(app.drag, Drag::Marquee { .. }), "a drag on nothing should sweep");
        assert!(app.marquee().is_some());
        release_at(&mut app, to.0, to.1);
        let group: Vec<EntityId> = app.group.iter().copied().collect();
        assert_eq!(group.len(), in_row.len(), "the sweep should take exactly the row");
        for r in &in_row {
            assert!(group.contains(&app.diagram.leaf_at((r.x + 1, r.y + 1)).unwrap()));
        }

        // Drag one member: they all move by the same amount, measured against
        // a leaf that stayed put -- the canvas re-homes itself on its own
        // top-left, so absolute coordinates say nothing.
        let ids: Vec<EntityId> = in_row.iter().map(|r| app.diagram.leaf_at((r.x + 1, r.y + 1)).unwrap()).collect();
        let other = leaves.iter().find(|r| r.y != row_y).expect("fixture: another row");
        let other_id = app.diagram.leaf_at((other.x + 1, other.y + 1)).unwrap();
        let rel = |app: &App, id: EntityId| {
            let (r, o) = (app.diagram.rect_of(id).unwrap(), app.diagram.rect_of(other_id).unwrap());
            (i32::from(r.x) - i32::from(o.x), i32::from(r.y) - i32::from(o.y))
        };
        let before: Vec<(i32, i32)> = ids.iter().map(|id| rel(&app, *id)).collect();
        let first = app.diagram.rect_of(ids[0]).unwrap();
        let grab = screen(&app, first.x + 1, first.y + 1);
        press_at(&mut app, grab.0, grab.1);
        drag_to(&mut app, grab.0, grab.1 + 3);
        release_at(&mut app, grab.0, grab.1 + 3);
        for (id, b) in ids.iter().zip(&before) {
            assert_eq!(rel(&app, *id), (b.0, b.1 + 3), "a group member did not move with the group");
        }
        assert_eq!(app.group.len(), in_row.len(), "dragging the group should keep it");

        // Shift-click takes one out; a plain click elsewhere drops the group.
        let out = app.diagram.rect_of(ids[0]).unwrap();
        let at = screen(&app, out.x + 1, out.y + 1);
        app.mouse(wheel(MouseEventKind::Down(MouseButton::Left), KeyModifiers::SHIFT, at.0, at.1));
        release_at(&mut app, at.0, at.1);
        assert!(!app.group.contains(&ids[0]));
        assert_eq!(app.group.len(), in_row.len() - 1);
        let other = app.diagram.rect_of(other_id).unwrap();
        let at = screen(&app, other.x + 1, other.y + 1);
        press_at(&mut app, at.0, at.1);
        release_at(&mut app, at.0, at.1);
        assert!(app.group.is_empty(), "clicking outside the group should let it go");
    }

    fn rank_of(app: &App, id: EntityId) -> graph_tui::rank::Rank {
        app.layout.rank_of(id).cloned().expect("every drawn node is ranked")
    }

    /// A drop changes the picture, and the order is read back off it
    /// afterwards -- for the node that moved, and nobody else. Dragging the
    /// last of a row in among the first two must leave it ordered between
    /// them while their own ranks are untouched, which is the whole reason a
    /// rank is a list of parts rather than a number: there is always room
    /// between two of them, so an insertion never renumbers a neighbour.
    #[test]
    fn a_drop_is_remembered_as_an_order_between_its_new_neighbours() {
        let rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> = vec![
            ("root", Folder, None),
            ("a.rs", File, Some(0)),
            ("b.rs", File, Some(0)),
            ("c.rs", File, Some(0)),
        ];
        let mut app = app_with(graph_from_parents(&rows, &[]), Rect::new(0, 0, 100, 40));
        app.key(KeyCode::Enter, KeyModifiers::NONE);
        let (a, b, c) = (EntityId(1), EntityId(2), EntityId(3));
        let (was_a, was_b, was_c) = (rank_of(&app, a), rank_of(&app, b), rank_of(&app, c));
        assert!(was_a < was_b && was_b < was_c, "the row should start in reading order");

        let ar = app.diagram.rect_of(a).expect("a is drawn");
        let cr = app.diagram.rect_of(c).expect("c is drawn");
        let grab = screen(&app, cr.x + 1, cr.y + 1);
        let onto = screen(&app, ar.right() + 1, ar.y + 1);
        press_at(&mut app, grab.0, grab.1);
        drag_to(&mut app, onto.0, onto.1);
        release_at(&mut app, onto.0, onto.1);

        let now = |id| rank_of(&app, id);
        assert!(now(a) < now(c), "c did not come to rest after a");
        assert!(now(c) < now(b), "c did not come to rest before b");
        assert_eq!(now(a), was_a, "a was renumbered by an insertion beside it");
        assert_eq!(now(b), was_b, "b was renumbered by an insertion beside it");
        assert_ne!(now(c), was_c, "c moved but its order was not remembered");
    }

    /// A box draws itself all the way round -- title rows, sides and bottom
    /// -- and every one of those cells is the box's rather than its
    /// children's, so a click on any of them takes hold of it. The room it
    /// keeps for its children is the only part that is not the box, which is
    /// what leaves a sweep inside a box still able to start.
    #[test]
    fn clicking_any_edge_of_a_box_selects_it() {
        let rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> = vec![
            ("root", Folder, None),
            ("left", Folder, Some(0)),
            ("right", Folder, Some(0)),
            ("a.rs", File, Some(1)),
            ("b.rs", File, Some(2)),
        ];
        let mut app = app_with(graph_from_parents(&rows, &[]), Rect::new(0, 0, 100, 40));
        app.key(KeyCode::Enter, KeyModifiers::NONE);
        app.select(EntityId(1));
        app.key(KeyCode::Enter, KeyModifiers::NONE); // left -> a.rs
        let (left, a) = (EntityId(1), EntityId(3));
        let r = app.diagram.rect_of(left).expect("the box is drawn");

        for (where_, point) in [
            ("its bottom", (r.x + 1, r.bottom() - 1)),
            ("its left side", (r.x, r.y + INSET.1 as u16)),
            ("its right side", (r.right() - 1, r.y + INSET.1 as u16)),
            ("its title", (r.x + 1, r.y + 1)),
        ] {
            app.select(a);
            let at = screen(&app, point.0, point.1);
            press_at(&mut app, at.0, at.1);
            assert_eq!(app.selected, Some(left), "clicking {where_} did not select the box");
            assert!(
                matches!(app.drag, Drag::Pending { grab: Grab::Box(id), .. } if id == left),
                "clicking {where_} did not take hold of the box"
            );
            release_at(&mut app, at.0, at.1);
        }

        // The room inside is the children's, so a press there still starts a
        // sweep rather than grabbing the box around it.
        let gap = screen(&app, r.right() - 2, r.bottom() - 2);
        press_at(&mut app, gap.0, gap.1);
        assert!(matches!(app.drag, Drag::Pending { grab: Grab::Nothing, .. }), "the box swallowed a sweep");
        release_at(&mut app, gap.0, gap.1);
    }

    /// A box is dragged by its title rows and everything in it comes along.
    #[test]
    fn dragging_a_box_by_its_title_moves_what_is_in_it() {
        let rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> = vec![
            ("root", Folder, None),
            ("left", Folder, Some(0)),
            ("right", Folder, Some(0)),
            ("a.rs", File, Some(1)),
            ("b.rs", File, Some(2)),
        ];
        let mut app = app_with(graph_from_parents(&rows, &[]), Rect::new(0, 0, 100, 40));
        app.key(KeyCode::Enter, KeyModifiers::NONE);
        app.select(EntityId(1));
        app.key(KeyCode::Enter, KeyModifiers::NONE); // left -> a.rs
        let (left, a) = (EntityId(1), EntityId(3));
        let left_was = app.diagram.rect_of(left).unwrap();
        let a_was = app.diagram.rect_of(a).unwrap();
        let right_was = app.diagram.rect_of(EntityId(2)).unwrap();
        let grab = screen(&app, left_was.x + 1, left_was.y + 1);
        press_at(&mut app, grab.0, grab.1);
        assert!(matches!(app.drag, Drag::Pending { grab: Grab::Box(id), .. } if id == left));
        drag_to(&mut app, grab.0, grab.1 + 6);
        release_at(&mut app, grab.0, grab.1 + 6);
        let left_now = app.diagram.rect_of(left).unwrap();
        let a_now = app.diagram.rect_of(a).unwrap();
        assert_eq!(
            (i32::from(a_now.x) - i32::from(left_now.x), i32::from(a_now.y) - i32::from(left_now.y)),
            (i32::from(a_was.x) - i32::from(left_was.x), i32::from(a_was.y) - i32::from(left_was.y)),
            "the child did not come with its box"
        );
        let right_now = app.diagram.rect_of(EntityId(2)).unwrap();
        assert_eq!(
            i32::from(left_now.y) - i32::from(right_now.y),
            i32::from(left_was.y) - i32::from(right_was.y) + 6,
            "the box did not move by the drag relative to its sibling"
        );
    }

    /// `L` forgets the arrangement: a dragged node goes back to where the
    /// ranking puts it.
    #[test]
    fn relayout_undoes_a_drag() {
        let mut app = app();
        let leaf = app.diagram.nodes.iter().find(|n| !n.is_box).unwrap();
        let (id, rect) = (leaf.id, leaf.rect);
        let fresh: Vec<_> = app.diagram.nodes.iter().map(|n| (n.id, n.rect)).collect();
        let grab = screen(&app, rect.x + 1, rect.y + 1);
        press_at(&mut app, grab.0, grab.1);
        drag_to(&mut app, grab.0 + 3, grab.1 + 8);
        release_at(&mut app, grab.0 + 3, grab.1 + 8);
        assert_ne!(app.diagram.rect_of(id).unwrap(), rect);
        app.key(KeyCode::Char('L'), KeyModifiers::NONE);
        let again: Vec<_> = app.diagram.nodes.iter().map(|n| (n.id, n.rect)).collect();
        assert_eq!(fresh, again, "re-layout did not restore the ranked picture");
    }

    #[test]
    fn n_walks_every_node_boxes_included_and_comes_back_round() {
        let mut app = app();
        let nodes = app.diagram.nodes.len();
        assert!(nodes >= 2, "fixture should draw several nodes");
        let first = app.selected.expect("something is selected at rest");
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..nodes {
            seen.insert(app.selected.unwrap());
            app.key(KeyCode::Char('n'), KeyModifiers::NONE);
        }
        assert_eq!(seen.len(), nodes, "n skipped a node");
        assert_eq!(app.selected, Some(first), "n did not wrap around");
    }

    /// The arrows walk the siblings of a level by where they are drawn; tab
    /// and shift-tab step through the containment, so a box is a thing the
    /// focus can rest on.
    #[test]
    fn arrows_walk_siblings_and_tab_goes_in_and_out_of_boxes() {
        let rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> = vec![
            ("root", Folder, None),
            ("a.rs", File, Some(0)),
            ("b.rs", File, Some(0)),
            ("c.rs", File, Some(0)),
        ];
        let mut app = app_with(graph_from_parents(&rows, &[]), Rect::new(0, 0, 80, 40));
        app.key(KeyCode::Enter, KeyModifiers::NONE); // root -> a, b, c in one row
        let (root, a, b, c) = (EntityId(0), EntityId(1), EntityId(2), EntityId(3));
        let x = |app: &App, id| app.diagram.rect_of(id).unwrap().x;
        assert!(x(&app, a) < x(&app, b) && x(&app, b) < x(&app, c), "fixture should rank a b c along a row");

        app.key(KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(app.selected, Some(root), "shift-tab did not focus the box around");
        app.key(KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(app.selected, Some(root), "there is nothing outside the root");
        app.key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.selected, Some(a), "tab did not go to the first child");
        app.key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.selected, Some(a), "a leaf with nothing in it has nowhere to go");

        app.key(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.selected, Some(b));
        app.key(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.selected, Some(c));
        app.key(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.selected, Some(c), "nothing is right of the last one");
        app.key(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(app.selected, Some(b));
        app.key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.selected, Some(b), "nothing is below a one-row level");

        let offset = app.camera.offset;
        app.key(KeyCode::Down, KeyModifiers::SHIFT);
        assert_eq!(app.selected, Some(b), "shift-arrow moved the focus");
        assert_ne!(app.camera.offset, offset, "shift-arrow did not scroll");
    }

    /// Two nodes sit side by side on one row with a third centred below
    /// them, so going back up is a question distance can barely answer -- and
    /// answers the same way whichever of the pair the focus came down from.
    /// The way it came is the better answer, and it survives being re-walked.
    #[test]
    fn going_back_up_returns_to_the_node_you_came_down_from() {
        let mut app = app();
        let (file_00, file_01, tests_rs) = (EntityId(2), EntityId(3), EntityId(10));

        app.select(file_01);
        app.key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.selected, Some(file_00), "fixture should make distance alone answer file_00.rs");

        app.select(tests_rs);
        app.key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.selected, Some(file_01), "down from either of the pair lands on the one below");
        app.key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.selected, Some(tests_rs), "up went by distance instead of back the way it came");

        app.select(file_00);
        app.key(KeyCode::Down, KeyModifiers::NONE);
        app.key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.selected, Some(file_00), "the second visit did not replace the first");
    }

    /// The trail only ever picks between nodes the arrow was already willing
    /// to go to. Once the one it remembers is not among them, distance has
    /// the answer back.
    #[test]
    fn a_remembered_neighbour_that_is_gone_gives_the_answer_back_to_distance() {
        let mut app = app();
        let (file_00, file_01, tests_rs) = (EntityId(2), EntityId(3), EntityId(10));

        app.select(tests_rs);
        app.key(KeyCode::Down, KeyModifiers::NONE);
        app.key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.selected, Some(tests_rs), "the trail should be holding tests.rs");

        app.key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(!drawn(&app, tests_rs), "x did not hide the node the trail remembers");

        app.select(file_01);
        app.key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.selected, Some(file_00), "up followed a node that is no longer in the picture");
    }

    /// Tab asks to be inside something, so a shut node opens on the way in
    /// rather than making the user press enter first. Leaving a box and going
    /// back into it returns to the child that was left, not to the first one.
    #[test]
    fn tab_opens_a_shut_node_and_returns_to_the_child_it_left() {
        let mut app = app_with(fixture(), Rect::new(0, 0, 60, 20));
        let (root, inner) = (EntityId(0), EntityId(1));
        app.select(root);
        assert!(!app.scene.is_box(root), "the fixture should start shut");

        app.key(KeyCode::Tab, KeyModifiers::NONE);
        assert!(app.scene.is_box(root), "tab did not open the node it was going into");
        assert_eq!(app.selected, Some(inner), "tab did not land inside what it opened");

        app.key(KeyCode::Tab, KeyModifiers::NONE);
        let first = app.selected.expect("tab opened the folder and went in");
        assert!(is_under(&app.graph, first, inner), "tab left the box it opened");

        app.key(KeyCode::Down, KeyModifiers::NONE);
        let left_at = app.selected.expect("the focus is still on a node");
        assert_ne!(left_at, first, "fixture should let the focus move off the first child");

        app.key(KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(app.selected, Some(inner), "shift-tab did not go out to the box around");
        app.key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.selected, Some(left_at), "tab went to the first child, not the one it left");
    }

    /// Option- or control-arrow is a drag by keyboard: the node lands a step over, the
    /// rest stay, and the screen does not lurch under the reader.
    #[test]
    fn ctrl_arrow_moves_the_focused_node_and_nothing_else() {
        let rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> = vec![
            ("root", Folder, None),
            ("a.rs", File, Some(0)),
            ("b.rs", File, Some(0)),
        ];
        let mut app = app_with(graph_from_parents(&rows, &[]), Rect::new(0, 0, 80, 40));
        app.key(KeyCode::Enter, KeyModifiers::NONE);
        let (a, b) = (EntityId(1), EntityId(2));
        app.select(a);
        let (a_was, b_was) = (app.diagram.rect_of(a).unwrap(), app.diagram.rect_of(b).unwrap());
        let a_screen = screen(&app, a_was.x, a_was.y);
        let b_screen = screen(&app, b_was.x, b_was.y);

        app.key(KeyCode::Down, KeyModifiers::CONTROL);
        let (a_now, b_now) = (app.diagram.rect_of(a).unwrap(), app.diagram.rect_of(b).unwrap());
        let apart = |p: Rect, q: Rect| (i32::from(p.x) - i32::from(q.x), i32::from(p.y) - i32::from(q.y));
        let was = apart(a_was, b_was);
        assert_eq!(apart(a_now, b_now), (was.0, was.1 + 1), "the node did not move one row down");
        assert_eq!(screen(&app, a_now.x, a_now.y), (a_screen.0, a_screen.1 + 1), "the node did not move on screen by the step");
        assert_eq!(screen(&app, b_now.x, b_now.y), b_screen, "the other node moved on screen");
        assert_eq!(app.selected, Some(a));

        // Option-arrow is the same move: on a Mac, ctrl-arrow never arrives.
        // Measured against the other node: `a` is the leftmost child, and
        // the box hugs its children, so on the canvas it is the rest that
        // shift; on screen, held by the camera, `a` is what moves.
        let a_screen = screen(&app, a_now.x, a_now.y);
        app.key(KeyCode::Right, KeyModifiers::ALT);
        let (a_then, b_then) = (app.diagram.rect_of(a).unwrap(), app.diagram.rect_of(b).unwrap());
        assert_eq!(apart(a_then, b_then), (was.0 + 2, was.1 + 1), "option-arrow did not move the node two columns");
        assert_eq!(screen(&app, a_then.x, a_then.y), (a_screen.0 + 2, a_screen.1), "the node did not move right on screen");
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

    /// At rest the fixture's focus is on the root, which is a box once it is
    /// expanded; hiding or scoping to that would act on the whole picture.
    fn focus_a_leaf(app: &mut App) -> EntityId {
        let leaf = app.diagram.nodes.iter().find(|n| !n.is_box).expect("a drawn leaf").id;
        app.select(leaf);
        leaf
    }

    /// Hiding is the switch a user reaches for most, and `show all` is the
    /// only way back from it -- so they are tested as the pair they are.
    #[test]
    fn hiding_the_selection_takes_it_out_until_show_all_brings_it_back() {
        let mut app = app();
        let gone = focus_a_leaf(&mut app);
        app.key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(!drawn(&app, gone), "x did not hide the selected node");
        assert!(app.diagram.nodes.iter().any(|n| !n.is_box), "x hid everything");

        app.key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(drawn(&app, gone), "show all did not bring the hidden node back");
    }

    #[test]
    fn scoping_keeps_only_the_selection_and_show_all_undoes_it() {
        let mut app = app();
        let kept = focus_a_leaf(&mut app);
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
        // One nvim starts at a time. Several started together by parallel
        // tests would, about one run in three, include one that never
        // answered its first request; alone or in turn they always do.
        static SPAWNING: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _one_at_a_time = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        app.control(Control::Editor);
        app
    }

    /// Code is written eighty columns wide. A pane wider than that shows
    /// margin the diagram could have used, so eighty is what it opens with;
    /// line numbers sit before the code, so they come on top; and a screen
    /// that cannot spare that much is split in half as before.
    #[test]
    fn the_pane_opens_eighty_columns_of_code_wide_or_half_the_screen() {
        if !have_nvim() {
            return;
        }
        let mut app = app_with_pane();
        let editor = app.editor.as_mut().expect("the pane opened");
        assert_eq!(editor.width(), 60, "a 120-column body cannot spare eighty");

        assert_eq!(editor.natural_width(200), 80, "--clean draws nothing before the code");
        editor.nvim().call("nvim_command", vec!["set number".into()]).expect("nvim answers");
        assert_eq!(editor.natural_width(200), 84, "the number column is on top of the eighty");
        assert_eq!(editor.natural_width(60), 60);
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
        app.press(press(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.selected, selected, "n moved the diagram's selection");

        app.press(press(KeyCode::Char('w'), KeyModifiers::CONTROL));
        app.press(press(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Graph);

        app.press(press(KeyCode::Char('n'), KeyModifiers::NONE));
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

    /// The queue comes back with it: a test that sets a reload going has to
    /// be the loop, and the loop is what drains this.
    fn expanded_app(
        dir: &tempfile::TempDir,
        graph: EntityGraph,
    ) -> (App, std::sync::mpsc::Receiver<Input>) {
        let (inputs, queue) = std::sync::mpsc::channel();
        let root = dir.path().to_path_buf();
        let reread = root.clone();
        let loader: Loader =
            std::sync::Arc::new(move || treesitter_producer::graph_from_path(&reread));
        let mut app = App::new(graph, Rect::new(0, 0, 120, 20), root, inputs, loader);
        for _ in 0..4 {
            for leaf in app.cursor.coalesced(&app.graph).leaves {
                app.cursor.move_down(leaf, &app.graph);
            }
        }
        app.rebuild();
        app.body = Rect::new(0, 0, 120, 20);
        app.nvim_args = &["--clean"];
        (app, queue)
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
        let (mut app, _queue) = expanded_app(&dir, graph);
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
        let (mut app, _queue) = expanded_app(&dir, graph);
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

    /// Writing in the pane makes the diagram wrong, so it re-reads the tree
    /// -- and has to come back with the user still where they were. Ids are
    /// arena indices, so every one of them changed underneath.
    #[test]
    fn writing_in_the_pane_brings_the_diagram_up_to_date_without_losing_your_place() {
        if !have_nvim() {
            return;
        }
        let (dir, graph) = project("fn alpha() {}\n\nfn beta() {}\n");
        let (mut app, queue) = expanded_app(&dir, graph);
        app.control(Control::Editor);
        app.select(entity_named(&app, "beta").id);
        let held = app.cursor.leaves.len();
        assert!(app.graph.entities.iter().all(|e| e.name != "gamma"), "gamma is the new thing");

        // Written from underneath, then the pane is told it happened -- which
        // is what nvim's own autocmd does after a `:w`.
        std::fs::write(dir.path().join("lib.rs"), "fn alpha() {}\n\nfn beta() {}\n\nfn gamma() {}\n")
            .expect("writing the file");
        app.take(Input::Pane(nvim_ui::Event::Notify(editor::WROTE.into(), Vec::new())));
        assert!(app.reloading, "nothing was set off");

        let reloaded = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while app.reloading {
            assert!(std::time::Instant::now() < reloaded, "the reload never came back");
            if let Ok(input) = queue.recv_timeout(std::time::Duration::from_millis(50)) {
                app.take(input);
            }
        }

        assert!(app.trouble.is_none(), "reload said: {:?}", app.trouble);
        assert!(
            app.graph.entities.iter().any(|e| e.name == "gamma"),
            "the diagram never saw the new function"
        );
        assert_eq!(app.cursor.leaves.len(), held + 1, "the expansion was not carried across");
        assert_eq!(
            app.selected.and_then(|id| app.graph.get(id)).map(|e| e.name.as_str()),
            Some("beta"),
            "the selection did not survive the renumbering"
        );
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
