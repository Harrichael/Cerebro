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
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use graph_tui::camera::Camera;
use graph_tui::controls::{self, Control, NodeAction, State};
use graph_tui::label::Labels;
use graph_tui::placer::{self, Diagram};
use graph_tui::render::{self, Stats};
use graph_tui::view::{self, Settings};
use graph_tui::zoom::Zoom;

const HINT: &str =
    "? keys  ·  scroll pans, ⇧scroll sideways, ctrl-scroll zooms  ·  ↵ expand  ·  ⌫ collapse  ·  q quit";

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
    ("?", "this list"),
    ("q  /  esc  /  ctrl-c", "quit"),
];

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
}

impl App {
    fn new(graph: EntityGraph, viewport: Rect) -> Self {
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
        if self.selected == Some(id) {
            return;
        }
        let was = self.selected;
        self.selected = Some(id);
        if let Some(rect) = self.diagram.rect_of(id) {
            self.camera.reveal(rect);
        }
        self.redraw_selection(was);
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
        Line::from(spans)
    }

    fn key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let page = self.camera.viewport.height.max(1) as i32 / 2;
        match code {
            // While the key list is up it owns the keyboard, so a stray press
            // dismisses it rather than scrolling something the user cannot see.
            _ if self.help => self.help = false,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.quit = true,
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
        let mut app = App::new(graph, viewport);
        run(&mut terminal, &mut app)
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

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.quit {
        terminal.draw(|frame| {
            let area = frame.area();
            let body = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2));
            if body != app.camera.viewport {
                // Only width feeds the layout -- it is what a rank wraps
                // against. A change in height just shows more of the same
                // diagram, and re-routing every edge to learn that would cost
                // seconds on every drag of a window corner.
                let relaid = body.width != app.camera.viewport.width;
                app.camera.resize(body);
                if relaid {
                    app.rebuild();
                }
            }
            render::blit(&app.canvas, frame.buffer_mut(), body, app.camera.offset);
            // Drawn after the diagram and remembered, because the click that
            // works a switch arrives after the frame that showed it.
            app.panel = controls::draw(frame.buffer_mut(), body, |c| app.state(c));
            frame.render_widget(
                Paragraph::new(app.status()),
                Rect::new(area.x, area.bottom().saturating_sub(2), area.width, 1),
            );
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    HINT,
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
                ))),
                Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
            );
            if app.help {
                draw_keys(frame, area);
            }
        })?;
        // Mouse capture reports every twitch of the pointer. Those change
        // nothing, so they neither reach the app -- where any event dismisses
        // the key list -- nor cost a full redraw of the frame.
        loop {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.key(key.code, key.modifiers);
                    break;
                }
                Event::Mouse(ev) if is_gesture(ev.kind) => {
                    app.mouse(ev);
                    break;
                }
                Event::Resize(..) => break,
                _ => {}
            }
        }
    }
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

    fn app() -> App {
        let mut app = App::new(fixture(), Rect::new(0, 0, 60, 20));
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
        let mut app = App::new(fixture(), Rect::new(0, 0, 30, 20));
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
        let mut app = App::new(graph, Rect::new(0, 0, 80, 24));
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
        let mut app = App::new(graph_from_parents(&rows, &[(3, 4, Call)]), Rect::new(0, 0, 80, 24));
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
        let states: Vec<(Control, State)> =
            [Control::Zoom, Control::Tests, Control::OnePerPair, Control::Edges,
             Control::Hide, Control::Scope, Control::ShowAll, Control::CollapseAll]
                .into_iter()
                .map(|c| (c, app.state(c)))
                .collect();
        app.panel = controls::draw(&mut buf, area, |c| {
            states.iter().find(|(k, _)| *k == c).expect("every control has a state").1
        });
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

    /// The panel exists so the mouse can do what the keys do. A click on a
    /// switch has to reach the same code the key reaches.
    #[test]
    fn clicking_a_switch_does_what_its_key_does() {
        let mut app = app();
        draw_panel(&mut app);
        let row = app.panel.rect.y + 1 + 1; // zoom, then tests
        assert!(app.settings.show_tests);

        app.mouse(wheel(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
            app.panel.rect.x + 2,
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
        let mut app = App::new(fixture(), Rect::new(0, 0, 60, 20));
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
