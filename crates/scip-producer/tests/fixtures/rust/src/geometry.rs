pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub fn new(x: f64, y: f64) -> Self {
        Point { x, y }
    }

    pub fn magnitude(&self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
}

impl std::fmt::Display for Point {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({}, {})", self.x, self.y)
    }
}

pub struct Span<'a> {
    pub label: &'a str,
}

impl<'a> Span<'a> {
    pub fn of(label: &'a str) -> Span<'a> {
        Span { label }
    }

    pub fn width(&self) -> usize {
        self.label.len()
    }
}
