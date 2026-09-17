//! Terminal 2D graph view of a code entity graph.
//!
//! The diagram is rendered once per change to a buffer its own size and the
//! visible window blitted from it, so scrolling never re-routes an edge.
//! Height is the axis that grows with zoom — width wraps against the viewport
//! — which is why panning is vertical first.

use anyhow::{Context, Result};
use entity_graph::{EntityGraph, EntityId};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use graph_tui::placer::{self, Diagram};
use graph_tui::render::{self, Stats};
use graph_tui::view::{self, Settings};

const HINT: &str = "? keys  ·  ↑↓←→ scroll  ·  tab select  ·  ↵ zoom in  ·  ⌫ zoom out  ·  q quit";

/// The status line has room for a handful of keys, which left most of these
/// undiscoverable. Everything that does something is listed here.
const KEYS: &[(&str, &str)] = &[
    ("↑ ↓ ← →  /  k j h l", "scroll"),
    ("PgUp PgDn  /  space", "scroll a half screen"),
    ("Home End  /  g G", "jump to top or bottom"),
    ("tab ⇧tab  /  n p", "select the next or previous node"),
    ("↵  /  +", "zoom into the selected node"),
    ("⌫  /  -", "zoom back out"),
    ("t", "show or hide test code"),
    ("e", "one edge per pair, or one per reference kind"),
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
    offset: (u16, u16),
    /// Always a drawn leaf, never a box; see `rebuild`.
    selected: Option<EntityId>,
    viewport: Rect,
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
            offset: (0, 0),
            selected: None,
            viewport,
            help: false,
            quit: false,
        };
        app.rebuild();
        app
    }

    fn rebuild(&mut self) {
        let picture = view::apply(&self.graph, &self.cursor.coalesced(), &self.settings);
        self.diagram = placer::place(&self.graph, &picture, self.viewport.width.max(20));
        // Only a leaf can be selected: zooming acts on cursor leaves, and a
        // box is an ancestor of one. Keeping a selection that has become a box
        // leaves every later `step_selection` unable to find its own starting
        // point, so tab silently returns to the first leaf each press.
        let still_a_leaf =
            self.diagram.nodes.iter().any(|n| !n.is_box && Some(n.id) == self.selected);
        if !still_a_leaf {
            self.selected = self.diagram.nodes.iter().find(|n| !n.is_box).map(|n| n.id);
        }
        let (canvas, stats) = render::render(&self.graph, &self.diagram, self.selected);
        self.canvas = canvas;
        self.stats = stats;
        self.clamp();
    }

    fn clamp(&mut self) {
        let max_x = self.diagram.width.saturating_sub(self.viewport.width);
        let max_y = self.diagram.height.saturating_sub(self.viewport.height);
        self.offset = (self.offset.0.min(max_x), self.offset.1.min(max_y));
    }

    /// Two boxes change; nothing else on the canvas does.
    fn redraw_selection(&mut self, was: Option<EntityId>) {
        let changed: Vec<EntityId> = [was, self.selected].into_iter().flatten().collect();
        render::restyle(&self.graph, &self.diagram, &mut self.canvas, &changed, self.selected);
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
        let (id, rect) = leaves[next];
        let was = self.selected;
        self.selected = Some(id);
        if rect.y < self.offset.1 {
            self.offset.1 = rect.y.saturating_sub(2);
        } else if rect.bottom() >= self.offset.1 + self.viewport.height {
            self.offset.1 = rect.bottom().saturating_sub(self.viewport.height) + 2;
        }
        if rect.x < self.offset.0 {
            self.offset.0 = rect.x.saturating_sub(2);
        } else if rect.right() >= self.offset.0 + self.viewport.width {
            self.offset.0 = rect.right().saturating_sub(self.viewport.width) + 2;
        }
        self.clamp();
        self.redraw_selection(was);
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

    fn scroll(&mut self, dx: i32, dy: i32) {
        let nx = (self.offset.0 as i32 + dx).max(0) as u16;
        let ny = (self.offset.1 as i32 + dy).max(0) as u16;
        self.offset = (nx, ny);
        self.clamp();
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
        spans.push(Span::raw(format!(
            "· {}x{} · row {}/{} ",
            self.diagram.width,
            self.diagram.height,
            self.offset.1,
            self.diagram.height.saturating_sub(self.viewport.height)
        )));
        if !self.settings.show_tests {
            spans.push(Span::styled("· tests hidden ", Style::default().fg(Color::DarkGray)));
        }
        if !self.settings.one_per_pair {
            spans.push(Span::styled("· every kind ", Style::default().fg(Color::DarkGray)));
        }
        Line::from(spans)
    }

    fn key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let page = self.viewport.height.max(1) as i32 / 2;
        match code {
            // While the key list is up it owns the keyboard, so a stray press
            // dismisses it rather than scrolling something the user cannot see.
            _ if self.help => self.help = false,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.scroll(0, -1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll(0, 1),
            KeyCode::Left | KeyCode::Char('h') => self.scroll(-4, 0),
            KeyCode::Right | KeyCode::Char('l') => self.scroll(4, 0),
            KeyCode::PageUp => self.scroll(0, -page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll(0, page),
            KeyCode::Home | KeyCode::Char('g') => self.offset = (0, 0),
            KeyCode::End | KeyCode::Char('G') => self.scroll(0, i32::from(self.diagram.height)),
            KeyCode::Tab | KeyCode::Char('n') => self.step_selection(true),
            KeyCode::BackTab | KeyCode::Char('p') => self.step_selection(false),
            KeyCode::Enter | KeyCode::Char('+') => self.zoom(true),
            KeyCode::Backspace | KeyCode::Char('-') => self.zoom(false),
            KeyCode::Char('t') => {
                self.settings.show_tests = !self.settings.show_tests;
                self.rebuild();
            }
            KeyCode::Char('e') => {
                self.settings.one_per_pair = !self.settings.one_per_pair;
                self.rebuild();
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
        ratatui::restore();
        hook(info);
    }));

    let mut terminal = ratatui::try_init().context(
        "opening the terminal (cerebro draws a diagram; it needs a real terminal, \
         not a pipe or redirect -- use `--example spike` for text output)",
    )?;
    let size = terminal.size()?;
    let viewport = Rect::new(0, 0, size.width, size.height.saturating_sub(2));
    let mut app = App::new(graph, viewport);

    let result = run(&mut terminal, &mut app);
    ratatui::restore();
    result
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

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.quit {
        terminal.draw(|frame| {
            let area = frame.area();
            let body = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2));
            if body.width != app.viewport.width || body.height != app.viewport.height {
                app.viewport = body;
                app.rebuild();
            }
            render::blit(&app.canvas, frame.buffer_mut(), body, app.offset);
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
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.key(key.code, key.modifiers);
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

    #[test]
    fn scrolling_stops_at_the_end_of_the_diagram() {
        let mut app = app();
        app.key(KeyCode::Char('G'), KeyModifiers::NONE);
        let limit = app.diagram.height.saturating_sub(app.viewport.height);
        assert_eq!(app.offset.1, limit, "scrolled past the last row");
        app.key(KeyCode::Char('g'), KeyModifiers::NONE);
        assert_eq!(app.offset, (0, 0));
        app.key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.offset.1, 0, "scrolled above the first row");
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
        let drawn = |a: &App| a.diagram.nodes.iter().any(|n| n.id == test_id);

        assert!(drawn(&app), "tests are shown by default, as in the browser");
        app.key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(!drawn(&app), "t should hide test code");
        app.key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(drawn(&app), "t should bring it back");
    }

    /// While the key list is up it owns the keyboard: a press meant to dismiss
    /// it must not also scroll or zoom something the user cannot see.
    #[test]
    fn the_key_list_swallows_the_press_that_dismisses_it() {
        let mut app = app();
        let (before_offset, before_sel) = (app.offset, app.selected);
        app.key(KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(app.help);

        app.key(KeyCode::Down, KeyModifiers::NONE);
        assert!(!app.help, "any key should dismiss the list");
        assert_eq!(app.offset, before_offset, "the dismissing press also scrolled");
        assert_eq!(app.selected, before_sel);

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
