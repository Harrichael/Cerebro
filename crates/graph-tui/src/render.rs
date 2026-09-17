//! Drawing a placed diagram onto cells.
//!
//! The whole diagram is rendered to its own buffer and the caller blits a
//! window of it. A zoom level is far taller than a terminal, and re-routing
//! every edge on each scroll keystroke would be both slow and unstable — an
//! edge would take a different path depending on where you had scrolled to.

use std::collections::HashMap;

use entity_graph::{EntityGraph, EntityId, EntityKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::placer::Diagram;
use crate::router::{Connection, ConnectionsLayout, FlowDirection, LineType};

pub struct Stats {
    pub submitted: usize,
    /// Edges the router could not find a path for. Their line is missing from
    /// the canvas, so a caller that does not surface this is showing an
    /// incomplete picture and cannot tell from looking.
    pub unroutable: usize,
}

fn kind_colour(kind: EntityKind) -> Color {
    match kind {
        EntityKind::Folder => Color::Blue,
        EntityKind::Module => Color::Cyan,
        EntityKind::File => Color::White,
        EntityKind::Class => Color::Magenta,
        EntityKind::Function => Color::Green,
    }
}

fn draw_box(buf: &mut Buffer, r: Rect, label: &str, is_box: bool, style: Style) {
    if r.width < 2 || r.height < 2 || r.right() > buf.area().width || r.bottom() > buf.area().height
    {
        return;
    }
    // Boxes are heavy and leaves are round, so a container never reads as a
    // leaf. Routed edges are square-cornered for the same reason.
    let (tl, tr, bl, br, h, v) =
        if is_box { ('┏', '┓', '┗', '┛', '━', '┃') } else { ('╭', '╮', '╰', '╯', '─', '│') };
    for x in r.x..r.right() {
        buf[(x, r.y)].set_symbol(&h.to_string()).set_style(style);
        buf[(x, r.bottom() - 1)].set_symbol(&h.to_string()).set_style(style);
    }
    for y in r.y..r.bottom() {
        buf[(r.x, y)].set_symbol(&v.to_string()).set_style(style);
        buf[(r.right() - 1, y)].set_symbol(&v.to_string()).set_style(style);
    }
    for (pos, c) in [((r.x, r.y), tl), ((r.right() - 1, r.y), tr), ((r.x, r.bottom() - 1), bl), ((r.right() - 1, r.bottom() - 1), br)] {
        buf[pos].set_symbol(&c.to_string()).set_style(style);
    }
    let room = r.width.saturating_sub(2) as usize;
    for (i, c) in label.chars().take(room).enumerate() {
        buf[(r.x + 1 + i as u16, r.y + 1)].set_symbol(&c.to_string()).set_style(style);
    }
}

/// Render `d` at full size. The returned buffer's area is the diagram's own
/// extent, not any terminal's.
pub fn render(graph: &EntityGraph, d: &Diagram, selected: Option<EntityId>) -> (Buffer, Stats) {
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

    for n in &d.nodes {
        let Some(entity) = graph.get(n.id) else { continue };
        let mut style = Style::default().fg(kind_colour(entity.kind));
        if Some(n.id) == selected {
            style = style.fg(Color::Yellow).add_modifier(Modifier::BOLD | Modifier::REVERSED);
        }
        draw_box(&mut buf, n.rect, &entity.name, n.is_box, style);
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
    graph: &EntityGraph,
    d: &Diagram,
    buf: &mut Buffer,
    changed: &[EntityId],
    selected: Option<EntityId>,
) {
    for &id in changed {
        let (Some(node), Some(entity)) = (d.nodes.iter().find(|n| n.id == id), graph.get(id))
        else {
            continue;
        };
        let mut style = Style::default().fg(kind_colour(entity.kind));
        if Some(id) == selected {
            style = style.fg(Color::Yellow).add_modifier(Modifier::BOLD | Modifier::REVERSED);
        }
        draw_box(buf, node.rect, &entity.name, node.is_box, style);
    }
}

/// Copy the window of `src` at `offset` into `area` of `dst`.
pub fn blit(src: &Buffer, dst: &mut Buffer, area: Rect, offset: (u16, u16)) {
    let bounds = *dst.area();
    for y in 0..area.height {
        for x in 0..area.width {
            let (sx, sy) = (x.saturating_add(offset.0), y.saturating_add(offset.1));
            let (dx, dy) = (area.x + x, area.y + y);
            // Both ends are checked: a window larger than the frame is a
            // caller's arithmetic slip, and panicking mid-frame takes the
            // whole app down with the terminal still in raw mode.
            if sx >= src.area().width
                || sy >= src.area().height
                || dx >= bounds.width
                || dy >= bounds.height
            {
                continue;
            }
            dst[(dx, dy)] = src[(sx, sy)].clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placer::{DrawnEdge, Placed};
    use entity_graph::EntityKind::{File, Folder};
    use entity_graph::test_support::graph_from_parents;

    /// A zoom level is far taller than it is wide, so an edge running the
    /// length of one is the ordinary case, not an extreme.
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
        };
        let (_, stats) = render(&graph, &d, None);
        assert_eq!(stats.submitted, 1);
        assert_eq!(stats.unroutable, 0, "a route 400 rows long should still be found");
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
        };
        let (buf, stats) = render(&graph, &d, None);
        assert_eq!(stats.submitted, 1);
        assert_eq!(stats.unroutable, 1, "this edge has nowhere to go; the test proves nothing if it routes");
        let arrows = (0..buf.area().height)
            .flat_map(|y| (0..buf.area().width).map(move |x| (x, y)))
            .filter(|&(x, y)| matches!(buf[(x, y)].symbol(), "▼" | "▲"))
            .count();
        assert_eq!(arrows, 0, "an edge that never routed still drew its arrowhead");
    }
}
