mod geometry;
mod util;

use geometry::{Point, Span};

fn main() {
    let p = Point::new(3.0, 4.0);
    println!("{}", describe(&p));
    util::greet();
}

fn describe(p: &Point) -> String {
    let tag = Span::of("point");
    format!("{} ({}) has magnitude {}", p, tag.width(), p.magnitude())
}
