//! One number set, six levels, so before/after are actually comparable.
use graph_tui::label::Labels;
use graph_tui::layout::{Layout, Options};
use graph_tui::render::{self, Lit};
use graph_tui::scene::Scene;
use graph_tui::view;
use graph_tui::zoom::Zoom;
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
        let scene = Scene::new(&graph, &pic);
        let t0 = Instant::now();
        let mut layout = Layout::new();
        layout.settle(&scene, &labels, 200);
        let d = layout.materialize(&scene, &labels, zoom, &Options::default());
        let lay_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let r = render::render(&labels, &scene, &d, Lit::none());
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let _ = r.compose(Lit::none());
        let comp_ms = t.elapsed().as_secs_f64() * 1000.0;
        let s = &r.stats;
        tot += s.submitted; fail += s.unroutable;
        println!("{path:>20} d{depth}: {:>4} edges, {:>3} cross ({:>2}%) | layout {:>4.0}ms render {:>5.0}ms compose {:>3.0}ms | {}x{}",
            s.submitted, s.unroutable,
            (s.unroutable * 100).checked_div(s.submitted).unwrap_or(0), lay_ms, ms, comp_ms, d.width, d.height);
    }
    println!("TOTAL ({}): {}/{} clean, {}% cross", zoom.name(), tot - fail, tot, fail * 100 / tot.max(1));
    Ok(())
}
