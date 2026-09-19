//! The path one edge takes around what is in its way.
//!
//! Pure geometry in cell units: rectangles and two ports in, a smooth polyline
//! out. A route leaves its port through a short perpendicular stub, follows
//! the taut shortest path over the corners of the inflated obstacles (A* on
//! the visibility graph, walled in by the box it is drawn in), and is then
//! made into one cubic curve through those corners -- an arc that spends
//! each leg turning, rather than a straight run with a bend at the end.
//!
//! Cell `(x, y)` spans `[x, x+1) × [y, y+1)`; its centre is `(x+0.5, y+0.5)`.

use ratatui::layout::Rect;
use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// Length of the perpendicular leg out of each port.
const STUB: f32 = 1.5;
/// Inflation of the rects an edge must keep clear of. 1.5 puts the runs that
/// hug an obstacle on the centre of the second cell out, leaving one clear
/// cell of air between node and line.
const MARGIN: f32 = 1.5;
/// The margin a crowded level falls back to: no air, but still no crossing.
/// Two nodes may sit two cells apart, and a line has to get between them.
const TIGHT_MARGIN: f32 = 0.5;
/// The endpoint rects inflate less: only enough that the stub end lies outside
/// them, so a route cannot cut back through the node it just left.
const END_MARGIN: f32 = 0.5;
/// Obstacles further than this from the edge's bounding region are ignored.
const REGION: f32 = 8.0;
/// The most obstacles a route will look at: the nearest to the straight line
/// between its ports. A level of several hundred leaves has far more in the
/// region of a long edge, and a visibility graph over all of them costs
/// seconds per edge; the ones far from the line are the ones a taut path
/// around the near ones will not meet anyway.
const MAX_OBSTACLES: usize = 80;
/// In the fallback rounding, legs are cut into pieces of about this length
/// first, which bounds the rounding radius to about half a piece.
const PIECE: f32 = 3.0;
const SMOOTH_ROUNDS: usize = 2;
/// Obstacles shrink by this much in the crossing test so a segment running
/// along an inflated border, or touching a corner, does not count as entering.
const EPS: f32 = 1e-3;

type P = (f32, f32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Side {
    Top,
    Bottom,
    Left,
    Right,
}

impl Side {
    pub fn outward(self) -> P {
        match self {
            Side::Top => (0.0, -1.0),
            Side::Bottom => (0.0, 1.0),
            Side::Left => (-1.0, 0.0),
            Side::Right => (1.0, 0.0),
        }
    }
}

/// Where an edge meets a node: the centre of the cell just outside its
/// border, and which side that is. Outside, not on: the arrowhead is drawn
/// on this cell, and an arrowhead on the border cell would replace a piece
/// of the frame instead of pointing at it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Port {
    pub at: P,
    pub side: Side,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    /// Starts exactly at `from.at`, ends exactly at `to.at`.
    pub points: Vec<P>,
    /// False when no clear path was found and the route is a direct curve that may cross obstacles.
    pub clean: bool,
}

/// Which sides two rects should connect on: the pair that makes the edge
/// steep on screen. A braille line drawn steeply is a line; drawn flat it is
/// a row of dots, and an S-curve between vertical ports puts its flattest
/// stretch, the crossover, right in the middle. So an edge that is more
/// across than down *as the eye sees it* -- a cell is twice as tall as it is
/// wide, so that is twice as many columns as rows -- leaves and arrives by
/// the facing sides, and the S then drops steeply between two short level
/// stubs. Nodes wholly above or below each other and not far apart drop.
pub fn sides(from: Rect, to: Rect) -> (Side, Side) {
    let (f, t) = (Bx::of(from, 0.0), Bx::of(to, 0.0));
    let (dx, dy) = (t.centre().0 - f.centre().0, t.centre().1 - f.centre().1);
    let stacked = t.y0 >= f.y1 || t.y1 <= f.y0;
    if stacked && dx.abs() <= 2.0 * dy.abs() {
        if dy >= 0.0 { (Side::Bottom, Side::Top) } else { (Side::Top, Side::Bottom) }
    } else if dx >= 0.0 {
        (Side::Right, Side::Left)
    } else {
        (Side::Left, Side::Right)
    }
}

/// The centre of the `k`-th of `n` port cells along `side` of `rect`: the cell
/// just outside the side, spread evenly along it but never beside a corner
/// cell. A side with fewer non-corner cells than `n` reuses cells.
pub fn port_at(rect: Rect, side: Side, k: usize, n: usize) -> P {
    let (origin, len) = match side {
        Side::Top | Side::Bottom => (rect.x, rect.width),
        Side::Left | Side::Right => (rect.y, rect.height),
    };
    let along = f32::from(origin) + f32::from(slot(len, k, n)) + 0.5;
    let b = Bx::of(rect, 0.0);
    match side {
        Side::Top => (along, b.y0 - 0.5),
        Side::Bottom => (along, b.y1 + 0.5),
        Side::Left => (b.x0 - 0.5, along),
        Side::Right => (b.x1 + 0.5, along),
    }
}

/// Offset of the port cell from the side's first cell. A side narrower than
/// three cells has no non-corner cell, so it takes its middle.
fn slot(len: u16, k: usize, n: usize) -> u16 {
    if len <= 2 {
        return len / 2;
    }
    let m = usize::from(len - 2);
    let n = n.max(1);
    let k = k.min(n - 1);
    let i = ((k as f32 + 0.5) / n as f32 * m as f32).floor() as usize;
    1 + i.min(m - 1) as u16
}

/// Route from `from` (a port on `from_rect`) to `to` (a port on `to_rect`),
/// keeping clear of `others` and, when the edge lies among the children of
/// a box, inside that box's interior `within`. The two endpoint rects are
/// obstacles too (a route must not cut back through the node it left), but
/// inflated less so the stub can exit. Never fails.
pub fn route(
    from: Port,
    from_rect: Rect,
    to: Port,
    to_rect: Rect,
    others: &[Rect],
    within: Option<Rect>,
) -> Route {
    let sp = step(from.at, from.side, STUB);
    let tp = step(to.at, to.side, STUB);
    let heading = (from.side.outward(), neg(to.side.outward()));
    // Roomy first, then tight: a line with air round it reads better, but a
    // line that hugs its neighbours still beats one drawn through them.
    for margin in [MARGIN, TIGHT_MARGIN] {
        let field = Field::new(sp, tp, from_rect, to_rect, others, within, margin);
        if let Some(taut) = field.taut_path() {
            let points = field.bend(&taut, heading, from.at, to.at);
            return Route { points, clean: true };
        }
    }
    let mut points = vec![from.at];
    points.extend(hermite(&[sp, tp], &tangents(&[sp, tp], heading, 1.0)));
    points.push(to.at);
    Route { points, clean: false }
}

fn step(p: P, side: Side, d: f32) -> P {
    let (ox, oy) = side.outward();
    (p.0 + ox * d, p.1 + oy * d)
}

fn neg(p: P) -> P {
    (-p.0, -p.1)
}

/// What one route has to get past: the nearby obstacles, inflated by the
/// margin being tried, and the walls of the box it is inside.
struct Field {
    sp: P,
    tp: P,
    /// Inflated by `margin`; the endpoint rects last, inflated less.
    obstacles: Vec<Bx>,
    /// The same, uninflated, for checking a curve that may use the air.
    raw: Vec<Bx>,
    from: Bx,
    to: Bx,
    bounds: Option<Bx>,
    margin: f32,
}

impl Field {
    fn new(
        sp: P,
        tp: P,
        from_rect: Rect,
        to_rect: Rect,
        others: &[Rect],
        within: Option<Rect>,
        margin: f32,
    ) -> Field {
        let from = Bx::of(from_rect, 0.0);
        let to = Bx::of(to_rect, 0.0);
        let region = from.hull(&to).hull(&Bx::point(sp)).hull(&Bx::point(tp)).grown(REGION);
        let mut raw: Vec<Bx> = others
            .iter()
            .filter(|r| r.width > 0 && r.height > 0 && **r != from_rect && **r != to_rect)
            .map(|r| Bx::of(*r, 0.0))
            .filter(|b| b.overlaps(&region))
            .collect();
        if raw.len() > MAX_OBSTACLES {
            raw.sort_by(|a, b| {
                segment_distance(a.centre(), sp, tp).total_cmp(&segment_distance(b.centre(), sp, tp))
            });
            raw.truncate(MAX_OBSTACLES);
        }
        let mut obstacles: Vec<Bx> = raw.iter().map(|b| b.grown(margin)).collect();
        obstacles.push(from.grown(END_MARGIN.min(margin)));
        obstacles.push(to.grown(END_MARGIN.min(margin)));
        // The walls keep the same air the obstacles do, so a route runs no
        // closer to its box's frame than to a sibling. A box too small for
        // that air is used to its edge.
        let bounds = within.map(|r| {
            let b = Bx::of(r, 0.0);
            let tight = b.grown(-(margin - 0.5).max(0.0));
            if tight.x1 - tight.x0 >= 1.0 && tight.y1 - tight.y0 >= 1.0 { tight } else { b }
        });
        Field { sp, tp, obstacles, raw, from, to, bounds, margin }
    }

    fn inside(&self, p: P) -> bool {
        self.obstacles.iter().any(|o| o.contains_open(p))
    }

    /// Shortest obstacle-free polyline from `sp` to `tp` (both included), or
    /// None when there is none or an endpoint sits inside an obstacle.
    ///
    /// A* over the corners of the inflated obstacles, clamped into the
    /// bounds: a corner past the wall becomes the point where the obstacle
    /// meets it, which is where a taut path along the wall would turn.
    /// The stub ends themselves may lie outside the bounds -- a node drawn
    /// against its box's edge puts them there -- and a leg from one of them
    /// is allowed to cross back in.
    fn taut_path(&self) -> Option<Vec<P>> {
        let (sp, tp) = (self.sp, self.tp);
        if self.inside(sp) || self.inside(tp) {
            return None;
        }
        let mut verts = vec![sp, tp];
        for o in &self.obstacles {
            for c in o.corners() {
                let c = match &self.bounds {
                    Some(b) => (c.0.clamp(b.x0, b.x1), c.1.clamp(b.y0, b.y1)),
                    None => c,
                };
                if !self.inside(c) {
                    verts.push(c);
                }
            }
        }
        let visible = |a: P, b: P| !self.obstacles.iter().any(|o| crosses(a, b, o));

        // A* with Euclidean lengths; 0 is the start, 1 the goal.
        let n = verts.len();
        let mut g = vec![f32::INFINITY; n];
        let mut prev = vec![usize::MAX; n];
        let mut closed = vec![false; n];
        let mut open = BinaryHeap::new();
        g[0] = 0.0;
        open.push(Open { f: dist(sp, tp), v: 0 });
        while let Some(Open { v: u, .. }) = open.pop() {
            if closed[u] {
                continue;
            }
            closed[u] = true;
            if u == 1 {
                let mut path = vec![];
                let mut v = 1;
                while v != usize::MAX {
                    path.push(verts[v]);
                    v = prev[v];
                }
                path.reverse();
                return Some(path);
            }
            for v in 0..n {
                if closed[v] {
                    continue;
                }
                let cand = g[u] + dist(verts[u], verts[v]);
                if cand < g[v] && visible(verts[u], verts[v]) {
                    g[v] = cand;
                    prev[v] = u;
                    open.push(Open { f: cand + dist(verts[v], tp), v });
                }
            }
        }
        None
    }

    /// The taut path made into one curve: a cubic through its corners, leaving
    /// and arriving along the stubs, that only turns where the path did but
    /// spreads each turn over the legs beside it. Tried full first, then
    /// tauter, because a curve bulges outward at a corner and the space out
    /// there may be someone else's; if even a slight curve touches something
    /// the path is merely rounded, which never leaves the margin.
    fn bend(&self, taut: &[P], heading: (P, P), start: P, end: P) -> Vec<P> {
        let check = (self.margin - 0.5).max(TIGHT_MARGIN);
        for tau in TENSIONS {
            let curve = hermite(taut, &tangents(taut, heading, tau));
            if self.clear(&curve, check) {
                return wrap(start, curve, end);
            }
        }
        let mut chain = vec![start];
        chain.extend_from_slice(taut);
        chain.push(end);
        smooth(&chain)
    }

    /// Does the curve keep `margin` from every obstacle, stay inside the
    /// bounds, and not cut back through its own two nodes past the stubs?
    fn clear(&self, curve: &[P], margin: f32) -> bool {
        let others: Vec<Bx> = self.raw.iter().map(|b| b.grown(margin)).collect();
        let ends = [(self.from.grown(END_MARGIN.min(margin)), self.sp), (self.to.grown(END_MARGIN.min(margin)), self.tp)];
        for w in curve.windows(2) {
            let (a, b) = (w[0], w[1]);
            if others.iter().any(|o| crosses(a, b, o)) {
                return false;
            }
            for (rect, stub) in &ends {
                if dist(a, *stub) > STUB && dist(b, *stub) > STUB && crosses(a, b, rect) {
                    return false;
                }
            }
            if let Some(bounds) = &self.bounds {
                let out = |p: P| !bounds.grown(EPS).contains_open(p);
                if out(a) && out(b) && dist(a, self.sp) > STUB && dist(a, self.tp) > STUB {
                    return false;
                }
            }
        }
        true
    }
}

fn wrap(start: P, mut curve: Vec<P>, end: P) -> Vec<P> {
    let mut points = Vec::with_capacity(curve.len() + 2);
    points.push(start);
    points.append(&mut curve);
    points.push(end);
    points
}

/// Curve tensions tried in turn: how far the tangents reach, as a fraction
/// of the shorter leg at each knot. At 1 a lone S-bend is the classic
/// two-control-point curve; at 0 the curve is the polyline itself.
const TENSIONS: [f32; 3] = [1.0, 0.6, 0.3];
/// How finely a curve is sampled for drawing: half a cell is one braille
/// dot across, so a curve never shows its own segments.
const SAMPLE: f32 = 0.5;

/// Tangents for a cubic through `knots`: outward along the first stub and
/// inward along the last, and at each corner the bisector of its two legs.
///
/// At a corner a tangent reaches `tau` of the shorter leg beside it, so the
/// curve can never loop back past a knot however uneven the legs. At the
/// ends it reaches one and a half times the leg's extent *along the stub*,
/// which is the classic S-bend and is monotone along that axis: measured by
/// the leg's full length, an edge to a node far to the side and only a
/// little lower left downward, rose above its own ports and came back, a
/// wave where a shallow S was wanted.
fn tangents(knots: &[P], heading: (P, P), tau: f32) -> Vec<P> {
    let n = knots.len();
    let leg = |i: usize| dist(knots[i], knots[i + 1]);
    let along = |i: usize, dir: P| {
        let v = (knots[i + 1].0 - knots[i].0, knots[i + 1].1 - knots[i].1);
        (1.5 * (v.0 * dir.0 + v.1 * dir.1)).max(STUB)
    };
    (0..n)
        .map(|i| {
            let (dir, reach) = if i == 0 {
                (heading.0, along(0, heading.0))
            } else if i == n - 1 {
                (heading.1, along(n - 2, heading.1))
            } else {
                let a = unit(knots[i - 1], knots[i]);
                let b = unit(knots[i], knots[i + 1]);
                let sum = (a.0 + b.0, a.1 + b.1);
                let len = sum.0.hypot(sum.1);
                (if len < EPS { b } else { (sum.0 / len, sum.1 / len) }, leg(i - 1).min(leg(i)))
            };
            (dir.0 * reach * tau, dir.1 * reach * tau)
        })
        .collect()
}

fn unit(a: P, b: P) -> P {
    let d = dist(a, b);
    if d < EPS { (0.0, 0.0) } else { ((b.0 - a.0) / d, (b.1 - a.1) / d) }
}

/// A cubic Hermite spline through `knots`, sampled about every [`SAMPLE`].
fn hermite(knots: &[P], tangents: &[P]) -> Vec<P> {
    let mut out = vec![knots[0]];
    for i in 0..knots.len().saturating_sub(1) {
        let (p0, p1, m0, m1) = (knots[i], knots[i + 1], tangents[i], tangents[i + 1]);
        let n = ((dist(p0, p1) / SAMPLE).ceil() as usize).max(2);
        for k in 1..=n {
            let t = k as f32 / n as f32;
            let (t2, t3) = (t * t, t * t * t);
            let (h00, h10, h01, h11) = (2.0 * t3 - 3.0 * t2 + 1.0, t3 - 2.0 * t2 + t, -2.0 * t3 + 3.0 * t2, t3 - t2);
            out.push((
                h00 * p0.0 + h10 * m0.0 + h01 * p1.0 + h11 * m1.0,
                h00 * p0.1 + h10 * m0.1 + h01 * p1.1 + h11 * m1.1,
            ));
        }
    }
    out
}

/// How far `p` is from the segment `a`–`b`.
fn segment_distance(p: P, a: P, b: P) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 { 0.0 } else { (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0) };
    dist(p, (a.0 + t * dx, a.1 + t * dy))
}

/// Min-heap entry; ties broken by vertex index so the search is deterministic.
#[derive(PartialEq)]
struct Open {
    f: f32,
    v: usize,
}

impl Eq for Open {}

impl Ord for Open {
    fn cmp(&self, other: &Self) -> Ordering {
        other.f.total_cmp(&self.f).then_with(|| other.v.cmp(&self.v))
    }
}

impl PartialOrd for Open {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Liang–Barsky: does the open segment `p`–`q` enter the interior of `o`?
fn crosses(p: P, q: P, o: &Bx) -> bool {
    let (dx, dy) = (q.0 - p.0, q.1 - p.1);
    let (mut t0, mut t1) = (0.0f32, 1.0f32);
    let edges = [
        (p.0 - (o.x0 + EPS), -dx),
        ((o.x1 - EPS) - p.0, dx),
        (p.1 - (o.y0 + EPS), -dy),
        ((o.y1 - EPS) - p.1, dy),
    ];
    for (num, den) in edges {
        if den == 0.0 {
            // Parallel to this pair of sides: crosses only if inside the slab.
            if num <= 0.0 {
                return false;
            }
            continue;
        }
        let r = num / den;
        if den < 0.0 {
            if r > t1 {
                return false;
            }
            t0 = t0.max(r);
        } else {
            if r < t0 {
                return false;
            }
            t1 = t1.min(r);
        }
    }
    t0 < t1
}

/// The fallback shape when no curve through a taut path is clear: legs cut
/// into pieces and corners rounded with Chaikin's midpoint scheme, which
/// never strays further from a corner than half a piece and so never
/// leaves the margin the path was found in. The first and last points, and
/// the direction of the first and last legs, are preserved.
fn smooth(chain: &[P]) -> Vec<P> {
    let mut pts = vec![chain[0]];
    for w in chain.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a == b {
            continue;
        }
        let n = ((dist(a, b) / PIECE).round() as usize).max(1);
        for k in 1..n {
            pts.push(lerp(a, b, k as f32 / n as f32));
        }
        pts.push(b);
    }
    for _ in 0..SMOOTH_ROUNDS {
        pts = chaikin(&pts);
    }
    pts
}

fn chaikin(pts: &[P]) -> Vec<P> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let mut out = Vec::with_capacity(pts.len() * 2);
    out.push(pts[0]);
    for w in pts.windows(2) {
        out.push(lerp(w[0], w[1], 0.25));
        out.push(lerp(w[0], w[1], 0.75));
    }
    out.push(pts[pts.len() - 1]);
    out
}

fn lerp(a: P, b: P, t: f32) -> P {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

fn dist(a: P, b: P) -> f32 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

/// A rect in cell coordinates: `[x0, x1) × [y0, y1)`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Bx {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl Bx {
    fn of(r: Rect, m: f32) -> Bx {
        Bx {
            x0: f32::from(r.x) - m,
            y0: f32::from(r.y) - m,
            x1: f32::from(r.x) + f32::from(r.width) + m,
            y1: f32::from(r.y) + f32::from(r.height) + m,
        }
    }

    fn point(p: P) -> Bx {
        Bx { x0: p.0, y0: p.1, x1: p.0, y1: p.1 }
    }

    fn grown(self, m: f32) -> Bx {
        Bx { x0: self.x0 - m, y0: self.y0 - m, x1: self.x1 + m, y1: self.y1 + m }
    }

    fn hull(&self, o: &Bx) -> Bx {
        Bx {
            x0: self.x0.min(o.x0),
            y0: self.y0.min(o.y0),
            x1: self.x1.max(o.x1),
            y1: self.y1.max(o.y1),
        }
    }

    fn centre(&self) -> P {
        ((self.x0 + self.x1) / 2.0, (self.y0 + self.y1) / 2.0)
    }

    fn overlaps(&self, o: &Bx) -> bool {
        self.x0 < o.x1 && o.x0 < self.x1 && self.y0 < o.y1 && o.y0 < self.y1
    }

    fn contains_open(&self, p: P) -> bool {
        p.0 > self.x0 && p.0 < self.x1 && p.1 > self.y0 && p.1 < self.y1
    }

    fn corners(&self) -> [P; 4] {
        [(self.x0, self.y0), (self.x1, self.y0), (self.x0, self.y1), (self.x1, self.y1)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::new(x, y, w, h)
    }

    /// Route between the single ports on the sides `sides` picks.
    fn connect(from: Rect, to: Rect, others: &[Rect]) -> Route {
        let (fs, ts) = sides(from, to);
        let f = Port { at: port_at(from, fs, 0, 1), side: fs };
        let t = Port { at: port_at(to, ts, 0, 1), side: ts };
        route(f, from, t, to, others, None)
    }

    fn enters(points: &[P], r: Rect) -> bool {
        let b = Bx::of(r, 0.0);
        points.windows(2).any(|w| crosses(w[0], w[1], &b))
    }

    /// Largest turn, in degrees, between consecutive non-degenerate segments.
    fn max_turn(points: &[P]) -> f32 {
        let dirs: Vec<P> = points
            .windows(2)
            .filter(|w| w[0] != w[1])
            .map(|w| {
                let d = dist(w[0], w[1]);
                ((w[1].0 - w[0].0) / d, (w[1].1 - w[0].1) / d)
            })
            .collect();
        dirs.windows(2)
            .map(|w| (w[0].0 * w[1].0 + w[0].1 * w[1].1).clamp(-1.0, 1.0).acos().to_degrees())
            .fold(0.0, f32::max)
    }

    /// Draw rects and a route on a character grid, for eyeballing the shapes.
    fn picture(rects: &[Rect], r: &Route) -> String {
        let (mut w, mut h) = (0usize, 0usize);
        for rc in rects {
            w = w.max((rc.x + rc.width) as usize + 2);
            h = h.max((rc.y + rc.height) as usize + 2);
        }
        let mut g = vec![vec![' '; w]; h];
        for rc in rects {
            for y in rc.y..rc.y + rc.height {
                for x in rc.x..rc.x + rc.width {
                    let edge = y == rc.y || y + 1 == rc.y + rc.height || x == rc.x || x + 1 == rc.x + rc.width;
                    g[y as usize][x as usize] = if edge { '#' } else { '.' };
                }
            }
        }
        for seg in r.points.windows(2) {
            let n = (dist(seg[0], seg[1]) * 4.0).ceil().max(1.0) as usize;
            for k in 0..=n {
                let p = lerp(seg[0], seg[1], k as f32 / n as f32);
                let (x, y) = (p.0.floor() as i64, p.1.floor() as i64);
                if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                    g[y as usize][x as usize] = '*';
                }
            }
        }
        g.iter().map(|row| row.iter().collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn straight_drop_between_stacked_rects() {
        let (a, b) = (rect(10, 0, 10, 4), rect(10, 14, 10, 4));
        let r = connect(a, b, &[]);
        assert!(r.clean);
        let first = *r.points.first().unwrap();
        let last = *r.points.last().unwrap();
        assert_eq!(first, port_at(a, Side::Bottom, 0, 1));
        assert_eq!(last, port_at(b, Side::Top, 0, 1));
        for p in &r.points {
            assert!((p.0 - first.0).abs() < 1.0, "wandered to {p:?}");
            assert!(p.1 >= first.1 && p.1 <= last.1, "doubled back at {p:?}");
        }
    }

    #[test]
    fn detours_around_a_rect_squarely_between() {
        let (a, b) = (rect(10, 0, 10, 4), rect(10, 14, 10, 4));
        let wall = rect(8, 8, 14, 2);
        let r = connect(a, b, &[wall]);
        println!("{}", picture(&[a, b, wall], &r));
        assert!(r.clean);
        assert!(!enters(&r.points, wall), "route cuts through the obstacle");
        assert_eq!(r.points.first(), Some(&port_at(a, Side::Bottom, 0, 1)));
        assert_eq!(r.points.last(), Some(&port_at(b, Side::Top, 0, 1)));

        // Control: without the obstacle the same edge goes straight through it.
        let straight = connect(a, b, &[]);
        assert!(enters(&straight.points, wall));
    }

    #[test]
    fn side_by_side_rects_connect_right_to_left() {
        let (a, b) = (rect(0, 0, 10, 4), rect(20, 0, 10, 4));
        assert_eq!(sides(a, b), (Side::Right, Side::Left));
        assert_eq!(sides(b, a), (Side::Left, Side::Right));
        // Below and near: a drop. Below but far across -- more columns than
        // twice the rows, which is what the eye calls "across" in cells that
        // are twice as tall as wide -- a sidestep, so the curve is steep
        // where it turns rather than flat where it crosses.
        let below = rect(12, 12, 10, 4);
        assert_eq!(sides(a, below), (Side::Bottom, Side::Top));
        assert_eq!(sides(below, a), (Side::Top, Side::Bottom));
        let far_below = rect(60, 4, 10, 4);
        assert_eq!(sides(a, far_below), (Side::Right, Side::Left));
        assert_eq!(sides(far_below, a), (Side::Left, Side::Right));
        // Rows shared, even by one: beside.
        assert_eq!(sides(a, rect(60, 3, 10, 4)), (Side::Right, Side::Left));
        let r = connect(a, b, &[]);
        assert!(r.clean);
        let n = r.points.len();
        assert!(r.points[1].0 > r.points[0].0, "does not leave rightwards");
        assert!(r.points[n - 2].0 < r.points[n - 1].0, "does not arrive from the left");
        assert_eq!(r.points[1].1, r.points[0].1);
        assert_eq!(r.points[n - 2].1, r.points[n - 1].1);
    }

    #[test]
    fn ports_spread_along_a_side_and_avoid_corners() {
        let wide = rect(0, 0, 10, 4);
        let cols: Vec<f32> = (0..3).map(|k| port_at(wide, Side::Top, k, 3).0).collect();
        assert!(cols[0] < cols[1] && cols[1] < cols[2], "not spread: {cols:?}");
        for c in &cols {
            assert!(*c > 1.0 && *c < 9.0, "on a corner column: {c}");
            assert_eq!(port_at(wide, Side::Top, 0, 1).1, -0.5, "the row above the top border");
        }
        assert_eq!(port_at(wide, Side::Bottom, 0, 1).1, 4.5);
        assert_eq!(port_at(wide, Side::Left, 0, 1).0, -0.5);
        assert_eq!(port_at(wide, Side::Right, 0, 1).0, 10.5);

        let narrow = rect(5, 5, 3, 3);
        for n in 1..6 {
            for k in 0..n {
                assert_eq!(port_at(narrow, Side::Top, k, n), (6.5, 4.5));
                assert_eq!(port_at(narrow, Side::Right, k, n), (8.5, 6.5));
            }
        }
    }

    #[test]
    fn walled_in_route_falls_back_to_a_direct_curve() {
        // A ring close enough to `a` to count as an obstacle on every side.
        let (a, b) = (rect(10, 10, 10, 4), rect(10, 30, 10, 4));
        let ring = [rect(4, 4, 22, 2), rect(4, 18, 22, 2), rect(4, 4, 2, 16), rect(24, 4, 2, 16)];
        let r = connect(a, b, &ring);
        println!("{}", picture(&[a, b, ring[0], ring[1], ring[2], ring[3]], &r));
        assert!(!r.clean);
        assert_eq!(r.points.first(), Some(&port_at(a, Side::Bottom, 0, 1)));
        assert_eq!(r.points.last(), Some(&port_at(b, Side::Top, 0, 1)));

        // Control: open the ring and the route is clean again.
        let open = connect(a, b, &ring[..1]);
        assert!(open.clean);
    }

    #[test]
    fn detours_are_smooth_and_stubs_are_perpendicular() {
        // The block's inflated left side lines up with a's bottom port and its
        // inflated bottom with b's left port, and it is far too wide to go over,
        // so the taut path is a true L.
        let (a, b) = (rect(0, 0, 6, 3), rect(20, 16, 6, 3));
        let block = rect(5, 6, 30, 10);
        let f = Port { at: port_at(a, Side::Bottom, 0, 1), side: Side::Bottom };
        let t = Port { at: port_at(b, Side::Left, 0, 1), side: Side::Left };
        let r = route(f, a, t, b, &[block], None);
        println!("{}", picture(&[a, b, block], &r));
        assert!(r.clean);
        assert!(!enters(&r.points, block));

        let turn = max_turn(&r.points);
        assert!(turn < 45.0, "sharpest turn {turn}°");
        let n = r.points.len();
        assert_eq!(r.points[1].0, r.points[0].0, "stub out of a Bottom port is not vertical");
        assert!(r.points[1].1 > r.points[0].1);
        assert_eq!(r.points[n - 2].1, r.points[n - 1].1, "stub into a Left port is not horizontal");
        assert!(r.points[n - 2].0 < r.points[n - 1].0);

        // Control: before smoothing, the same path has a right angle in it.
        let sp = step(f.at, Side::Bottom, STUB);
        let tp = step(t.at, Side::Left, STUB);
        let raw = Field::new(sp, tp, a, b, &[block], None, MARGIN).taut_path().unwrap();
        assert!((max_turn(&raw) - 90.0).abs() < 1e-3, "taut path is not an L: {raw:?}");
    }

    /// An offset drop is one S-curve, not a diagonal with a hook at each
    /// end: it turns gently the whole way, never doubles back on either
    /// axis, and is halfway across when it is halfway down. The same holds
    /// of a shallow one, far across and barely down, which must not wave.
    #[test]
    fn an_offset_drop_is_one_gentle_s_curve() {
        let (a, b) = (rect(0, 0, 10, 4), rect(30, 20, 10, 4));
        let r = connect(a, b, &[]);
        println!("{}", picture(&[a, b], &r));
        assert!(r.clean);
        let turn = max_turn(&r.points);
        assert!(turn < 12.0, "sharpest turn {turn}°");
        for w in r.points.windows(2) {
            assert!(w[1].0 >= w[0].0 - 1e-3 && w[1].1 >= w[0].1 - 1e-3, "doubles back at {w:?}");
        }
        let shallow = connect(rect(0, 0, 10, 4), rect(40, 9, 10, 4), &[]);
        println!("{}", picture(&[rect(0, 0, 10, 4), rect(40, 9, 10, 4)], &shallow));
        for w in shallow.points.windows(2) {
            assert!(w[1].0 >= w[0].0 - 1e-3 && w[1].1 >= w[0].1 - 1e-3, "the shallow drop waves at {w:?}");
        }
        let (first, last) = (r.points[0], r.points[r.points.len() - 1]);
        let mid_y = (first.1 + last.1) / 2.0;
        let at_mid = r.points.iter().min_by(|p, q| (p.1 - mid_y).abs().total_cmp(&(q.1 - mid_y).abs())).unwrap();
        assert!((at_mid.0 - (first.0 + last.0) / 2.0).abs() < 1.5, "lopsided: {at_mid:?}");

        // Control: the same path merely rounded has a real corner where the
        // diagonal meets a stub.
        let sp = step(first, Side::Bottom, STUB);
        let tp = step(last, Side::Top, STUB);
        let rounded = smooth(&[first, sp, tp, last]);
        assert!(max_turn(&rounded) > 20.0, "the control is not a hook: {}", max_turn(&rounded));
    }

    /// An edge among a box's children stays on the box's floor. A detour
    /// that would leave the interior is not taken even when it is the
    /// shorter way round, because out there it would be drawn under the
    /// frame and simply vanish.
    #[test]
    fn a_route_stays_inside_the_box_it_is_drawn_in() {
        let floor = rect(0, 0, 40, 30);
        let (a, b) = (rect(2, 2, 10, 4), rect(2, 24, 10, 4));
        // Reaches the floor's left edge: the short way round is outside.
        let wall = rect(0, 12, 26, 4);
        let (fs, ts) = sides(a, b);
        let f = Port { at: port_at(a, fs, 0, 1), side: fs };
        let t = Port { at: port_at(b, ts, 0, 1), side: ts };
        let inside = route(f, a, t, b, &[wall], Some(floor));
        println!("{}", picture(&[a, b, wall], &inside));
        assert!(inside.clean);
        assert!(!enters(&inside.points, wall));
        for p in &inside.points {
            assert!(p.0 >= 0.0 && p.0 <= 40.0 && p.1 >= 0.0 && p.1 <= 30.0, "left the floor at {p:?}");
        }
        assert!(inside.points.iter().any(|p| p.0 > 26.0), "did not go round the open side");

        // Control: unwalled, the shorter way round is to the left, outside.
        let loose = route(f, a, t, b, &[wall], None);
        assert!(loose.points.iter().any(|p| p.0 < 0.0), "control did not leave the floor");
    }

    /// Wrapped rows of a big level sit two cells apart. There is no room for
    /// air there, but there is room for a line, and a line that squeezes
    /// through beats one drawn across the nodes.
    #[test]
    fn a_two_cell_channel_is_still_routed_through_not_across() {
        let a = rect(0, 0, 10, 4);
        let b = rect(0, 20, 10, 4);
        // Two wrapped rows between them, spanning the whole width, with a
        // two-cell gap between the rows and a two-cell gutter at each end.
        let row1 = [rect(14, 6, 10, 4), rect(26, 6, 10, 4)];
        let row2 = [rect(14, 12, 10, 4), rect(26, 12, 10, 4)];
        let mut others = row1.to_vec();
        others.extend(row2);
        let r = connect(a, b, &others);
        println!("{}", picture(&[a, b, row1[0], row1[1], row2[0], row2[1]], &r));
        assert!(r.clean, "a two-cell channel should be routable");
        for o in &others {
            assert!(!enters(&r.points, *o), "the route cuts through {o:?}");
        }

        // Control: one cell apart and there is no channel at all.
        let squeezed = [rect(14, 6, 10, 4), rect(26, 6, 10, 4), rect(14, 11, 10, 4), rect(26, 11, 10, 4)];
        let far = rect(0, 30, 10, 4);
        let wall = [rect(0, 8, 40, 4)];
        let blocked = connect(a, far, &wall);
        assert!(!blocked.clean || !enters(&blocked.points, wall[0]));
        let _ = squeezed;
    }

    #[test]
    fn routing_is_deterministic() {
        let (a, b) = (rect(0, 0, 8, 3), rect(30, 20, 8, 3));
        let others = [rect(10, 4, 6, 6), rect(18, 12, 6, 6), rect(12, 14, 4, 4), rect(26, 6, 3, 8)];
        let r1 = connect(a, b, &others);
        let r2 = connect(a, b, &others);
        assert!(r1.clean);
        assert_eq!(r1, r2);
    }

    #[test]
    fn many_routes_among_many_obstacles_are_quick() {
        // A grid of 40 boxes, 8 across by 5 down, with 3-cell gutters.
        let boxes: Vec<Rect> = (0..40).map(|i| rect(2 + (i % 8) * 13, 2 + (i / 8) * 7, 10, 4)).collect();
        let mut seed = 12345u32;
        let mut next = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 16) as usize % boxes.len()
        };
        let start = Instant::now();
        let mut clean = 0;
        for _ in 0..300 {
            let (i, j) = (next(), next());
            if i == j {
                continue;
            }
            let others: Vec<Rect> = boxes.iter().copied().filter(|r| *r != boxes[i] && *r != boxes[j]).collect();
            let r = connect(boxes[i], boxes[j], &others);
            assert_eq!(r.points.first(), Some(&port_at(boxes[i], sides(boxes[i], boxes[j]).0, 0, 1)));
            clean += usize::from(r.clean);
        }
        let took = start.elapsed();
        println!("300 routes among 40 obstacles: {took:?} ({clean} clean)");
        assert!(took.as_millis() < 2000, "far too slow: {took:?}");
    }
}
