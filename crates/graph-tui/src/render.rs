//! Painting a diagram: nodes as cells, edges as braille beneath them.
//!
//! Three vocabularies, so nothing is mistaken for anything else. A container
//! is an *area*: a background tint one step lighter per level, a thin dim
//! frame, its name in the colour of its kind. A leaf is a *card*: a rounded
//! frame in its kind's colour on a tint one step lighter than the box it is
//! in. An edge is a *dotted curve*: braille, never box-drawing, drawn under
//! the nodes so it goes behind whatever it cannot avoid.
//!
//! The whole diagram is rendered to a buffer its own size and the caller
//! blits a window of it. Routing is the expensive step and depends only on
//! where the nodes are, so it is kept in [`Rendered`] and a change of
//! selection recomposes the edges over the nodes without routing again.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use entity_graph::{EntityId, EntityKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::braille::Canvas;
use crate::controls::{self, NodeAction};
use crate::label::Labels;
use crate::layout::{Diagram, Placed, INSET};
use crate::route::{self, Port, Side};
use crate::scene::Scene;
use crate::zoom::Metrics;

pub struct Stats {
    pub submitted: usize,
    /// Edges the router found no clear path for. They are still drawn, as a
    /// direct curve that may cross something, so the count is the one thing
    /// saying the picture is not quite honest.
    pub unroutable: usize,
}

/// What is lit: the primary selection, and the group a marquee or a
/// shift-click has gathered round it.
#[derive(Debug, Clone, Copy)]
pub struct Lit<'a> {
    pub primary: Option<EntityId>,
    pub group: &'a BTreeSet<EntityId>,
}

impl Lit<'_> {
    pub fn none() -> Lit<'static> {
        static EMPTY: BTreeSet<EntityId> = BTreeSet::new();
        Lit { primary: None, group: &EMPTY }
    }

    fn is_primary(&self, id: EntityId) -> bool {
        self.primary == Some(id)
    }

    fn in_group(&self, id: EntityId) -> bool {
        self.group.contains(&id)
    }

    fn touches(&self, id: EntityId) -> bool {
        self.is_primary(id) || self.in_group(id)
    }
}

/// A cell nothing may draw an edge over: a frame, a title row, a leaf.
const SOLID: u8 = u8::MAX;
/// An edge that has nothing to do with the selection.
const MUTED: Color = Color::Indexed(245);
/// An edge into or out of a selected node.
const LIT: Color = Color::Cyan;
const FRAME: Color = Color::Indexed(243);

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

/// One step lighter per level, so a box reads as an area and a leaf as a
/// card lying on it. Capped where grey text would stop being legible.
fn tint(depth: u8) -> Color {
    Color::Indexed((233 + 2 * u16::from(depth)).min(243) as u8)
}

/// How a node is drawn, which is the one thing a coarse zoom really changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Box,
    Leaf,
    /// A leaf with no room for a frame: its name, and a rule under it so it
    /// still reads as one thing.
    Bare,
}

/// A cell's style is *patched*, not replaced: `set_style` inserts the
/// modifiers a style adds and removes only the ones it names, so the styles
/// here name the modifiers they do not want, or the node the selection just
/// left stays lit.
fn plain(fg: Color, bg: Color) -> Style {
    Style::default().fg(fg).bg(bg).remove_modifier(Modifier::BOLD | Modifier::REVERSED | Modifier::DIM)
}

fn write_row(buf: &mut Buffer, y: u16, x0: u16, room: usize, text: &str, st: Style) {
    let mut chars = text.chars().take(room);
    for i in 0..room {
        let c = chars.next().unwrap_or(' ');
        buf[(x0 + i as u16, y)].set_symbol(&c.to_string()).set_style(st);
    }
}

fn fill(buf: &mut Buffer, r: Rect, st: Style) {
    for y in r.y..r.bottom() {
        for x in r.x..r.right() {
            buf[(x, y)].set_symbol(" ").set_style(st);
        }
    }
}

fn frame(buf: &mut Buffer, r: Rect, glyphs: [char; 6], st: Style) {
    let [tl, tr, bl, br, h, v] = glyphs;
    for x in r.x..r.right() {
        buf[(x, r.y)].set_symbol(&h.to_string()).set_style(st);
        buf[(x, r.bottom() - 1)].set_symbol(&h.to_string()).set_style(st);
    }
    for y in r.y..r.bottom() {
        buf[(r.x, y)].set_symbol(&v.to_string()).set_style(st);
        buf[(r.right() - 1, y)].set_symbol(&v.to_string()).set_style(st);
    }
    for (pos, c) in [
        ((r.x, r.y), tl),
        ((r.right() - 1, r.y), tr),
        ((r.x, r.bottom() - 1), bl),
        ((r.right() - 1, r.bottom() - 1), br),
    ] {
        buf[pos].set_symbol(&c.to_string()).set_style(st);
    }
}

/// Is there room on the canvas for this node at all? Everything that paints
/// or hit-tests a node asks this first, or the parts disagree.
fn on_canvas(r: Rect, area: Rect) -> bool {
    r.width >= 2 && r.height >= 2 && r.right() <= area.width && r.bottom() <= area.height
}

/// One node, in whatever state it is in.
fn paint(labels: &Labels, buf: &mut Buffer, node: &Placed, lit: Lit, m: &Metrics) {
    let (id, r, is_box, depth) = (node.id, node.rect, node.is_box, node.depth);
    let Some(kind) = labels.kind(id) else { return };
    if !on_canvas(r, *buf.area()) {
        return;
    }
    let shape = match (is_box, m.bordered) {
        (true, _) => Shape::Box,
        (false, true) => Shape::Leaf,
        (false, false) => Shape::Bare,
    };
    let bg = tint(depth);
    let kind_fg = kind_colour(kind);
    // Selection reverses a node in its own colour rather than recolouring
    // it: the colour is the only thing saying what kind of node it is, and a
    // selection that painted over it would hide that just as you looked.
    let primary = lit.is_primary(id);
    let grouped = lit.in_group(id) && !primary;
    let (frame_st, name_st, dim_st) = if primary {
        let s = plain(kind_fg, bg).add_modifier(Modifier::BOLD | Modifier::REVERSED);
        (s, s, s)
    } else if grouped {
        (
            plain(LIT, bg).add_modifier(Modifier::BOLD),
            plain(Color::Reset, bg).add_modifier(Modifier::BOLD),
            plain(Color::Gray, bg),
        )
    } else {
        match shape {
            Shape::Box => (plain(FRAME, bg), plain(kind_fg, bg).add_modifier(Modifier::BOLD), plain(FRAME, bg)),
            _ => (plain(kind_fg, bg), plain(Color::Reset, bg).add_modifier(Modifier::BOLD), plain(Color::Gray, bg)),
        }
    };

    match shape {
        Shape::Box => {
            // The tint is what makes it an area; the frame is only there so
            // two boxes of the same depth side by side do not merge.
            fill(buf, r, plain(FRAME, bg));
            frame(buf, r, ['┌', '┐', '└', '┘', '─', '│'], frame_st);
            let title = labels.head(id, m.box_size);
            let room = usize::from(r.width.saturating_sub(2));
            if r.height > 2 {
                write_row(buf, r.y + 1, r.x + 1, room, &title, name_st);
            }
        }
        Shape::Leaf => {
            fill(buf, r, plain(kind_fg, bg));
            frame(buf, r, ['╭', '╮', '╰', '╯', '─', '│'], frame_st);
            let room = usize::from(r.width.saturating_sub(2));
            let detail = if m.detail { labels.detail(id) } else { String::new() };
            for (row, (text, st)) in [(labels.name(id).to_string(), name_st), (detail, dim_st)]
                .into_iter()
                .enumerate()
            {
                let y = r.y + 1 + row as u16;
                if text.is_empty() || y + 1 >= r.bottom() {
                    continue;
                }
                write_row(buf, y, r.x + 1, room, &text, st);
            }
        }
        Shape::Bare => {
            write_row(buf, r.y, r.x, usize::from(r.width), labels.name(id), name_st);
            // Not "─": a rule made of the line glyph reads as an edge running
            // into the node from both sides.
            for x in r.x..r.right() {
                buf[(x, r.bottom() - 1)].set_symbol("▁").set_style(frame_st);
            }
        }
    }

    let expandable = labels.graph.get(id).is_some_and(|e| !e.children.is_empty());
    let actions = controls::node_actions(is_box, expandable);
    draw_actions(buf, r, actions, shape != Shape::Bare, dim_st);
}

/// The buttons in a node's own corner. Drawn after the frame, over it.
fn draw_actions(buf: &mut Buffer, r: Rect, actions: &[NodeAction], bordered: bool, style: Style) {
    let Some((x0, y)) = controls::node_action_row(r, actions.len(), bordered) else { return };
    for (i, a) in actions.iter().enumerate() {
        buf[(x0 + i as u16, y)].set_symbol(&a.glyph().to_string()).set_style(style);
    }
}

/// One edge, routed. Kept so the picture can be recomposed without routing.
struct Routed {
    from: EntityId,
    to: EntityId,
    points: Vec<(f32, f32)>,
    /// The cell the arrowhead goes on, and the side it enters from.
    head: ((u16, u16), Side),
    /// The mask value of the cells this edge may draw on: its level.
    level: u8,
}

/// The routed edges and the painted nodes of one diagram, ready to compose.
pub struct Rendered {
    nodes: Buffer,
    /// Per cell: the depth of the level whose empty floor it is, or
    /// [`SOLID`] where a node is. An edge among the children of a box may
    /// only draw on that box's floor, so it goes under anything deeper and
    /// never over a frame.
    mask: Vec<u8>,
    routes: Vec<Routed>,
    pub stats: Stats,
}

impl Rendered {
    fn allowed(&self, x: u16, y: u16, level: u8) -> bool {
        let w = usize::from(self.nodes.area().width);
        self.mask.get(usize::from(y) * w + usize::from(x)).is_some_and(|&m| m == level)
    }

    /// Repaint the nodes whose lit state changed, in place. A box is painted
    /// as one area, over whatever sits on it, so repainting a box repaints
    /// everything inside it too, parents first as the first paint did.
    pub fn restyle(&mut self, labels: &Labels, d: &Diagram, changed: &[EntityId], lit: Lit) {
        let m = d.zoom.metrics();
        let boxes: Vec<Rect> =
            d.nodes.iter().filter(|n| n.is_box && changed.contains(&n.id)).map(|n| n.rect).collect();
        for node in &d.nodes {
            let inside = boxes.iter().any(|b| b.contains(node.rect.as_position()));
            if inside || changed.contains(&node.id) {
                paint(labels, &mut self.nodes, node, lit, &m);
            }
        }
    }

    /// Nodes with the edges over their floors. Edges touching the selection
    /// are drawn last and bright, so they win the cells they share.
    pub fn compose(&self, lit: Lit) -> Buffer {
        let mut buf = self.nodes.clone();
        let area = *buf.area();
        let mut by_level: BTreeMap<u8, Vec<&Routed>> = BTreeMap::new();
        for r in &self.routes {
            by_level.entry(r.level).or_default().push(r);
        }
        let mut canvas = Canvas::new(area.width, area.height);
        for (level, routes) in by_level {
            canvas.clear();
            let is_lit = |r: &Routed| lit.touches(r.from) || lit.touches(r.to);
            for r in routes.iter().filter(|r| !is_lit(r)) {
                canvas.polyline(&r.points, MUTED);
            }
            for r in routes.iter().filter(|r| is_lit(r)) {
                canvas.polyline(&r.points, LIT);
            }
            canvas.composite(&mut buf, |x, y| self.allowed(x, y, level));
        }
        for r in &self.routes {
            let ((x, y), side) = r.head;
            if x < area.width && y < area.height {
                let colour = if lit.touches(r.from) || lit.touches(r.to) { LIT } else { MUTED };
                let glyph = match side {
                    Side::Top => "▼",
                    Side::Bottom => "▲",
                    Side::Left => "▶",
                    Side::Right => "◀",
                };
                buf[(x, y)].set_symbol(glyph).set_fg(colour);
            }
        }
        buf
    }
}

/// Which side of each node every edge uses, and its slot along that side.
/// Ports are spread along a side in the order of the far end, so fan-in
/// reads as a fan and never crosses itself at the node.
fn ports(d: &Diagram, rect_of: &HashMap<EntityId, Rect>) -> Vec<(Port, Port)> {
    // (node, side) -> [(edge index, far end coordinate along the side)]
    let mut slots: BTreeMap<(EntityId, Side), Vec<(usize, i32)>> = BTreeMap::new();
    let mut chosen: Vec<(Side, Side)> = Vec::with_capacity(d.edges.len());
    for (i, e) in d.edges.iter().enumerate() {
        let (Some(&a), Some(&b)) = (rect_of.get(&e.from), rect_of.get(&e.to)) else {
            chosen.push((Side::Bottom, Side::Top));
            continue;
        };
        let (sa, sb) = route::sides(a, b);
        let along = |r: Rect, side: Side| match side {
            Side::Top | Side::Bottom => i32::from(r.x) * 2 + i32::from(r.width),
            Side::Left | Side::Right => i32::from(r.y) * 2 + i32::from(r.height),
        };
        slots.entry((e.from, sa)).or_default().push((i, along(b, sa)));
        slots.entry((e.to, sb)).or_default().push((i, along(a, sb)));
        chosen.push((sa, sb));
    }
    let mut at: HashMap<(usize, bool), (f32, f32)> = HashMap::new();
    for ((node, side), mut list) in slots {
        let Some(&rect) = rect_of.get(&node) else { continue };
        list.sort_by_key(|&(i, far)| (far, i));
        let n = list.len();
        for (k, (i, _)) in list.into_iter().enumerate() {
            let is_from = d.edges[i].from == node && chosen[i].0 == side
                && !at.contains_key(&(i, true));
            at.insert((i, is_from), route::port_at(rect, side, k, n));
        }
    }
    d.edges
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let (sa, sb) = chosen[i];
            let from = at.get(&(i, true)).copied().unwrap_or((0.0, 0.0));
            let to = at.get(&(i, false)).copied().unwrap_or((0.0, 0.0));
            (Port { at: from, side: sa }, Port { at: to, side: sb })
        })
        .collect()
}

/// Render `d` at full size. The returned buffer's area is the diagram's own
/// extent, not any terminal's.
pub fn render(labels: &Labels, scene: &Scene, d: &Diagram, lit: Lit) -> Rendered {
    let area = Rect { x: 0, y: 0, width: d.width.max(1), height: d.height.max(1) };
    let mut nodes = Buffer::empty(area);
    let mut mask = vec![0u8; usize::from(area.width) * usize::from(area.height)];
    let m = d.zoom.metrics();
    let w = usize::from(area.width);
    let rect_of: HashMap<EntityId, Rect> = d.nodes.iter().map(|n| (n.id, n.rect)).collect();

    // Parents come before children in `nodes`, so a child's cells overwrite
    // the floor its box laid down.
    for n in &d.nodes {
        paint(labels, &mut nodes, n, lit, &m);
        if !on_canvas(n.rect, area) {
            continue;
        }
        let r = n.rect;
        for y in r.y..r.bottom() {
            for x in r.x..r.right() {
                let floor = n.is_box
                    && x > r.x
                    && x + 1 < r.right()
                    && y >= r.y + INSET.1 as u16
                    && y + 1 < r.bottom();
                mask[usize::from(y) * w + usize::from(x)] = if floor { n.depth } else { SOLID };
            }
        }
    }

    let ports = ports(d, &rect_of);
    let mut routes = Vec::with_capacity(d.edges.len());
    let mut unroutable = 0usize;
    for (i, e) in d.edges.iter().enumerate() {
        let (Some(&a), Some(&b)) = (rect_of.get(&e.from), rect_of.get(&e.to)) else { continue };
        let others: Vec<Rect> = scene
            .children(e.level)
            .iter()
            .filter(|&&k| k != e.from && k != e.to)
            .filter_map(|k| rect_of.get(k).copied())
            .collect();
        let (from, to) = ports[i];
        // An edge among a box's children is drawn on that box's floor and
        // nowhere else, so it must be routed within the floor: a path that
        // slipped out under the frame would simply vanish there.
        let within = e.level.and_then(|c| rect_of.get(&c)).map(|r| floor_of(*r));
        let r = route::route(from, a, to, b, &others, within);
        if !r.clean {
            unroutable += 1;
        }
        let level = e.level.map_or(0, |c| scene.depth(c));
        // The arrowhead sits on the floor cell the port names, touching the
        // border. A target drawn hard against something else -- its box's
        // title row, a neighbour -- has no such cell, and the head goes on
        // the border cell instead, as the lesser of covering the frame and
        // covering whatever is next door.
        let cell = |p: (f32, f32)| -> Option<(u16, u16)> {
            let (x, y) = (p.0.floor(), p.1.floor());
            (x >= 0.0 && y >= 0.0 && x < f32::from(area.width) && y < f32::from(area.height)).then_some((x as u16, y as u16))
        };
        let (ox, oy) = to.side.outward();
        let on_floor = cell(to.at).filter(|&(x, y)| mask[usize::from(y) * w + usize::from(x)] == level);
        let head = match on_floor.or_else(|| cell((to.at.0 - ox, to.at.1 - oy))) {
            Some(c) => (c, to.side),
            None => continue,
        };
        routes.push(Routed { from: e.from, to: e.to, points: r.points, head, level });
    }
    Rendered { nodes, mask, routes, stats: Stats { submitted: d.edges.len(), unroutable } }
}

/// The cells of a box that are its own empty floor: inside the frame and
/// below the title row. The same rule the mask uses.
fn floor_of(r: Rect) -> Rect {
    let top = INSET.1 as u16;
    if r.width < 2 || r.height < top + 1 {
        return Rect::new(r.x, r.y, 0, 0);
    }
    Rect::new(r.x + 1, r.y + top, r.width - 2, r.height - top - 1)
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
    use crate::layout::{Layout, Options};
    use crate::view::Picture;
    use entity_graph::EntityGraph;
    use entity_graph::EntityKind::{File, Folder};
    use entity_graph::ReferenceKind::Call;
    use entity_graph::test_support::graph_from_parents;

    fn fully_expanded(graph: &EntityGraph) -> Picture {
        let mut cursor = coalesce::Cursor::new(graph);
        loop {
            let mut moved = false;
            for leaf in cursor.coalesced().leaves {
                moved |= cursor.move_down(leaf, graph);
            }
            if !moved {
                return crate::view::apply(graph, &cursor.coalesced(), &Default::default());
            }
        }
    }

    fn drawn(graph: &EntityGraph, zoom: crate::zoom::Zoom) -> (Scene, Diagram) {
        let labels = Labels::new(graph);
        let scene = Scene::new(graph, &fully_expanded(graph));
        let mut layout = Layout::new();
        layout.settle(&scene, &labels, 200);
        let d = layout.materialize(&scene, &labels, zoom, &Options::default());
        (scene, d)
    }

    fn is_braille(s: &str) -> bool {
        s.chars().next().is_some_and(|c| ('\u{2800}'..='\u{28FF}').contains(&c))
    }

    fn cells(buf: &Buffer) -> impl Iterator<Item = (u16, u16)> + '_ {
        (0..buf.area().height).flat_map(move |y| (0..buf.area().width).map(move |x| (x, y)))
    }

    /// The point of the whole renderer: an edge is dots between two nodes,
    /// it never lands on a node or a frame, and it ends in an arrowhead on
    /// the target's border.
    #[test]
    fn an_edge_is_braille_between_its_nodes_and_never_over_them() {
        let graph = graph_from_parents(
            &[("root", Folder, None), ("caller.rs", File, Some(0)), ("callee.rs", File, Some(0))],
            &[(1, 2, Call)],
        );
        let labels = Labels::new(&graph);
        let (scene, d) = drawn(&graph, crate::zoom::Zoom::Close);
        let rendered = render(&labels, &scene, &d, Lit::none());
        assert_eq!(rendered.stats.submitted, 1);
        assert_eq!(rendered.stats.unroutable, 0);
        let buf = rendered.compose(Lit::none());

        let dotted: Vec<(u16, u16)> = cells(&buf).filter(|&p| is_braille(buf[p].symbol())).collect();
        assert!(!dotted.is_empty(), "no braille was drawn at all");
        let (caller, callee) = (d.rect_of(EntityId(1)).unwrap(), d.rect_of(EntityId(2)).unwrap());
        for p in &dotted {
            assert!(!caller.contains((*p).into()) && !callee.contains((*p).into()), "dots on a node at {p:?}");
            let root = d.rect_of(EntityId(0)).unwrap();
            assert!(p.0 > root.x && p.0 + 1 < root.right() && p.1 > root.y + 1 && p.1 + 1 < root.bottom(), "dots on the root's frame at {p:?}");
        }
        let is_head = |p: (u16, u16)| matches!(buf[p].symbol(), "▼" | "▲" | "◀" | "▶");
        let heads: Vec<(u16, u16)> = cells(&buf).filter(|&p| is_head(p)).collect();
        assert_eq!(heads.len(), 1, "one edge, one arrowhead");
        let head = heads[0];
        let touching = !callee.contains(head.into())
            && Rect::new(callee.x - 1, callee.y - 1, callee.width + 2, callee.height + 2).contains(head.into());
        assert!(touching, "the arrowhead {head:?} is not just outside the target's frame {callee:?}");
        assert!(buf[head].fg == MUTED, "an unselected edge is muted");
        for x in callee.x..callee.right() {
            assert!(!is_head((x, callee.y)) && !is_head((x, callee.bottom() - 1)), "the frame was written over");
        }
    }

    /// A box can be the selection, and repainting it must not take its
    /// children with it: the box is an area painted under them.
    #[test]
    fn selecting_a_box_repaints_it_without_wiping_what_is_inside() {
        let graph = graph_from_parents(
            &[("root", Folder, None), ("a.rs", File, Some(0)), ("b.rs", File, Some(0))],
            &[],
        );
        let labels = Labels::new(&graph);
        let (scene, d) = drawn(&graph, crate::zoom::Zoom::Close);
        let mut rendered = render(&labels, &scene, &d, Lit::none());
        let a = d.rect_of(EntityId(1)).unwrap();
        let text_at = |buf: &Buffer, r: Rect| -> String {
            (r.x + 1..r.right() - 1).map(|x| buf[(x, r.y + 1)].symbol().to_string()).collect::<String>().trim().to_string()
        };
        assert_eq!(text_at(&rendered.compose(Lit::none()), a), "a.rs");

        let group = BTreeSet::new();
        let lit = Lit { primary: Some(EntityId(0)), group: &group };
        rendered.restyle(&labels, &d, &[EntityId(0)], lit);
        let buf = rendered.compose(lit);
        assert_eq!(text_at(&buf, a), "a.rs", "selecting the box wiped its child");
        let root = d.rect_of(EntityId(0)).unwrap();
        assert!(buf[(root.x, root.y)].modifier.contains(Modifier::REVERSED), "the box is not drawn selected");
    }

    /// Selecting a node lights the edges at it and nothing else, without
    /// routing again: the same routes, recomposed.
    #[test]
    fn selecting_a_node_lights_its_edges_and_dims_the_rest() {
        let graph = graph_from_parents(
            &[
                ("root", Folder, None),
                ("a.rs", File, Some(0)),
                ("b.rs", File, Some(0)),
                ("c.rs", File, Some(0)),
                ("d.rs", File, Some(0)),
            ],
            &[(1, 2, Call), (3, 4, Call)],
        );
        let labels = Labels::new(&graph);
        let (scene, d) = drawn(&graph, crate::zoom::Zoom::Close);
        let rendered = render(&labels, &scene, &d, Lit::none());
        let group = BTreeSet::new();
        let lit = Lit { primary: Some(EntityId(1)), group: &group };
        let buf = rendered.compose(lit);
        let colours: BTreeSet<String> = cells(&buf)
            .filter(|&p| is_braille(buf[p].symbol()))
            .map(|p| format!("{:?}", buf[p].fg))
            .collect();
        assert!(colours.contains(&format!("{LIT:?}")), "the selected node's edge is not lit");
        assert!(colours.contains(&format!("{MUTED:?}")), "the other edge should stay muted");

        let dark = rendered.compose(Lit::none());
        assert!(
            cells(&dark).filter(|&p| is_braille(dark[p].symbol())).all(|p| dark[p].fg == MUTED),
            "with nothing selected every edge is muted"
        );
    }

    /// An edge among a box's children may cross the box's floor and nothing
    /// else -- not a sibling leaf, not the frame of a nested box. A straight
    /// drop with a leaf placed squarely in the way is the case the router
    /// might get wrong; the mask is the backstop that keeps it under the leaf.
    #[test]
    fn an_edge_goes_under_a_node_it_cannot_avoid() {
        let graph = graph_from_parents(
            &[
                ("root", Folder, None),
                ("a.rs", File, Some(0)),
                ("wall.rs", File, Some(0)),
                ("b.rs", File, Some(0)),
            ],
            &[(1, 3, Call)],
        );
        let labels = Labels::new(&graph);
        let scene = Scene::new(&graph, &fully_expanded(&graph));
        // Hand-built: `wall` spans the full interior between a and b.
        let d = Diagram {
            nodes: vec![
                crate::layout::Placed { id: EntityId(0), rect: Rect::new(0, 0, 16, 16), is_box: true, depth: 1 },
                crate::layout::Placed { id: EntityId(1), rect: Rect::new(2, 2, 12, 3), is_box: false, depth: 2 },
                crate::layout::Placed { id: EntityId(2), rect: Rect::new(2, 6, 12, 3), is_box: false, depth: 2 },
                crate::layout::Placed { id: EntityId(3), rect: Rect::new(2, 11, 12, 3), is_box: false, depth: 2 },
            ],
            edges: scene.edges.clone(),
            width: 16,
            height: 16,
            zoom: crate::zoom::Zoom::Close,
        };
        let rendered = render(&labels, &scene, &d, Lit::none());
        assert_eq!(rendered.stats.submitted, 1);
        let buf = rendered.compose(Lit::none());
        let wall = Rect::new(2, 6, 12, 3);
        for p in cells(&buf).filter(|&p| is_braille(buf[p].symbol())) {
            assert!(!wall.contains(p.into()), "an edge was drawn over a leaf at {p:?}");
        }
        assert!(cells(&buf).any(|p| is_braille(buf[p].symbol())), "the edge vanished entirely");
    }

    /// Selection is a highlight you move, so the node it left has to go back
    /// to looking like every other node. A cell's style is patched rather
    /// than replaced, so REVERSED survives being painted over unless the
    /// plain style names it.
    #[test]
    fn stepping_the_selection_off_a_node_takes_the_highlight_with_it() {
        let graph = graph_from_parents(
            &[("root", Folder, None), ("a.rs", File, Some(0)), ("b.rs", File, Some(0))],
            &[],
        );
        let labels = Labels::new(&graph);
        let (scene, d) = drawn(&graph, crate::zoom::Zoom::Close);
        let (a, b) = (EntityId(1), EntityId(2));
        let lit_cells = |buf: &Buffer, r: Rect| {
            (r.y..r.bottom()).flat_map(|y| (r.x..r.right()).map(move |x| (x, y))).any(|p| {
                buf[p].modifier.contains(Modifier::REVERSED)
            })
        };
        let group = BTreeSet::new();
        let mut rendered = render(&labels, &scene, &d, Lit { primary: Some(a), group: &group });
        let buf = rendered.compose(Lit::none());
        let (ra, rb) = (d.rect_of(a).unwrap(), d.rect_of(b).unwrap());
        assert!(lit_cells(&buf, ra), "the selected node should stand out");
        assert!(!lit_cells(&buf, rb));
        assert!(
            (ra.x + 1..ra.right() - 1).all(|x| buf[(x, ra.y + 1)].modifier.contains(Modifier::REVERSED)),
            "the highlight stops partway across the name row"
        );

        rendered.restyle(&labels, &d, &[a, b], Lit { primary: Some(b), group: &group });
        let buf = rendered.compose(Lit::none());
        assert!(lit_cells(&buf, rb), "the selection did not arrive");
        assert!(!lit_cells(&buf, ra), "the node the selection left is still lit");

        // A group member is marked but not reversed: it is chosen, not current.
        let grouped: BTreeSet<EntityId> = [a].into_iter().collect();
        rendered.restyle(&labels, &d, &[a], Lit { primary: Some(b), group: &grouped });
        let buf = rendered.compose(Lit::none());
        assert!(!lit_cells(&buf, ra));
        assert_eq!(buf[(ra.x, ra.y)].fg, LIT, "a grouped node's frame is lit");
    }

    /// Boxes and leaves have to look like different things: a box is an area
    /// with a tint and a thin frame, a leaf a card on a lighter tint with a
    /// rounded frame in its kind's colour.
    #[test]
    fn a_box_is_a_tinted_area_and_a_leaf_a_lighter_card_on_it() {
        let graph = graph_from_parents(
            &[("root", Folder, None), ("a.rs", File, Some(0))],
            &[],
        );
        let labels = Labels::new(&graph);
        let (scene, d) = drawn(&graph, crate::zoom::Zoom::Close);
        let buf = render(&labels, &scene, &d, Lit::none()).compose(Lit::none());
        let root = d.rect_of(EntityId(0)).unwrap();
        let leaf = d.rect_of(EntityId(1)).unwrap();
        assert_eq!(buf[(root.x, root.y)].symbol(), "┌");
        assert_eq!(buf[(leaf.x, leaf.y)].symbol(), "╭");
        assert_eq!(buf[(leaf.x, leaf.y)].fg, kind_colour(File));
        let floor = buf[(root.x + 1, root.bottom() - 2)].bg;
        assert_eq!(floor, tint(1));
        assert_eq!(buf[(leaf.x + 1, leaf.y + 1)].bg, tint(2), "a leaf sits one step lighter");
        assert_ne!(floor, tint(2));
        let title: String = (root.x + 1..root.x + 5).map(|x| buf[(x, root.y + 1)].symbol().to_string()).collect();
        assert_eq!(title, "root");
        assert_eq!(buf[(root.x + 1, root.y + 1)].fg, kind_colour(Folder));
    }

    /// The coarsest zoom is the one nothing else covers: leaves lose their
    /// frame there and become a name over a rule.
    #[test]
    fn a_leaf_at_the_coarsest_zoom_is_its_name_over_a_rule() {
        let graph = graph_from_parents(
            &[("root", Folder, None), ("name_00.rs", File, Some(0))],
            &[],
        );
        let labels = Labels::new(&graph);
        let (scene, d) = drawn(&graph, crate::zoom::Zoom::Far);
        let buf = render(&labels, &scene, &d, Lit::none()).compose(Lit::none());
        let r = d.rect_of(EntityId(1)).unwrap();
        let row = |y: u16| (r.x..r.right()).map(|x| buf[(x, y)].symbol()).collect::<String>();
        assert!(row(r.y).starts_with("name_00.rs"), "the name is not where a bare leaf puts it: {:?}", row(r.y));
        assert!(row(r.y + 1).starts_with('▁'), "no rule under the name: {:?}", row(r.y + 1));
        assert!(!row(r.y).contains('╭'), "the coarsest zoom drew a frame it has no room for");
    }
}
