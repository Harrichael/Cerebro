//! One number set, six levels, so before/after are actually comparable.
use graph_tui::label::Labels;
use graph_tui::zoom::Zoom;
use graph_tui::{placer, render, view};
use std::time::Instant;
fn main() -> anyhow::Result<()> {
    let levels: Vec<(&str, usize)> = vec![
        (".", 2), (".", 3), (".", 4),
        ("crates/graph-server", 2), ("crates/graph-server", 3), ("crates/graph-server", 4),
    ];
    let zoom = match std::env::args().nth(1).as_deref() {
        Some("mid") => Zoom::Mid,
        Some("far") => Zoom::Far,
        _ => Zoom::Close,
    };
    let (mut tot, mut fail) = (0usize, 0usize);
    for (path, depth) in levels {
        let graph = treesitter_producer::graph_from_path(std::path::Path::new(path))?;
        let mut c = coalesce::Cursor::new(&graph);
        for _ in 0..depth { for l in c.coalesced().leaves { c.move_down(l, &graph); } }
        let pic = view::apply(&graph, &c.coalesced(), &Default::default());
        let labels = Labels::new(&graph);
        let d = placer::place(&labels, &pic, 200, zoom);
        let t = Instant::now();
        let (buf, s) = render::render(&labels, &d, None);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        // Detour: drawn line cells against straight-line distance, as a crude
        // check that routes are not snaking across the whole canvas.
        let ink = (0..buf.area().height).flat_map(|y| (0..buf.area().width).map(move |x| (x, y)))
            .filter(|&(x, y)| matches!(buf[(x, y)].symbol(), "─"|"│"|"┌"|"┐"|"└"|"┘"|"├"|"┤"|"┬"|"┴"|"┼"))
            .count();
        let straight: usize = d.edges.iter().filter_map(|e| {
            let a = d.nodes.iter().find(|n| n.id == e.from)?.rect;
            let b = d.nodes.iter().find(|n| n.id == e.to)?.rect;
            Some((a.x as i32 - b.x as i32).unsigned_abs() as usize
               + (a.y as i32 - b.y as i32).unsigned_abs() as usize)
        }).sum();
        tot += s.submitted; fail += s.unroutable;
        println!("{path:>20} d{depth}: {:>4}/{:<4} fail {:>2}% | {:>5.0}ms | ink/straight {:.2}",
            s.submitted - s.unroutable, s.submitted,
            (s.unroutable * 100).checked_div(s.submitted).unwrap_or(0), ms,
            if straight > 0 { ink as f64 / straight as f64 } else { 0.0 });
    }
    println!("TOTAL ({}): {}/{} drawn, {}% fail", zoom.name(), tot - fail, tot, fail * 100 / tot.max(1));
    Ok(())
}
