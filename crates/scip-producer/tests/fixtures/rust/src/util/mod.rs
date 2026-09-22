use crate::geometry;
use crate::geometry::*;

pub fn greet() {
    println!("{}", inner::banner());
}

pub fn origin() -> Point {
    geometry::Point::new(0.0, 0.0)
}

mod inner {
    pub fn banner() -> &'static str {
        "hello"
    }
}
