//! Headless render of one zoom level, so the diagram can be judged from a
//! transcript and diffed between changes. The binary needs a terminal; this
//! does not.
//!
//!   cargo run -p graph-tui --example spike -- <path> [depth] [width]

use graph_tui::view::Settings;
use graph_tui::label::Labels;
use graph_tui::{placer, render, view};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| ".".into());
    let depth: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(2);
    let width: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(200);

    let graph = treesitter_producer::graph_from_path(std::path::Path::new(&root))?;
    let mut cursor = coalesce::Cursor::new(&graph);
    for _ in 0..depth {
        for leaf in cursor.coalesced().leaves {
            cursor.move_down(leaf, &graph);
        }
    }

    let settings = Settings::default();
    let picture = view::apply(&graph, &cursor.coalesced(), &settings);
    let labels = Labels::new(&graph);
    let diagram = placer::place(&labels, &picture, width);
    let (buf, stats) = render::render(&labels, &diagram, None);

    for y in 0..buf.area().height {
        let row: String =
            (0..buf.area().width).map(|x| buf[(x, y)].symbol().to_string()).collect();
        println!("{}", row.trim_end());
    }
    let leaves = diagram.nodes.iter().filter(|n| !n.is_box).count();
    eprintln!(
        "depth {depth}: {leaves} leaves in {} boxes -> {}x{}; edges {} drawn, {} unroutable",
        diagram.nodes.len() - leaves,
        diagram.width,
        diagram.height,
        stats.submitted - stats.unroutable,
        stats.unroutable,
    );
    Ok(())
}
