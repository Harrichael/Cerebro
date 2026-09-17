//! Drawing a placed diagram onto cells.
//!
//! The whole diagram is rendered to its own buffer and the caller blits a
//! window of it. A diagram is far taller than a terminal, and re-routing
//! every edge on each scroll keystroke would be both slow and unstable — an
//! edge would take a different path depending on where you had scrolled to.

use std::collections::HashMap;

use entity_graph::{EntityId, EntityKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::controls::{self, NodeAction};
use crate::label::Labels;
use crate::placer::Diagram;
use crate::zoom::Metrics;
use crate::router::{Connection, ConnectionsLayout, FlowDirection, LineType};

pub struct Stats {
    pub submitted: usize,
    /// Edges the router could not find a path for. Their line is missing from
    /// the canvas, so a caller that does not surface this is showing an
    /// incomplete picture and cannot tell from looking.
    pub unroutable: usize,
}

/// The browser view's palette, so the same kind is the same colour in both.
fn kind_colour(kind: EntityKind) -> Color {
    match kind {
        EntityKind::Folder => Color::Yellow,
        EntityKind::Module => Color::Blue,
        EntityKind::File => Color::Green,
        EntityKind::Class => Color::Magenta,
        EntityKind::Function => Color::Gray,
    }
}

/// How a node and its second line are painted.
///
/// A cell's style is *patched*, not replaced: `Cell::set_style` inserts the
/// modifiers a style adds and removes only the ones it names. A plain
/// `Style::default()` names none, so painting a node back to its own colour
/// leaves BOLD and REVERSED behind and the node the selection just left stays
/// lit. The two styles have to be written as each other's undo -- which is
/// also why the second line is dimmed with a colour rather than with the DIM
/// modifier.
///
/// Selection reverses a node in its own colour rather than recolouring it:
/// the colour is the only thing saying what kind of node it is, and a
/// selection that painted over it would hide that just as you looked at it.
fn node_style(kind: EntityKind, selected: bool) -> (Style, Style) {
    let stale = Modifier::BOLD | Modifier::REVERSED;
    if selected {
        let s = Style::default().fg(kind_colour(kind)).add_modifier(stale);
        (s, s)
    } else {
        (
            Style::default().fg(kind_colour(kind)).remove_modifier(stale),
            Style::default().fg(Color::DarkGray).remove_modifier(stale),
        )
    }
}

/// How a node is drawn, which is the one thing a coarse zoom really changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// A container: heavy frame, its name on the row inside the top.
    Box,
    /// A cursor leaf: light rounded frame.
    Leaf,
    /// A cursor leaf with no room for a frame: its name, and a rule under it
    /// so it still reads as one thing -- and so the router has two rows of
    /// zone to refuse to cross, which one row would not give it.
    Bare,
}

fn draw_node(buf: &mut Buffer, r: Rect, lines: [&str; 2], shape: Shape, style: (Style, Style)) {
    if !on_canvas(r, *buf.area()) {
        return;
    }
    // Whole rows are painted, not just their characters: under REVERSED a
    // half-painted row shows the highlight breaking off mid-node.
    let write_row = |buf: &mut Buffer, y: u16, x0: u16, room: usize, text: &str, st: Style| {
        let mut chars = text.chars().take(room);
        for i in 0..room {
            let c = chars.next().unwrap_or(' ');
            buf[(x0 + i as u16, y)].set_symbol(&c.to_string()).set_style(st);
        }
    };

    if shape == Shape::Bare {
        write_row(buf, r.y, r.x, r.width as usize, lines[0], style.0);
        // Not "─": that is the glyph the router draws edges with, and a rule
        // made of it reads as a line running into the node from both sides.
        for x in r.x..r.right() {
            buf[(x, r.bottom() - 1)].set_symbol("▁").set_style(style.1);
        }
        return;
    }

    // Boxes are heavy and leaves are round, so a container never reads as a
    // leaf. Routed edges are square-cornered for the same reason.
    let (tl, tr, bl, br, h, v) = if shape == Shape::Box {
        ('┏', '┓', '┗', '┛', '━', '┃')
    } else {
        ('╭', '╮', '╰', '╯', '─', '│')
    };
    let style = (style.0, if shape == Shape::Box { style.0 } else { style.1 });
    for x in r.x..r.right() {
        buf[(x, r.y)].set_symbol(&h.to_string()).set_style(style.0);
        buf[(x, r.bottom() - 1)].set_symbol(&h.to_string()).set_style(style.0);
    }
    for y in r.y..r.bottom() {
        buf[(r.x, y)].set_symbol(&v.to_string()).set_style(style.0);
        buf[(r.right() - 1, y)].set_symbol(&v.to_string()).set_style(style.0);
    }
    for (pos, c) in [((r.x, r.y), tl), ((r.right() - 1, r.y), tr), ((r.x, r.bottom() - 1), bl), ((r.right() - 1, r.bottom() - 1), br)] {
        buf[pos].set_symbol(&c.to_string()).set_style(style.0);
    }
    let room = r.width.saturating_sub(2) as usize;
    for (row, (text, st)) in lines.iter().zip([style.0, style.1]).enumerate() {
        let y = r.y + 1 + row as u16;
        if text.is_empty() || y + 1 >= r.bottom() {
            continue;
        }
        write_row(buf, y, r.x + 1, room, text, st);
    }
}

/// The buttons in a node's own corner. Drawn after the frame, over it.
fn draw_actions(buf: &mut Buffer, r: Rect, actions: &[NodeAction], bordered: bool, style: Style) {
    let Some((x0, y)) = controls::node_action_row(r, actions.len(), bordered) else { return };
    for (i, a) in actions.iter().enumerate() {
        buf[(x0 + i as u16, y)].set_symbol(&a.glyph().to_string()).set_style(style);
    }
}

/// Is there room on the canvas for this node at all?
///
/// Placement lets a child escape its parent when the minimum-width floor
/// binds -- an honest outcome for a box that cannot hold its contents -- and
/// a deep enough nesting in a narrow enough terminal walks a node off the
/// canvas. Everything that paints or hit-tests a node asks this first, or the
/// parts disagree: the frame silently vanishes while the buttons carry on
/// being painted, and clicked, out in the margin.
fn on_canvas(r: Rect, area: Rect) -> bool {
    r.width >= 2 && r.height >= 2 && r.right() <= area.width && r.bottom() <= area.height
}

/// Render `d` at full size. The returned buffer's area is the diagram's own
/// extent, not any terminal's.
pub fn render(labels: &Labels, d: &Diagram, selected: Option<EntityId>) -> (Buffer, Stats) {
    let canvas = Rect { x: 0, y: 0, width: d.width.max(1), height: d.height.max(1) };
    let mut buf = Buffer::empty(canvas);
    let idx: HashMap<EntityId, usize> = d.nodes.iter().enumerate().map(|(i, n)| (n.id, i)).collect();

    let mut rt = ConnectionsLayout::new(canvas.width as usize, canvas.height as usize);
    for n in &d.nodes {
        let r = n.rect;
        if !n.is_box {
            rt.block_zone(r);
            continue;
        }
        // A box blocks the row carrying its name and its outermost ring,
        // leaving the interior open: edges between its children belong inside
        // it. Blocking the whole rect instead walls off everything within and
        // *nothing* routes.
        //
        // The ring is not a wall. `block_zone` seals a zone's interior edges,
        // so a one-cell-wide rect blocks nothing crossing it sideways, and a
        // route can pass straight through a border. `draw_box` paints over it
        // afterwards, which hides the crossing rather than preventing it.
        rt.block_zone(Rect { height: 2.min(r.height), ..r });
        rt.block_zone(Rect { y: r.bottom() - 1, height: 1, ..r });
        rt.block_zone(Rect { width: 1, ..r });
        rt.block_zone(Rect { x: r.right() - 1, width: 1, ..r });
    }

    // Ports spread along a node's border rather than stacking down its side, so
    // a node's edge count is bounded by its width — the axis that wraps —
    // instead of its height. Upstream stacks them, which is why fan-in past a
    // handful fails there.
    let mut used: HashMap<usize, u16> = HashMap::new();
    let mut arrows: Vec<(usize, (u16, u16), bool)> = Vec::new();
    let mut submitted = 0usize;
    for (i, e) in d.edges.iter().enumerate() {
        let (Some(&a), Some(&b)) = (idx.get(&e.from), idx.get(&e.to)) else { continue };
        let (ra, rb) = (d.nodes[a].rect, d.nodes[b].rect);
        let slot = |r: Rect, k: u16| r.x + 1 + (k % r.width.saturating_sub(2).max(1));
        let ka = { let c = used.entry(a).or_insert(0); *c += 1; *c - 1 };
        let kb = { let c = used.entry(b).or_insert(0); *c += 1; *c - 1 };
        // A cycle broken during layering ranks its target above its source.
        // Attaching such an edge bottom-to-top asks the router to travel
        // against the flow direction, and it fails — invisibly.
        let up = rb.y < ra.y;
        let low = canvas.height.saturating_sub(1);
        let (src, dst) = if up {
            ((slot(ra, ka), ra.y.saturating_sub(1)), (slot(rb, kb), rb.bottom().min(low)))
        } else {
            ((slot(ra, ka), ra.bottom().min(low)), (slot(rb, kb), rb.y.saturating_sub(1)))
        };
        rt.insert_port(false, a.into(), i.into(), (src.0 as usize, src.1 as usize));
        rt.insert_port(true, b.into(), i.into(), (dst.0 as usize, dst.1 as usize));
        rt.push_connection((
            Connection::new(a.into(), i.into(), b.into(), i.into())
                .with_line_type(LineType::Plain),
            i + 1,
        ));
        arrows.push((i, dst, up));
        submitted += 1;
    }
    rt.calculate(FlowDirection::Ttb);
    // An edge that found no path draws no line, so its arrowhead would be a
    // head with no tail -- and would inflate the very count kept to make the
    // missing edges visible.
    let failed: std::collections::BTreeSet<usize> = rt
        .diagnostics()
        .iter()
        .filter_map(|d| match d {
            crate::router::Diagnostic::RoutingFailed { from_port, .. } => {
                Some(from_port.as_u32() as usize)
            }
            _ => None,
        })
        .collect();
    let unroutable = rt.diagnostics().len();
    rt.render(canvas, &mut buf);

    let m = d.zoom.metrics();
    for n in &d.nodes {
        paint(labels, &mut buf, n.id, n.rect, n.is_box, selected, &m);
    }

    // Only the router was forked; the upstream widget draws arrowheads itself,
    // so without this a call graph shows no direction at all.
    for (i, (x, y), up) in arrows {
        if !failed.contains(&i) && x < canvas.width && y < canvas.height {
            buf[(x, y)].set_symbol(if up { "▲" } else { "▼" });
        }
    }
    (buf, Stats { submitted, unroutable })
}

/// Repaint just the nodes whose selection state changed, in place.
///
/// Moving the selection changes two boxes and nothing else, but a full
/// [`render`] re-routes every edge on the canvas: measured at 1.3 s on this
/// repo's own graph and 6.3 s on a large subtree, all of it router time. That
/// is far too slow for a keypress, and re-routing would also let unrelated
/// edges shift under the user for no reason they can see.
pub fn restyle(
    labels: &Labels,
    d: &Diagram,
    buf: &mut Buffer,
    changed: &[EntityId],
    selected: Option<EntityId>,
) {
    for &id in changed {
        let Some(node) = d.nodes.iter().find(|n| n.id == id) else { continue };
        paint(labels, buf, id, node.rect, node.is_box, selected, &d.zoom.metrics());
    }
}

/// One node, frame, text and buttons, in whatever state it is in.
fn paint(
    labels: &Labels,
    buf: &mut Buffer,
    id: EntityId,
    rect: Rect,
    is_box: bool,
    selected: Option<EntityId>,
    m: &Metrics,
) {
    let Some(kind) = labels.kind(id) else { return };
    if !on_canvas(rect, *buf.area()) {
        return;
    }
    let style = node_style(kind, Some(id) == selected);
    let shape = match (is_box, m.bordered) {
        (true, _) => Shape::Box,
        (false, true) => Shape::Leaf,
        (false, false) => Shape::Bare,
    };
    // A box has one row to spend, so its size goes beside its name; a leaf has
    // a second row for what kind of thing it is, until the zoom takes it away.
    let (head, detail) = if is_box {
        (labels.head(id, m.box_size), String::new())
    } else if m.detail {
        (labels.name(id).to_string(), labels.detail(id))
    } else {
        (labels.name(id).to_string(), String::new())
    };
    draw_node(buf, rect, [&head, &detail], shape, style);
    let expandable = labels.graph.get(id).is_some_and(|e| !e.children.is_empty());
    let actions = controls::node_actions(is_box, expandable);
    draw_actions(buf, rect, actions, shape != Shape::Bare, style.1);
}

/// Copy the window of `src` at `offset` into `area` of `dst`.
///
/// The offset is signed because a diagram smaller than the viewport is centred
/// in it, and the cells outside the diagram are simply left as they are.
pub fn blit(src: &Buffer, dst: &mut Buffer, area: Rect, offset: (i32, i32)) {
    let bounds = *dst.area();
    for y in 0..area.height {
        for x in 0..area.width {
            let (sx, sy) = (i32::from(x) + offset.0, i32::from(y) + offset.1);
            let (dx, dy) = (area.x + x, area.y + y);
            // Both ends are checked: a window larger than the frame is a
            // caller's arithmetic slip, and panicking mid-frame takes the
            // whole app down with the terminal still in raw mode.
            if sx < 0
                || sy < 0
                || sx >= i32::from(src.area().width)
                || sy >= i32::from(src.area().height)
                || dx >= bounds.right()
                || dy >= bounds.bottom()
            {
                continue;
            }
            dst[(dx, dy)] = src[(sx as u16, sy as u16)].clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placer::{DrawnEdge, Placed};
    use entity_graph::EntityKind::{File, Folder};
    use entity_graph::test_support::graph_from_parents;

    /// A diagram is far taller than it is wide, so an edge running the length
    /// of one is the ordinary case, not an extreme.
    ///
    /// Long *diagonal* routes are a different matter and deliberately not
    /// asserted here: upstream prices a turn at the squared distance from both
    /// endpoints, so a mid-canvas turn costs thousands against a step cost of
    /// two, and no budget worth spending finds one. That is a real limitation
    /// of the forked cost function, not of this test.
    #[test]
    fn an_edge_down_the_length_of_a_tall_canvas_still_routes() {
        let graph = graph_from_parents(
            &[("root", Folder, None), ("top.rs", File, Some(0)), ("bottom.rs", File, Some(0))],
            &[],
        );
        let (top, bottom) = (EntityId(1), EntityId(2));
        let d = Diagram {
            nodes: vec![
                Placed { id: top, rect: Rect::new(4, 1, 12, 3), is_box: false },
                Placed { id: bottom, rect: Rect::new(4, 400, 12, 3), is_box: false },
            ],
            edges: vec![DrawnEdge { from: top, to: bottom }],
            width: 190,
            height: 410,
            zoom: crate::zoom::Zoom::Close,
        };
        let (_, stats) = render(&Labels::new(&graph), &d, None);
        assert_eq!(stats.submitted, 1);
        assert_eq!(stats.unroutable, 0, "a route 400 rows long should still be found");
    }

    /// Selection is a highlight you move, so the node it left has to go back
    /// to looking like every other node. It did not: a cell's style is patched
    /// rather than replaced, so REVERSED survived being painted over and the
    /// whole trail of visited nodes stayed lit.
    #[test]
    fn stepping_the_selection_off_a_node_takes_the_highlight_with_it() {
        let graph = graph_from_parents(
            &[("root", Folder, None), ("a.rs", File, Some(0)), ("b.rs", File, Some(0))],
            &[],
        );
        let (a, b) = (EntityId(1), EntityId(2));
        let d = Diagram {
            nodes: vec![
                Placed { id: a, rect: Rect::new(0, 0, 10, 3), is_box: false },
                Placed { id: b, rect: Rect::new(0, 4, 10, 3), is_box: false },
            ],
            edges: vec![],
            width: 10,
            height: 7,
            zoom: crate::zoom::Zoom::Close,
        };
        let lit = |buf: &Buffer, r: Rect| {
            (r.y..r.bottom()).flat_map(|y| (r.x..r.right()).map(move |x| (x, y))).any(|p| {
                buf[p].modifier.intersects(Modifier::REVERSED | Modifier::BOLD)
            })
        };

        let (mut buf, _) = render(&Labels::new(&graph), &d, Some(a));
        assert!(lit(&buf, d.nodes[0].rect), "the selected node should stand out");
        assert!(!lit(&buf, d.nodes[1].rect));
        let r = d.nodes[0].rect;
        assert!(
            (r.x + 1..r.right() - 1)
                .all(|x| buf[(x, r.y + 1)].modifier.contains(Modifier::REVERSED)),
            "the highlight stops partway across the name row"
        );

        restyle(&Labels::new(&graph), &d, &mut buf, &[a, b], Some(b));
        assert!(lit(&buf, d.nodes[1].rect), "the selection did not arrive");
        assert!(!lit(&buf, d.nodes[0].rect), "the node the selection left is still lit");
    }

    /// The coarsest zoom is the one nothing else covers: leaves lose their
    /// frame there and become a name over a rule. It is also the only test of
    /// `Diagram::zoom` being read at all -- with a constant `Close` in its
    /// place this draws a frame and fails.
    #[test]
    fn a_leaf_at_the_coarsest_zoom_is_its_name_over_a_rule() {
        let graph = graph_from_parents(
            &[("root", Folder, None), ("name_00.rs", File, Some(0))],
            &[],
        );
        let leaf = EntityId(1);
        let r = Rect::new(0, 0, 12, 2);
        let d = Diagram {
            nodes: vec![Placed { id: leaf, rect: r, is_box: false }],
            edges: vec![],
            width: 12,
            height: 2,
            zoom: crate::zoom::Zoom::Far,
        };
        let (buf, _) = render(&Labels::new(&graph), &d, None);
        let row = |y: u16| (r.x..r.right()).map(|x| buf[(x, y)].symbol()).collect::<String>();

        // The whole name, starting at the node's own left edge: a frame's
        // worth of inset here would cut two characters off every leaf.
        assert_eq!(row(0), "name_00.rs  ", "the name is not where a bare leaf puts it");
        assert!(row(1).starts_with('▁'), "no rule under the name: {:?}", row(1));
        assert!(
            !row(0).contains('╭') && !row(1).contains('╰'),
            "the coarsest zoom drew a frame it has no room for"
        );
    }

    /// An unroutable edge draws no line, so its arrowhead would be a head with
    /// no tail -- and would inflate the very count kept to make it visible.
    #[test]
    fn an_edge_that_cannot_route_leaves_no_arrowhead() {
        let graph = graph_from_parents(
            &[
                ("root", Folder, None),
                ("a.rs", File, Some(0)),
                ("wall.rs", File, Some(0)),
                ("b.rs", File, Some(0)),
            ],
            &[],
        );
        let (a, wall, b) = (EntityId(1), EntityId(2), EntityId(3));
        // `wall` spans the full width with no gutter, so nothing can get from
        // `a` down to `b`.
        let d = Diagram {
            nodes: vec![
                Placed { id: a, rect: Rect::new(0, 0, 10, 3), is_box: false },
                Placed { id: wall, rect: Rect::new(0, 3, 10, 3), is_box: false },
                Placed { id: b, rect: Rect::new(0, 6, 10, 3), is_box: false },
            ],
            edges: vec![DrawnEdge { from: a, to: b }],
            width: 10,
            height: 9,
            zoom: crate::zoom::Zoom::Close,
        };
        let (buf, stats) = render(&Labels::new(&graph), &d, None);
        assert_eq!(stats.submitted, 1);
        assert_eq!(stats.unroutable, 1, "this edge has nowhere to go; the test proves nothing if it routes");
        let arrows = (0..buf.area().height)
            .flat_map(|y| (0..buf.area().width).map(move |x| (x, y)))
            .filter(|&(x, y)| matches!(buf[(x, y)].symbol(), "▼" | "▲"))
            .count();
        assert_eq!(arrows, 0, "an edge that never routed still drew its arrowhead");
    }
}
