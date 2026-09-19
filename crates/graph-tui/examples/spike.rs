//! Headless render of one expansion, so the diagram can be judged from a
//! transcript and diffed between changes. The binary needs a terminal; this
//! does not.
//!
//!   cargo run -p graph-tui --example spike -- <path> [depth] [width] [close|mid|far]

use graph_tui::label::Labels;
use graph_tui::layout::{Layout, Options};
use graph_tui::render::{self, Lit};
use graph_tui::scene::Scene;
use graph_tui::view::{self, Settings};
use graph_tui::zoom::Zoom;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| ".".into());
    let depth: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(2);
    let width: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(200);
    let zoom = match args.next().as_deref() {
        Some("mid") => Zoom::Mid,
        Some("far") => Zoom::Far,
        _ => Zoom::Close,
    };

    let graph = treesitter_producer::graph_from_path(std::path::Path::new(&root))?;
    let mut cursor = coalesce::Cursor::new(&graph);
    for _ in 0..depth {
        for leaf in cursor.coalesced().leaves {
            cursor.move_down(leaf, &graph);
        }
    }

    let picture = view::apply(&graph, &cursor.coalesced(), &Settings::default());
    let labels = Labels::new(&graph);
    let scene = Scene::new(&graph, &picture);
    let mut layout = Layout::new();
    layout.settle(&scene, &labels, width);
    let diagram = layout.materialize(&scene, &labels, zoom, &Options::default());
    let rendered = render::render(&labels, &scene, &diagram, Lit::none());
    let buf = rendered.compose(Lit::none());

    for y in 0..buf.area().height {
        let row: String =
            (0..buf.area().width).map(|x| buf[(x, y)].symbol().to_string()).collect();
        println!("{}", row.trim_end());
    }
    let leaves = diagram.nodes.iter().filter(|n| !n.is_box).count();
    eprintln!(
        "depth {depth}: {leaves} leaves in {} boxes -> {}x{}; edges {}, {} cross something",
        diagram.nodes.len() - leaves,
        diagram.width,
        diagram.height,
        rendered.stats.submitted,
        rendered.stats.unroutable,
    );
    Ok(())
}
