//! Terminal 2D graph view of a code entity graph.
//!
//! The diagram is rendered once per change to a buffer its own size and the
//! visible window blitted from it, so scrolling never re-routes an edge.
//! Height is the axis that grows with zoom — width wraps against the viewport
//! — which is why panning is vertical first.

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
use graph_tui::controls::{self, Control, State};
use graph_tui::label::Labels;
use graph_tui::placer::{self, Diagram};
use graph_tui::render::{self, Stats};
use graph_tui::view::{self, Settings};

const HINT: &str =
    "? keys  ·  scroll pans, ⇧scroll sideways, ctrl-scroll zooms  ·  click selects  ·  q quit";

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
    ("click", "select a node"),
    ("scroll  ⇧scroll", "pan up and down, or left and right"),
    ("ctrl-scroll", "zoom in and out of the node under the pointer"),
    ("c", "centre the diagram (same as Home)"),
    ("↵  /  +", "zoom into the selected node"),
    ("⌫  /  -", "zoom back out"),
    ("?", "this list"),
    ("q  /  esc  /  ctrl-c", "quit"),
];

struct App {
    graph: EntityGraph,
    cursor: coalesce::Cursor,
    settings: Settings,
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
            diagram: Diagram { nodes: Vec::new(), edges: Vec::new(), width: 0, height: 0 },
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

    fn labels(&self) -> Labels<'_> {
        Labels::new(&self.graph)
    }

    fn rebuild(&mut self) {
        let picture = view::apply(&self.graph, &self.cursor.coalesced(), &self.settings);
        self.diagram = placer::place(&self.labels(), &picture, self.camera.viewport.width.max(20));
        // Only a leaf can be selected: zooming acts on cursor leaves, and a
        // box is an ancestor of one. Keeping a selection that has become a box
        // leaves every later `step_selection` unable to find its own starting
        // point, so tab silently returns to the first leaf each press.
        let still_a_leaf =
            self.diagram.nodes.iter().any(|n| !n.is_box && Some(n.id) == self.selected);
        if !still_a_leaf {
            self.selected = self.diagram.nodes.iter().find(|n| !n.is_box).map(|n| n.id);
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
        self.camera.fit(self.diagram.extent());
        if let Some(rect) = self.selected.and_then(|id| self.diagram.rect_of(id)) {
            self.camera.reveal(rect);
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
            Control::Tests => State::Switch(self.settings.show_tests),
            Control::OnePerPair => State::Switch(self.settings.one_per_pair),
            Control::Edges => State::Switch(self.show_edges),
            Control::Hide | Control::Scope => State::Action(self.selected.is_some()),
            Control::ShowAll => {
                State::Undo(self.settings.hidden.len() + usize::from(self.settings.scope.is_some()))
            }
            Control::Reset => State::Action(self.cursor.active().iter().any(|&l| {
                self.graph.get(l).is_some_and(|e| e.parent.is_some())
            })),
        }
    }

    fn control(&mut self, c: Control) {
        match c {
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
            Control::Reset => {
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

    fn zoom(&mut self, in_: bool) {
        let Some(id) = self.selected else {
            // Nothing is drawn -- every node at this level was filtered out.
            // Zooming the whole cursor back up is the only way out that does
            // not require guessing which setting the user wants changed.
            if !in_ {
                let leaves = self.cursor.active().to_vec();
                let mut moved = false;
                for leaf in leaves {
                    moved |= self.cursor.move_up(leaf, &self.graph);
                }
                if moved {
                    self.rebuild();
                }
            }
            return;
        };
        let moved = if in_ {
            self.cursor.move_down(id, &self.graph)
        } else {
            self.cursor.move_up(id, &self.graph)
        };
        if moved {
            // Zooming out folds the selection into its parent, which is the
            // node the user is now looking at.
            if !in_ {
                self.selected = self.graph.get(id).and_then(|e| e.parent);
            }
            self.rebuild();
        }
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
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(id) = self.leaf_under(ev.column, ev.row) {
                    self.select(id);
                }
            }
            _ => {}
        }
    }

    fn leaf_under(&self, col: u16, row: u16) -> Option<EntityId> {
        self.camera.at(col, row).and_then(|p| self.diagram.leaf_at(p))
    }

    /// Zoom the node under the pointer rather than the selection: pointing at
    /// something and turning the wheel should act on what is pointed at.
    fn zoom_at(&mut self, col: u16, row: u16, in_: bool) {
        if let Some(id) = self.leaf_under(col, row) {
            self.select(id);
        }
        self.zoom(in_);
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
            KeyCode::Enter | KeyCode::Char('+') => self.zoom(true),
            KeyCode::Backspace | KeyCode::Char('-') => self.zoom(false),
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

fn main() -> Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let root = std::path::Path::new(&path)
        .canonicalize()
        .with_context(|| format!("resolving {path}"))?;
    eprintln!("parsing {}...", root.display());
    let graph = treesitter_producer::graph_from_path(&root)
        .with_context(|| format!("parsing {}", root.display()))?;

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
    let width = 52.min(area.width);
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
        // Zoom to the files; the root alone has nothing to scroll or select.
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

    /// Ctrl-scroll acts on what is pointed at, not on what happens to be
    /// selected: the pointer has to move the selection *and* the zoom has to
    /// land on the node it moved to.
    #[test]
    fn ctrl_scrolling_over_a_node_zooms_that_node() {
        // Two folders side by side, so there is a second zoomable leaf to
        // point at -- the shared fixture bottoms out at files, which cannot
        // be zoomed into at all.
        let rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> = vec![
            ("root", Folder, None),
            ("left", Folder, Some(0)),
            ("right", Folder, Some(0)),
            ("a.rs", File, Some(1)),
            ("b.rs", File, Some(2)),
        ];
        let graph = graph_from_parents(&rows, &[(3, 4, Call)]);
        let mut app = App::new(graph, Rect::new(0, 0, 80, 24));
        app.key(KeyCode::Enter, KeyModifiers::NONE); // root -> left, right

        let target = app
            .diagram
            .nodes
            .iter()
            .filter(|n| !n.is_box)
            .find(|n| Some(n.id) != app.selected)
            .expect("both folders should be drawn");
        let (id, rect) = (target.id, target.rect);
        app.camera.reveal(rect);
        let col = (i32::from(rect.x + 1) - app.camera.offset.0) as u16;
        let row = (i32::from(rect.y + 1) - app.camera.offset.1) as u16;

        app.mouse(wheel(MouseEventKind::ScrollUp, KeyModifiers::CONTROL, col, row));
        assert!(
            app.diagram.nodes.iter().any(|n| n.id == id && n.is_box),
            "the wheel zoomed something other than the node under it"
        );
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
            [Control::Tests, Control::OnePerPair, Control::Edges, Control::Hide,
             Control::Scope, Control::ShowAll, Control::Reset]
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
        let row = app.panel.rect.y + 1; // the first control: tests
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

    /// The selection has to survive a rebuild, or zooming leaves the user with
    /// nothing selected and no way back.
    #[test]
    fn zooming_keeps_something_selected() {
        let mut app = App::new(fixture(), Rect::new(0, 0, 60, 20));
        assert!(app.selected.is_some());
        for _ in 0..3 {
            app.key(KeyCode::Enter, KeyModifiers::NONE);
            assert!(app.selected.is_some(), "zooming in lost the selection");
        }
        for _ in 0..3 {
            app.key(KeyCode::Backspace, KeyModifiers::NONE);
            assert!(app.selected.is_some(), "zooming out lost the selection");
        }
    }
}
