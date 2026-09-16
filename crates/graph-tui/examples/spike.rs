//! M0: does a real zoom level of a real repo fit and read in a terminal?
//!
//! Renders headlessly to a ratatui buffer and prints it, so it can be judged
//! from a transcript. Not the eventual TUI — no input, no viewport.
//!
//!   cargo run -p graph-tui --example spike -- <path> [depth] [width] [height]

use std::collections::HashMap;

use entity_graph::{EntityGraph, EntityId};
use graph_tui::placer;
use graph_tui::router::{Connection, ConnectionsLayout, FlowDirection};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn draw_box(buf: &mut Buffer, r: Rect, label: &str, is_box: bool) {
    let (tl, tr, bl, br) = if is_box { ('┏', '┓', '┗', '┛') } else { ('╭', '╮', '╰', '╯') };
    let (h, v) = if is_box { ('━', '┃') } else { ('─', '│') };
    if r.width < 2 || r.height < 2 {
        return;
    }
    for x in r.x..r.right() {
        buf[(x, r.y)].set_symbol(&h.to_string());
        buf[(x, r.bottom() - 1)].set_symbol(&h.to_string());
    }
    for y in r.y..r.bottom() {
        buf[(r.x, y)].set_symbol(&v.to_string());
        buf[(r.right() - 1, y)].set_symbol(&v.to_string());
    }
    buf[(r.x, r.y)].set_symbol(&tl.to_string());
    buf[(r.right() - 1, r.y)].set_symbol(&tr.to_string());
    buf[(r.x, r.bottom() - 1)].set_symbol(&bl.to_string());
    buf[(r.right() - 1, r.bottom() - 1)].set_symbol(&br.to_string());
    let room = r.width.saturating_sub(2) as usize;
    for (i, c) in label.chars().take(room).enumerate() {
        buf[(r.x + 1 + i as u16, r.y + 1)].set_symbol(&c.to_string());
    }
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| ".".into());
    let depth: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);
    let width: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(200);
    let height: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(50);

    let graph: EntityGraph = treesitter_producer::graph_from_path(std::path::Path::new(&root))?;
    let mut cursor = coalesce::Cursor::new(&graph);
    for _ in 0..depth {
        for l in cursor.coalesced().leaves {
            cursor.move_down(l, &graph);
        }
    }
    let coalesced = cursor.coalesced();
    let d = placer::place(&graph, &coalesced, width);
    let leaves = d.nodes.iter().filter(|n| !n.is_box).count();
    let boxes = d.nodes.len() - leaves;
    eprintln!(
        "depth {depth}: {leaves} leaves in {boxes} boxes, {} drawn edges -> {}x{} (canvas {width}x{height})",
        d.edges.len(),
        d.width,
        d.height
    );

    // Render at the diagram's own size and report fit separately: a real TUI
    // pans, and clipping here would hide what M0 is meant to judge.
    let fits = d.width <= width && d.height <= height;
    eprintln!("fits {width}x{height}: {}", if fits { "YES" } else { "no (would pan)" });
    let canvas = Rect { x: 0, y: 0, width: d.width.max(1), height: d.height.max(1) };
    let mut buf = Buffer::empty(canvas);
    let idx: HashMap<EntityId, usize> =
        d.nodes.iter().enumerate().map(|(i, n)| (n.id, i)).collect();

    let mut rt = ConnectionsLayout::new(canvas.width as usize, canvas.height as usize);
    // Only leaves are obstacles. A box's interior is where its children live,
    // and an edge into a nested leaf has to travel through it -- blocking the
    // whole rect walls off everything inside.
    for n in &d.nodes {
        let r = n.rect;
        if !n.is_box {
            rt.block_zone(r);
            continue;
        }
        // A box blocks its border and the row carrying its name, and leaves
        // its interior open: edges between its children belong inside it,
        // while an edge from outside is lifted to this level and stops here.
        rt.block_zone(Rect { height: 2.min(r.height), ..r });
        rt.block_zone(Rect { y: r.bottom() - 1, height: 1, ..r });
        rt.block_zone(Rect { width: 1, ..r });
        rt.block_zone(Rect { x: r.right() - 1, width: 1, ..r });
    }
    // Ports are spread along a node's border rather than stacked down its side,
    // so a node's edge count is bounded by its width, not its height. The
    // upstream widget stacks them, which is why fan-in past a handful fails
    // there.
    let mut used: HashMap<usize, u16> = HashMap::new();
    let mut arrows: Vec<((usize, usize), bool)> = Vec::new();
    let mut routed = 0usize;
    for (i, e) in d.edges.iter().enumerate() {
        let (Some(&a), Some(&b)) = (idx.get(&e.from), idx.get(&e.to)) else { continue };
        let (ra, rb) = (d.nodes[a].rect, d.nodes[b].rect);
        let slot = |r: Rect, k: u16| -> u16 {
            let inner = r.width.saturating_sub(2).max(1);
            r.x + 1 + (k % inner)
        };
        let ka = { let c = used.entry(a).or_insert(0); *c += 1; *c - 1 };
        let kb = { let c = used.entry(b).or_insert(0); *c += 1; *c - 1 };
        // A cycle broken during layering ranks its target *above* its source.
        // Attaching such an edge bottom-to-top asks the router to travel
        // against the flow direction, which simply fails -- and a failure is
        // invisible on the canvas, so it reads as a missing edge.
        let up = rb.y < ra.y;
        let (src, dst) = if up {
            (
                (slot(ra, ka) as usize, ra.y.saturating_sub(1) as usize),
                (slot(rb, kb) as usize, rb.bottom().min(canvas.height.saturating_sub(1)) as usize),
            )
        } else {
            (
                (slot(ra, ka) as usize, ra.bottom().min(canvas.height.saturating_sub(1)) as usize),
                (slot(rb, kb) as usize, rb.y.saturating_sub(1) as usize),
            )
        };
        arrows.push((dst, up));
        rt.insert_port(false, a.into(), i.into(), src);
        rt.insert_port(true, b.into(), i.into(), dst);
        // Plain, so an edge's corners are square and a leaf's are round: with
        // one glyph set for both, a routed corner reads as a node border.
        rt.push_connection((
            Connection::new(a.into(), i.into(), b.into(), i.into())
                .with_line_type(graph_tui::router::LineType::Plain),
            i + 1,
        ));
        routed += 1;
    }
    rt.calculate(FlowDirection::Ttb);
    let failed = rt.diagnostics().len();
    rt.render(canvas, &mut buf);

    for n in &d.nodes {
        let name = graph.get(n.id).map(|e| e.name.as_str()).unwrap_or("?");
        draw_box(&mut buf, n.rect, name, n.is_box);
    }

    // The upstream widget draws arrowheads itself; only the router came
    // across in the fork, so direction has to be drawn here or a call graph
    // shows none at all.
    for ((x, y), up) in arrows {
        let (x, y) = (x as u16, y as u16);
        if x < canvas.width && y < canvas.height {
            buf[(x, y)].set_symbol(if up { "▲" } else { "▼" });
        }
    }
    for y in 0..canvas.height {
        let row: String = (0..canvas.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
        println!("{}", row.trim_end());
    }
    eprintln!("{routed} submitted, {failed} unroutable, {} drawn", routed - failed);
    Ok(())
}
