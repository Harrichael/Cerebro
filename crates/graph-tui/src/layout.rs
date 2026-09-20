//! Where the boxes go, and remembering it.
//!
//! Two layers, one pipeline. The *virtual* layer is a position per node,
//! relative to the container holding it, in unit cells. It persists across
//! rebuilds and is written by exactly three things: ranking nodes that have
//! no position yet ([`Layout::settle`]), a drag ([`Layout::nudge`]) and a
//! drag being kept ([`Layout::commit`]). The *material* layer is a
//! [`Diagram`] of integer rects at one zoom, derived from the virtual layer
//! whenever anything changes: scale, push overlapping siblings apart, fit
//! every box round its children, emit.
//!
//! A position names a node's top-left corner, leaf or box alike, and a
//! box's children are measured from just inside that corner. So a leaf
//! that opens into a box grows down and right from where it was, its
//! children starting where its name was, and nothing the user did not
//! touch moves because something near it changed size.
//!
//! The push is resolved once and kept: whatever a materialisation moved to
//! make room is the position from then on ([`Layout::commit`]). Deriving
//! the push afresh from untouched positions every frame meant the
//! arrangement on screen was never the one being edited, and an unrelated
//! action could redistribute it.
//!
//! Ranking is per container, bottom-up: a container's direct children are
//! ranked only by the edges among *them*, and the finished container is one
//! fixed-size node a level up. Within a level the children are arranged by
//! what they are ([`arrange`]): the structure -- folders, modules, files --
//! across the top, and beneath it the functions in one column, ranked by
//! who calls whom, beside the types in another, ranked by who refers to
//! whom. A file's functions and its types ranked together came out as one
//! tangle in which neither the call graph nor the type hierarchy could be
//! read. Ranking every leaf together does not work --
//! measured on this repo, a global rank of seventeen nodes needed ten ranks
//! and 106x57 cells; per container the deepest box was 21x33. The browser
//! learned the same thing the same way (`graph-server/ui/layout.js`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use entity_graph::{EntityId, EntityKind};
use ratatui::layout::Rect;

use crate::label::Labels;
use crate::rank::Rank;
use crate::scene::{DrawnEdge, Scene};
use crate::zoom::{Metrics, Zoom};

/// A box spends a row on its own name, plus a border on each side. Unlike the
/// rest of the measurements this does not vary with zoom: a box with no frame
/// is not a box.
pub const BOX_PAD: u16 = 1;
pub const LABEL_H: u16 = 1;
/// Where a box's children start, relative to its top-left corner.
pub const INSET: (i32, i32) = (BOX_PAD as i32 + 1, LABEL_H as i32 + 1);
/// What a box adds around its children in all.
const FRAME: (i32, i32) = (2 * (BOX_PAD as i32 + 1), BOX_PAD as i32 + LABEL_H as i32 + 2);
/// Room in a box's title row for its buttons and the gap before them.
const BUTTONS_W: i32 = 4;
/// Passes of pushing before giving up on a level that will not settle.
const PUSH_ROUNDS: usize = 64;
/// The least clearance the push leaves between siblings. Deliberately less
/// than the gaps ranking uses: those are where the edges like to run, this
/// is only what stops two frames touching. A rank that wraps packs its rows
/// one apart, and a user may drop a node wherever it fits.
const CLEARANCE: (i32, i32) = (2, 2);
/// Edges across one gap that it already has room for. A gap fits a line or
/// two as it stands; past that each wants a cell of its own to run in.
const FREE_TRACKS: usize = 2;
/// Rows of one rank that wrapped are this far apart.
const WRAP_GAP: u16 = 2;
/// Width over height a box is happiest at, in cells. A cell is about twice
/// as tall as it is wide, so this is a box that *looks* wider than tall
/// without being a ribbon -- and the terminal it is read in is wide.
const TARGET_ASPECT: f32 = 3.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub id: EntityId,
    pub rect: Rect,
    /// A container drawn around other nodes, as opposed to a cursor leaf.
    pub is_box: bool,
    /// Roots are 1. What the renderer uses to keep an edge inside its level.
    pub depth: u8,
}

impl Placed {
    /// Is this cell part of the room a box keeps for its children, rather
    /// than the frame it draws around them? The renderer fills the frame with
    /// the box's own ink and leaves this to whatever is inside, so the two
    /// have to agree: a cell drawn as the box's that the hit-test does not
    /// count as the box's is a cell the user can click and nothing happens.
    pub fn holds(&self, point: (u16, u16)) -> bool {
        let r = self.rect;
        self.is_box
            && point.0 > r.x
            && point.0 + 1 < r.right()
            && point.1 >= r.y + INSET.1 as u16
            && point.1 + 1 < r.bottom()
    }
}

pub struct Diagram {
    pub nodes: Vec<Placed>,
    pub edges: Vec<DrawnEdge>,
    pub width: u16,
    pub height: u16,
    /// What the nodes were sized for. Carried so the renderer cannot fill a
    /// box that was measured for one zoom using the text of another.
    pub zoom: Zoom,
}

impl Diagram {
    pub fn empty() -> Diagram {
        Diagram { nodes: Vec::new(), edges: Vec::new(), width: 0, height: 0, zoom: Zoom::Close }
    }

    pub fn extent(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// The leaf drawn at a diagram cell. Boxes are deliberately not hit: a box
    /// is an ancestor of whatever is being pointed at, and the selection has
    /// to stay a leaf for expanding to have something to act on.
    pub fn leaf_at(&self, point: (u16, u16)) -> Option<EntityId> {
        self.nodes
            .iter()
            .find(|n| !n.is_box && n.rect.contains(point.into()))
            .map(|n| n.id)
    }

    /// The box whose frame is under a cell -- its title rows, its sides and
    /// its bottom. That frame is the whole of what a box draws for itself
    /// rather than keeps for its children, which makes it both the handle to
    /// drag the box by and the place to click to select it. The deepest wins,
    /// since a nested box's frame stands in its parent's room.
    pub fn handle_at(&self, point: (u16, u16)) -> Option<EntityId> {
        self.nodes
            .iter()
            .filter(|n| n.is_box && n.rect.contains(point.into()) && !n.holds(point))
            .max_by_key(|n| n.depth)
            .map(|n| n.id)
    }

    /// Every leaf touching `area`.
    pub fn leaves_in(&self, area: Rect) -> Vec<EntityId> {
        self.nodes
            .iter()
            .filter(|n| !n.is_box && n.rect.intersects(area))
            .map(|n| n.id)
            .collect()
    }

    pub fn rect_of(&self, id: EntityId) -> Option<Rect> {
        self.nodes.iter().find(|n| n.id == id).map(|n| n.rect)
    }
}

/// What a materialisation is allowed to move.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Nodes that never yield to a push: the ones under the pointer.
    pub anchored: BTreeSet<EntityId>,
    /// Levels where nothing is pushed at all this frame. While a node is
    /// being dragged, the thing under the pointer should move and nothing
    /// else should twitch; the siblings make way when it is dropped.
    pub loose: BTreeSet<Option<EntityId>>,
}

#[derive(Debug, Clone, Default)]
pub struct Layout {
    /// Each node's top-left corner, relative to the interior origin of the
    /// box holding it -- [`INSET`] in from that box's own corner -- or to
    /// the canvas for a root, in cells at [`Zoom::Close`]. A box's frame may
    /// be drawn away from its corner while a child is dragged before the
    /// origin; a commit then makes the frame's corner the position again.
    pos: BTreeMap<EntityId, (f32, f32)>,
    /// Where each node stands among its siblings, as an order rather than a
    /// distance. The material positions above are what is drawn and what a
    /// drag edits; this is what says where a node belongs when something has
    /// to put it back -- and it is remembered, not read back off the picture,
    /// because the picture cannot always be read back (a level packed across
    /// centres its columns, so a rank is not a row).
    rank: BTreeMap<EntityId, Rank>,
}

impl Layout {
    pub fn new() -> Layout {
        Layout::default()
    }

    #[doc(hidden)]
    pub fn debug_pos(&self, id: EntityId) -> Option<(f32, f32)> {
        self.pos.get(&id).copied()
    }

    pub fn is_placed(&self, id: EntityId) -> bool {
        self.pos.contains_key(&id)
    }

    /// Forget every position: the next `settle` ranks the whole picture afresh.
    pub fn clear(&mut self) {
        self.pos.clear();
        self.rank.clear();
    }

    pub fn rank_of(&self, id: EntityId) -> Option<&Rank> {
        self.rank.get(&id)
    }

    /// Remember where a node now stands among `siblings`, which are given in
    /// the order they are drawn. Called when a drop has moved it: the picture
    /// is what changed, and this is the picture read back for the one node
    /// that moved, which is the one case reading back is unambiguous.
    pub fn reranked(&mut self, id: EntityId, siblings: &[EntityId]) {
        let at = siblings.iter().position(|&s| s == id);
        let Some(at) = at else { return };
        let before = at.checked_sub(1).and_then(|i| self.rank.get(&siblings[i])).cloned();
        let after = siblings.get(at + 1).and_then(|s| self.rank.get(s)).cloned();
        self.rank.insert(id, Rank::between(before.as_ref(), after.as_ref()));
    }

    /// Carry positions across a rebuild that renumbered every entity.
    pub fn migrate(&mut self, map: &[Option<EntityId>]) {
        self.pos = std::mem::take(&mut self.pos)
            .into_iter()
            .filter_map(|(id, p)| Some((map.get(id.0).copied().flatten()?, p)))
            .collect();
        self.rank = std::mem::take(&mut self.rank)
            .into_iter()
            .filter_map(|(id, r)| Some((map.get(id.0).copied().flatten()?, r)))
            .collect();
    }

    /// Move nodes by a screen distance at the zoom they are being looked at.
    pub fn nudge(&mut self, ids: &[EntityId], by: (i32, i32), zoom: Zoom) {
        let scale = zoom.scale();
        for id in ids {
            if let Some(p) = self.pos.get_mut(id) {
                p.0 += by.0 as f32 / scale;
                p.1 += by.1 as f32 / scale;
            }
        }
    }

    /// Give every drawn node without a position one.
    ///
    /// A level whose children are all new is ranked: layered by the edges
    /// among them, ranks wrapped at the width the level has. New children on
    /// a level that already has some -- a file that appeared on reload --
    /// go in a row beneath what is there, and the push sorts out the rest.
    pub fn settle(&mut self, scene: &Scene, labels: &Labels, max_w: u16) {
        let m = Zoom::Close.metrics();
        // Levels come deepest first, so by the time a box is a child to be
        // ranked its own children are settled and its frame is final. Kept,
        // rather than fitted again for every ancestor: a level of several
        // hundred leaves fitted once per nesting level is what made a deep
        // tree take seconds.
        let mut frames: HashMap<EntityId, Frame> = HashMap::new();
        let plain = Options::default();
        for level in scene.levels_bottom_up() {
            let kids = scene.children(level);
            let fresh: Vec<usize> =
                (0..kids.len()).filter(|&i| !self.pos.contains_key(&kids[i])).collect();
            if !fresh.is_empty() {
                // Each nesting level spends a frame, so the room left for a
                // rank shrinks with depth; ranking against the full width at
                // every level would let deep boxes overflow the viewport.
                let depth = level.map_or(0, |c| i32::from(scene.depth(c)));
                let avail = (i32::from(max_w) - FRAME.0 * depth).max(i32::from(m.min_inner_w));
                let kid_frames: Vec<Frame> = {
                    let mut fitter = Fitter::new(self, scene, labels, Zoom::Close, &plain);
                    fitter.frames = std::mem::take(&mut frames);
                    let out = kids.iter().map(|&k| fitter.frame(k)).collect();
                    frames = fitter.frames;
                    out
                };
                // Ranking places frames; what is kept is each frame's origin.
                let sizes: Vec<(u16, u16)> =
                    kid_frames.iter().map(|f| (f.size.0 as u16, f.size.1 as u16)).collect();
                let idx: HashMap<EntityId, usize> =
                    kids.iter().enumerate().map(|(i, &k)| (k, i)).collect();
                let edges: Vec<(usize, usize)> = scene
                    .edges_at(level)
                    .filter_map(|e| Some((*idx.get(&e.from)?, *idx.get(&e.to)?)))
                    .collect();
                let kinds: Vec<EntityKind> =
                    kids.iter().map(|&k| labels.kind(k).unwrap_or(EntityKind::Function)).collect();
                let (want, _, _) = arrange(&sizes, &kinds, &edges, avail as u16, &m, level.is_none());
                let mut placed: Vec<(EntityId, (i32, i32))> = Vec::new();
                if fresh.len() == kids.len() {
                    for (i, &k) in kids.iter().enumerate() {
                        placed.push((k, (i32::from(want[i].0), i32::from(want[i].1))));
                    }
                } else {
                    // A level already arranged, with something new turning up
                    // in it: a file added and saved. `want` is where the
                    // ranking would have put it, which says who it belongs
                    // between; the ranks those neighbours already carry say
                    // how to put it there without renumbering either of them.
                    // Dropping it in a row under everything -- which is what
                    // this did -- put a new file at the foot of its folder
                    // however obviously it belonged higher up.
                    let mut order: Vec<usize> = (0..kids.len()).collect();
                    order.sort_by_key(|&i| (want[i].1, want[i].0, i));
                    let mut origin: Vec<Option<(i32, i32)>> = (0..kids.len())
                        .map(|i| {
                            let p = self.pos.get(&kids[i])?;
                            Some((
                                p.0.round() as i32 + kid_frames[i].off.0,
                                p.1.round() as i32 + kid_frames[i].off.1,
                            ))
                        })
                        .collect();
                    for at in 0..order.len() {
                        let i = order[at];
                        if origin[i].is_some() {
                            continue;
                        }
                        let rank = Rank::between(
                            order[..at].iter().rev().find_map(|&x| self.rank.get(&kids[x])),
                            order[at + 1..].iter().find_map(|&x| self.rank.get(&kids[x])),
                        );
                        self.rank.insert(kids[i], rank);
                        // Beside the nearest neighbour that has a place, on
                        // the side its order puts it. Landing on top of one
                        // would do as well -- the push reads the order now --
                        // but starting clear of it keeps the shove small.
                        let w = kid_frames[i].size.0;
                        let before = order[..at].iter().rev().find_map(|&x| Some((x, origin[x]?)));
                        let after = order[at + 1..].iter().find_map(|&x| Some((x, origin[x]?)));
                        let spot = match (before, after) {
                            (Some((x, at)), _) => {
                                (at.0 + kid_frames[x].size.0 + i32::from(m.gap_x), at.1)
                            }
                            (None, Some((_, at))) => (at.0 - w - i32::from(m.gap_x), at.1),
                            (None, None) => (0, 0),
                        };
                        origin[i] = Some(spot);
                        placed.push((kids[i], spot));
                    }
                }
                // Ranked in the order they were placed, reading order, so a
                // level's first ordering agrees with how it is drawn.
                let mut fresh_order: Vec<(EntityId, (i32, i32))> = placed.clone();
                fresh_order.sort_by_key(|(_, at)| (at.1, at.0));
                for (n, (k, _)) in fresh_order.into_iter().enumerate() {
                    self.rank.entry(k).or_insert_with(|| Rank::nth(n));
                }
                for (i, (k, at)) in placed.into_iter().enumerate() {
                    let off = kid_frames[kids.iter().position(|&x| x == k).unwrap_or(i)].off;
                    self.pos.insert(k, ((at.0 - off.0) as f32, (at.1 - off.1) as f32));
                }
            }
            if let Some(c) = level {
                let mut fitter = Fitter::new(self, scene, labels, Zoom::Close, &plain);
                fitter.frames = std::mem::take(&mut frames);
                fitter.fit(Some(c));
                frames = fitter.frames;
            }
        }
    }

    /// The picture at one zoom: scale, push, fit, emit.
    pub fn materialize(
        &self,
        scene: &Scene,
        labels: &Labels,
        zoom: Zoom,
        opts: &Options,
    ) -> Diagram {
        let mut fitter = Fitter::new(self, scene, labels, zoom, opts);
        let whole = fitter.fit(None);
        let mut nodes = Vec::new();
        fitter.emit(None, fitter.root, 1, &mut nodes);
        Diagram { nodes, edges: scene.edges.clone(), width: whole.size.0 as u16, height: whole.size.1 as u16, zoom }
    }

    /// Make the material positions the virtual ones, so the arrangement on
    /// screen is the one every later materialisation starts from. Levels
    /// nothing pushed are unchanged by this.
    pub fn commit(&mut self, diagram: &Diagram, scene: &Scene) {
        let scale = diagram.zoom.scale();
        let rect_of: HashMap<EntityId, Rect> = diagram.nodes.iter().map(|n| (n.id, n.rect)).collect();
        for node in &diagram.nodes {
            let origin = match scene.parent.get(&node.id) {
                Some(c) => match rect_of.get(c) {
                    Some(r) => (i32::from(r.x) + INSET.0, i32::from(r.y) + INSET.1),
                    None => continue,
                },
                None => (0, 0),
            };
            self.pos.insert(
                node.id,
                (
                    (i32::from(node.rect.x) - origin.0) as f32 / scale,
                    (i32::from(node.rect.y) - origin.1) as f32 / scale,
                ),
            );
        }
    }
}

/// Bounded on both sides: nothing is narrower than a few cells, and no label
/// sets the width of the thing holding it. Minified sources really do produce
/// identifiers tens of thousands of characters long, and one of them would
/// otherwise size a box past what the arithmetic below can hold.
fn width_of(text: &str, m: &Metrics) -> u16 {
    // Measured only as far as could matter. Width is linear in the text, and
    // a vendored file has hundreds of names that run to thousands of
    // characters; measuring all of each made sizing one level take longer
    // than routing every edge in the picture.
    let head: String = text.chars().take(m.label_max + 1).collect();
    unicode_width::UnicodeWidthStr::width(head.as_str()).clamp(6, m.label_max) as u16
}

/// Display width of what a node has to fit. A leaf is sized by whichever of
/// its two lines is wider -- sizing it by the name alone truncates
/// `function · 10–30` on every short-named function there is.
pub fn label_width(labels: &Labels, id: EntityId, is_box: bool, m: &Metrics) -> u16 {
    if is_box {
        width_of(&labels.head(id, m.box_size), m)
    } else if m.detail {
        width_of(labels.name(id), m).max(width_of(&labels.detail(id), m))
    } else {
        width_of(labels.name(id), m)
    }
}

/// Wide enough for its label, its frame, and one port per edge touching it.
fn leaf_size(labels: &Labels, id: EntityId, degree: u16, m: &Metrics) -> (i32, i32) {
    (i32::from(label_width(labels, id, false, m).max(degree)) + 2, i32::from(m.leaf_h))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Geo {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// Push overlapping siblings apart until none overlap, keeping their order.
///
/// A pair is pushed along the axis their positions differ on more, each
/// difference measured against the smaller of the two on that axis: a leaf
/// beside a box that has just grown taller than it is still *beside* it,
/// and goes right rather than up out of its row. Both move half the way
/// unless one is anchored, in which case the other moves all of it.
///
/// Two nodes at the same place are the case geometry cannot answer: there is
/// no "differ on more" between two zeroes. `first` breaks it -- the position
/// each node holds in its level's remembered order -- because the fallback
/// was the order the graph happened to list them in, which is nothing the
/// reader chose and nothing they can change.
fn separate(geos: &mut [Geo], anchored: &[bool], first: &[usize], gap: (i32, i32)) {
    let mut order: Vec<usize> = (0..geos.len()).collect();
    for _ in 0..PUSH_ROUNDS {
        let mut moved = false;
        // Sorted by left edge, so a pair is only looked at while it could
        // still overlap in x: a level of several hundred leaves is the
        // ordinary case for a vendored file, and every pair of them is not.
        order.sort_by_key(|&i| (geos[i].x, i));
        for oi in 0..order.len() {
            let i = order[oi];
            for &j in &order[oi + 1..] {
                if geos[j].x >= geos[i].x + geos[i].w + gap.0 {
                    break;
                }
                let (i, j) = if i < j { (i, j) } else { (j, i) };
                if anchored[i] && anchored[j] {
                    continue;
                }
                let (a, b) = (geos[i], geos[j]);
                let ox = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x) + gap.0;
                let oy = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y) + gap.1;
                if ox <= 0 || oy <= 0 {
                    continue;
                }
                let dx = (b.x - a.x) as f32 / a.w.min(b.w).max(1) as f32;
                let dy = (b.y - a.y) as f32 / a.h.min(b.h).max(1) as f32;
                let (along_y, overlap, sign) = if dy.abs() > dx.abs() {
                    (true, oy, if dy >= 0.0 { 1 } else { -1 })
                } else {
                    (false, ox, if dx >= 0.0 { 1 } else { -1 })
                };
                // `sign` of 1 sends `i` back and `j` on; exactly on top of
                // one another, which of them goes which way is the order's
                // to say.
                let flat = if along_y { b.y == a.y } else { b.x == a.x };
                let sign = match flat {
                    true if first[i] > first[j] => -1,
                    true => 1,
                    false => sign,
                };
                let (ma, mb) = match (anchored[i], anchored[j]) {
                    (true, _) => (0, overlap),
                    (_, true) => (overlap, 0),
                    _ => (overlap - overlap / 2, overlap / 2),
                };
                if along_y {
                    geos[i].y -= ma * sign;
                    geos[j].y += mb * sign;
                } else {
                    geos[i].x -= ma * sign;
                    geos[j].x += mb * sign;
                }
                moved = true;
            }
        }
        if !moved {
            return;
        }
    }
}

/// A box's frame, relative to its position. `off` is zero for a leaf and
/// for a box whose children start at its origin, and is wherever they do
/// start otherwise: the frame hugs the children, so a child dragged before
/// the origin takes the frame with it while every other child stays put.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Frame {
    off: (i32, i32),
    size: (i32, i32),
}

/// One materialisation in progress: frames bottom-up, then rects top-down.
struct Fitter<'a> {
    layout: &'a Layout,
    scene: &'a Scene,
    labels: &'a Labels<'a>,
    m: Metrics,
    scale: f32,
    loose: &'a BTreeSet<Option<EntityId>>,
    /// The nodes asked for, and every box holding one: a box that yielded
    /// would carry its anchored child with it.
    anchored: HashSet<EntityId>,
    /// Each level's children, relative to the level's interior origin.
    rels: HashMap<Option<EntityId>, Vec<(EntityId, Geo)>>,
    /// Frames already fitted, for a caller that fits level by level.
    frames: HashMap<EntityId, Frame>,
    /// Where the root origin lands on the canvas, whose top-left is the
    /// top-left of whatever is drawn.
    root: (i32, i32),
}

impl<'a> Fitter<'a> {
    fn new(
        layout: &'a Layout,
        scene: &'a Scene,
        labels: &'a Labels<'a>,
        zoom: Zoom,
        opts: &'a Options,
    ) -> Self {
        let mut anchored = HashSet::new();
        for &id in &opts.anchored {
            let mut cur = Some(id);
            while let Some(c) = cur {
                if !anchored.insert(c) {
                    break;
                }
                cur = scene.parent.get(&c).copied();
            }
        }
        Fitter {
            layout,
            scene,
            labels,
            m: zoom.metrics(),
            scale: zoom.scale(),
            loose: &opts.loose,
            anchored,
            rels: HashMap::new(),
            frames: HashMap::new(),
            root: (0, 0),
        }
    }

    /// Each node's place in the level's remembered order, as an index. Nodes
    /// with no rank yet -- new this frame -- keep the order they are listed
    /// in, which is the best that can be said about them.
    fn order_of(&self, kids: &[EntityId]) -> Vec<usize> {
        let mut by_rank: Vec<usize> = (0..kids.len()).collect();
        by_rank.sort_by(|&a, &b| {
            self.layout.rank_of(kids[a]).cmp(&self.layout.rank_of(kids[b])).then(a.cmp(&b))
        });
        let mut first = vec![0; kids.len()];
        for (place, &i) in by_rank.iter().enumerate() {
            first[i] = place;
        }
        first
    }

    fn frame(&mut self, id: EntityId) -> Frame {
        if self.scene.is_box(id) {
            if let Some(&f) = self.frames.get(&id) {
                return f;
            }
            return self.fit(Some(id));
        }
        let degree = self.scene.degree.get(&id).copied().unwrap_or(0);
        Frame { off: (0, 0), size: leaf_size(self.labels, id, degree, &self.m) }
    }

    /// Place a level's children relative to its origin and return its frame.
    fn fit(&mut self, level: Option<EntityId>) -> Frame {
        let kids = self.scene.children(level).to_vec();
        let mut geos: Vec<Geo> = Vec::with_capacity(kids.len());
        for &k in &kids {
            let f = self.frame(k);
            let (px, py) = self.layout.pos.get(&k).copied().unwrap_or((0.0, 0.0));
            geos.push(Geo {
                x: (px * self.scale).round() as i32 + f.off.0,
                y: (py * self.scale).round() as i32 + f.off.1,
                w: f.size.0,
                h: f.size.1,
            });
        }
        if !self.loose.contains(&level) {
            let anchored: Vec<bool> = kids.iter().map(|k| self.anchored.contains(k)).collect();
            separate(&mut geos, &anchored, &self.order_of(&kids), CLEARANCE);
        }
        let min_x = geos.iter().map(|g| g.x).min().unwrap_or(0);
        let min_y = geos.iter().map(|g| g.y).min().unwrap_or(0);
        let max_x = geos.iter().map(|g| g.x + g.w).max().unwrap_or(0);
        let max_y = geos.iter().map(|g| g.y + g.h).max().unwrap_or(0);
        self.rels.insert(level, kids.into_iter().zip(geos).collect());
        match level {
            None => {
                self.root = (-min_x, -min_y);
                Frame { off: (0, 0), size: (max_x - min_x, max_y - min_y) }
            }
            Some(c) => {
                let title = i32::from(label_width(self.labels, c, true, &self.m)) + FRAME.0 + BUTTONS_W;
                let f = Frame {
                    off: (min_x, min_y),
                    size: ((max_x - min_x + FRAME.0).max(title), max_y - min_y + FRAME.1),
                };
                self.frames.insert(c, f);
                f
            }
        }
    }

    fn emit(&self, level: Option<EntityId>, origin: (i32, i32), depth: u8, out: &mut Vec<Placed>) {
        let Some(rels) = self.rels.get(&level) else { return };
        for &(id, g) in rels {
            let at = (g.x + origin.0, g.y + origin.1);
            let rect = Rect::new(cell(at.0), cell(at.1), cell(g.w), cell(g.h));
            let is_box = self.scene.is_box(id);
            out.push(Placed { id, rect, is_box, depth });
            if let Some(f) = self.frames.get(&id).filter(|_| is_box) {
                let origin = (at.0 - f.off.0 + INSET.0, at.1 - f.off.1 + INSET.1);
                self.emit(Some(id), origin, depth + 1, out);
            }
        }
    }
}

fn cell(v: i32) -> u16 {
    v.clamp(0, i32::from(u16::MAX)) as u16
}

/// Back edges by DFS colouring. A code graph is always cyclic, so ranking has
/// to be told which edges to ignore rather than assuming a DAG.
fn back_edges(n: usize, edges: &[(usize, usize)]) -> HashSet<usize> {
    let mut out: HashMap<usize, Vec<(usize, usize)>> = HashMap::new();
    for (i, &(a, b)) in edges.iter().enumerate() {
        out.entry(a).or_default().push((b, i));
    }
    let (mut colour, mut back) = (vec![0u8; n], HashSet::new());
    for start in 0..n {
        if colour[start] != 0 {
            continue;
        }
        colour[start] = 1;
        let mut stack = vec![(start, 0usize)];
        while let Some(&mut (v, ref mut i)) = stack.last_mut() {
            let empty = Vec::new();
            let adj = out.get(&v).unwrap_or(&empty);
            if *i < adj.len() {
                let (w, ei) = adj[*i];
                *i += 1;
                match colour[w] {
                    1 => {
                        back.insert(ei);
                    }
                    0 => {
                        colour[w] = 1;
                        stack.push((w, 0));
                    }
                    _ => {}
                }
            } else {
                colour[v] = 2;
                stack.pop();
            }
        }
    }
    back
}

/// Longest-path layering, then a median-heuristic sweep to cut crossings.
fn layers(n: usize, edges: &[(usize, usize)]) -> Vec<Vec<usize>> {
    let back = back_edges(n, edges);
    let forward: Vec<(usize, usize)> = edges
        .iter()
        .enumerate()
        .filter(|(i, (a, b))| !back.contains(i) && a != b)
        .map(|(_, &e)| e)
        .collect();

    let mut indeg = vec![0usize; n];
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(a, b) in &forward {
        out[a].push(b);
        indeg[b] += 1;
    }
    let mut queue: Vec<usize> = (0..n).filter(|&v| indeg[v] == 0).collect();
    let mut rank = vec![0usize; n];
    while let Some(v) = queue.pop() {
        for &w in &out[v] {
            rank[w] = rank[w].max(rank[v] + 1);
            indeg[w] -= 1;
            if indeg[w] == 0 {
                queue.push(w);
            }
        }
    }

    let depth = rank.iter().copied().max().map_or(0, |m| m + 1);
    let mut levels: Vec<Vec<usize>> = vec![Vec::new(); depth];
    for v in 0..n {
        levels[rank[v]].push(v);
    }
    let mut up: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(a, b) in &forward {
        up[b].push(a);
    }
    for _ in 0..4 {
        for li in 1..depth {
            let prev: HashMap<usize, usize> =
                levels[li - 1].iter().enumerate().map(|(i, &v)| (v, i)).collect();
            let mut keyed: Vec<(f64, usize)> = levels[li]
                .iter()
                .map(|&v| {
                    let mut ps: Vec<usize> =
                        up[v].iter().filter_map(|u| prev.get(u).copied()).collect();
                    ps.sort_unstable();
                    let m = if ps.is_empty() { f64::MAX } else { ps[ps.len() / 2] as f64 };
                    (m, v)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
            levels[li] = keyed.into_iter().map(|(_, v)| v).collect();
        }
    }
    levels
}

/// Children of one container, already sized, packed into ranks.
///
/// Ranks run top to bottom, or -- inside a box, when that would make a tall
/// thin column of it -- left to right, whichever is nearer a readable shape.
/// A dependency chain inside a file is the ordinary case: ranked downward
/// it is one node per row for the height of the screen. The root level
/// always flows down, so the picture as a whole reads one way. The browser
/// makes the same choice per box (`layout.js`).
fn pack(
    sizes: &[(u16, u16)],
    edges: &[(usize, usize)],
    max_w: u16,
    m: &Metrics,
    downward_only: bool,
) -> (Vec<(u16, u16)>, u16, u16) {
    let ranks = layers(sizes.len(), edges);
    let down = pack_down(sizes, &ranks, edges, max_w, m);
    if downward_only || ranks.len() < 2 {
        return down;
    }
    let across = pack_across(sizes, &ranks, edges, m);
    let score = |w: u16, h: u16| ((f32::from(w.max(1)) / f32::from(h.max(1))).ln() - TARGET_ASPECT.ln()).abs();
    if score(across.1, across.2) < score(down.1, down.2) && across.1 <= max_w {
        across
    } else {
        down
    }
}

/// What a child is to the arrangement of its level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Band {
    Structure,
    Function,
    Type,
}

fn band(kind: EntityKind) -> Band {
    match kind {
        EntityKind::Folder | EntityKind::Module | EntityKind::File => Band::Structure,
        EntityKind::Function => Band::Function,
        EntityKind::Class => Band::Type,
    }
}

/// A level's children in bands: the structure across the top, ranked as a
/// level of its own; beneath it the functions in a column on the left and
/// the types in a column on the right, each ranked downward by the edges
/// among themselves, so one side reads as the call graph and the other as
/// the type hierarchy. A level that is all one band is simply ranked.
fn arrange(
    sizes: &[(u16, u16)],
    kinds: &[EntityKind],
    edges: &[(usize, usize)],
    max_w: u16,
    m: &Metrics,
    downward_only: bool,
) -> (Vec<(u16, u16)>, u16, u16) {
    let members = |b: Band| -> Vec<usize> { (0..sizes.len()).filter(|&i| band(kinds[i]) == b).collect() };
    let (structure, functions, types) = (members(Band::Structure), members(Band::Function), members(Band::Type));
    if [&structure, &functions, &types].iter().filter(|g| !g.is_empty()).count() <= 1 {
        return pack(sizes, edges, max_w, m, downward_only);
    }

    // Each band is ranked on its own, by the edges that stay inside it.
    type Local = (Vec<(u16, u16)>, Vec<(usize, usize)>);
    let local = |group: &[usize]| -> Local {
        let index: HashMap<usize, usize> = group.iter().enumerate().map(|(i, &v)| (v, i)).collect();
        let sizes = group.iter().map(|&v| sizes[v]).collect();
        let edges = edges.iter().filter_map(|(a, b)| Some((*index.get(a)?, *index.get(b)?))).collect();
        (sizes, edges)
    };
    let column = |group: &[usize], width: u16| -> (Vec<(u16, u16)>, u16, u16) {
        if group.is_empty() {
            return (Vec::new(), 0, 0);
        }
        let (sizes, edges) = local(group);
        pack_down(&sizes, &layers(sizes.len(), &edges), &edges, width, m)
    };
    let (mut left, mut right) = (column(&functions, max_w), column(&types, max_w));
    // Side by side when there is room; each in half the room when not.
    let between = m.gap_y;
    if !functions.is_empty() && !types.is_empty() && left.1 + between + right.1 > max_w {
        let half = max_w.saturating_sub(between) / 2;
        left = column(&functions, half);
        right = column(&types, half);
    }
    let top = if structure.is_empty() {
        (Vec::new(), 0, 0)
    } else {
        let (sizes, edges) = local(&structure);
        pack(&sizes, &edges, max_w, m, downward_only)
    };

    let columns_w = match (functions.is_empty(), types.is_empty()) {
        (false, false) => left.1 + between + right.1,
        _ => left.1.max(right.1),
    };
    let width = top.1.max(columns_w);
    let mut pos = vec![(0u16, 0u16); sizes.len()];
    let top_shift = (width - top.1) / 2;
    for (i, &v) in structure.iter().enumerate() {
        pos[v] = (top.0[i].0 + top_shift, top.0[i].1);
    }
    let below = if structure.is_empty() { 0 } else { top.2 + m.gap_y };
    let columns_shift = (width - columns_w) / 2;
    for (i, &v) in functions.iter().enumerate() {
        pos[v] = (left.0[i].0 + columns_shift, left.0[i].1 + below);
    }
    let right_x = columns_shift + if functions.is_empty() { 0 } else { left.1 + between };
    for (i, &v) in types.iter().enumerate() {
        pos[v] = (right.0[i].0 + right_x, right.0[i].1 + below);
    }
    (pos, width, below + left.2.max(right.2))
}

/// How many edges cross each gap between consecutive ranks.
///
/// An edge between two ranks passes through every gap between them, and is
/// counted in each. Counted over the edges as given rather than the ones
/// layering kept: an edge it ranked backwards is still drawn, and is the
/// hardest of the lot to find a way for.
fn crossings(n: usize, ranks: &[Vec<usize>], edges: &[(usize, usize)]) -> Vec<usize> {
    let mut rank_of = vec![usize::MAX; n];
    for (r, rank) in ranks.iter().enumerate() {
        for &v in rank {
            if let Some(slot) = rank_of.get_mut(v) {
                *slot = r;
            }
        }
    }
    let mut over = vec![0usize; ranks.len().saturating_sub(1)];
    for &(a, b) in edges {
        let (ra, rb) = (rank_of.get(a).copied(), rank_of.get(b).copied());
        let (Some(ra), Some(rb)) = (ra, rb) else { continue };
        if ra == usize::MAX || rb == usize::MAX {
            continue;
        }
        for crossed in over.iter_mut().take(ra.max(rb)).skip(ra.min(rb)) {
            *crossed += 1;
        }
    }
    over
}

/// What a gap between ranks opens out to for the lines crossing it.
///
/// A gap is where every edge between two ranks has to run, and they all take
/// the same taut line through it, so a crowded one had every line in the
/// picture stacked on the same few cells. It widens by a cell a line, and by
/// at most its own width again -- a level with fifty edges across one gap
/// cannot be given fifty rows, and past that the lines share as they did.
fn gap_for(crossing: usize, m: &Metrics) -> u16 {
    let extra = crossing.saturating_sub(FREE_TRACKS).min(usize::from(m.gap_y));
    m.gap_y.saturating_add(extra as u16)
}

/// Ranks as rows. A rank wraps when it would exceed `max_w`: layering puts
/// every edge between different ranks, so nodes inside one rank never link
/// to each other and wrapping one cannot cross an edge. Every row is then
/// centred, so a parent sits over its children rather than left of them.
fn pack_down(
    sizes: &[(u16, u16)],
    ranks: &[Vec<usize>],
    edges: &[(usize, usize)],
    max_w: u16,
    m: &Metrics,
) -> (Vec<(u16, u16)>, u16, u16) {
    let over = crossings(sizes.len(), ranks, edges);
    let mut pos = vec![(0u16, 0u16); sizes.len()];
    let mut rows: Vec<(Vec<usize>, u16)> = Vec::new();
    let (mut y, mut widest) = (0u16, 0u16);
    for (r, rank) in ranks.iter().enumerate() {
        let (mut x, mut tallest) = (0u16, 0u16);
        let mut row: Vec<usize> = Vec::new();
        for &v in rank {
            let (w, h) = sizes[v];
            if x > 0 && x.saturating_add(w) > max_w {
                // Two rows, not one: the least a line can be routed through.
                rows.push((std::mem::take(&mut row), x.saturating_sub(m.gap_x)));
                y = y.saturating_add(tallest).saturating_add(WRAP_GAP);
                x = 0;
                tallest = 0;
            }
            pos[v] = (x, y);
            row.push(v);
            x = x.saturating_add(w).saturating_add(m.gap_x);
            tallest = tallest.max(h);
            widest = widest.max(x.saturating_sub(m.gap_x));
        }
        rows.push((row, x.saturating_sub(m.gap_x)));
        y = y.saturating_add(tallest).saturating_add(gap_for(over.get(r).copied().unwrap_or(0), m));
    }
    for (row, width) in rows {
        let shift = (widest - width) / 2;
        for v in row {
            pos[v].0 += shift;
        }
    }
    // The last rank added a gap it has nothing to be apart from, and with
    // nothing after it that gap was never widened.
    (pos, widest, y.saturating_sub(m.gap_y))
}

/// Ranks as columns, each column centred on the tallest.
fn pack_across(
    sizes: &[(u16, u16)],
    ranks: &[Vec<usize>],
    edges: &[(usize, usize)],
    m: &Metrics,
) -> (Vec<(u16, u16)>, u16, u16) {
    let over = crossings(sizes.len(), ranks, edges);
    let mut pos = vec![(0u16, 0u16); sizes.len()];
    let mut columns: Vec<(Vec<usize>, u16)> = Vec::new();
    let (mut x, mut tallest) = (0u16, 0u16);
    for (r, rank) in ranks.iter().enumerate() {
        let (mut y, mut widest) = (0u16, 0u16);
        for &v in rank {
            let (w, h) = sizes[v];
            pos[v] = (x, y);
            y = y.saturating_add(h).saturating_add(WRAP_GAP);
            widest = widest.max(w);
        }
        let height = y.saturating_sub(WRAP_GAP);
        columns.push((rank.clone(), height));
        tallest = tallest.max(height);
        // The between-rank gap is the same distance whichever way ranks
        // run: it is where the edges bend, and a bend needs the same room.
        x = x.saturating_add(widest).saturating_add(gap_for(over.get(r).copied().unwrap_or(0), m));
    }
    for (column, height) in columns {
        let shift = (tallest - height) / 2;
        for v in column {
            pos[v].1 += shift;
        }
    }
    (pos, x.saturating_sub(m.gap_y), tallest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::Picture;
    use entity_graph::EntityGraph;
    use entity_graph::EntityKind::{File, Folder};
    use entity_graph::ReferenceKind::Call;
    use entity_graph::test_support::graph_from_parents;

    /// Expand all the way, which for these fixtures puts every file in view.
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

    /// Settle and materialise in one go, the way a fresh view does.
    fn place(graph: &EntityGraph, picture: &Picture, max_w: u16, zoom: Zoom) -> Diagram {
        let labels = Labels::new(graph);
        let scene = Scene::new(graph, picture);
        let mut layout = Layout::new();
        layout.settle(&scene, &labels, max_w);
        layout.materialize(&scene, &labels, zoom, &Options::default())
    }

    fn rect_of(d: &Diagram, graph: &EntityGraph, name: &str) -> Rect {
        let id = graph.entities.iter().find(|e| e.name == name).expect("no such entity").id;
        d.nodes.iter().find(|n| n.id == id).expect("entity not placed").rect
    }

    fn contains(outer: Rect, inner: Rect) -> bool {
        inner.x > outer.x
            && inner.y > outer.y
            && inner.right() < outer.right()
            && inner.bottom() < outer.bottom()
    }

    fn overlap(a: Rect, b: Rect) -> bool {
        a.intersects(b)
    }

    #[test]
    fn every_file_is_drawn_strictly_inside_the_folder_that_holds_it() {
        let graph = graph_from_parents(
            &[
                ("root", Folder, None),
                ("a.rs", File, Some(0)),
                ("b.rs", File, Some(0)),
                ("c.rs", File, Some(0)),
            ],
            &[],
        );
        let d = place(&graph, &fully_expanded(&graph), 200, Zoom::Close);
        let folder = rect_of(&d, &graph, "root");
        let root = d.nodes.iter().find(|n| n.id.0 == 0).unwrap();
        assert!(root.is_box);
        assert_eq!(root.depth, 1);
        for file in ["a.rs", "b.rs", "c.rs"] {
            assert!(contains(folder, rect_of(&d, &graph, file)), "{file} escaped its folder");
            let leaf = d.nodes.iter().find(|n| n.id == graph.entities.iter().find(|e| e.name == file).unwrap().id).unwrap();
            assert_eq!(leaf.depth, 2);
        }
    }

    /// Is `b` wholly after `a` in reading order -- below it, or to its right?
    fn after(a: Rect, b: Rect) -> bool {
        b.y >= a.bottom() || b.x >= a.right()
    }

    /// A reference ranks its target after its source: below when the box
    /// flows down, to the right when it flows across. Never beside it in
    /// the same rank, and never before it.
    #[test]
    fn a_reference_puts_its_target_after_its_source() {
        let graph = graph_from_parents(
            &[("root", Folder, None), ("caller.rs", File, Some(0)), ("callee.rs", File, Some(0))],
            &[(1, 2, Call)],
        );
        let d = place(&graph, &fully_expanded(&graph), 200, Zoom::Close);
        let caller = rect_of(&d, &graph, "caller.rs");
        let callee = rect_of(&d, &graph, "callee.rs");
        assert!(after(caller, callee), "caller {caller:?} should rank before callee {callee:?}");
        assert!(!after(callee, caller));
        assert_eq!(d.edges.len(), 1);

        // Two nodes with no edge share a rank and sit side by side.
        let loose = graph_from_parents(
            &[("root", Folder, None), ("a.rs", File, Some(0)), ("b.rs", File, Some(0))],
            &[],
        );
        let d = place(&loose, &fully_expanded(&loose), 200, Zoom::Close);
        assert_eq!(rect_of(&d, &loose, "a.rs").y, rect_of(&d, &loose, "b.rs").y);
    }

    /// Ranking assumes a DAG and a code graph is never one, so the interesting
    /// case is that mutual references still terminate and still place both
    /// nodes rather than dropping one.
    #[test]
    fn mutually_referencing_files_are_both_placed() {
        let graph = graph_from_parents(
            &[("root", Folder, None), ("one.rs", File, Some(0)), ("two.rs", File, Some(0))],
            &[(1, 2, Call), (2, 1, Call)],
        );
        let d = place(&graph, &fully_expanded(&graph), 200, Zoom::Close);
        assert!(!overlap(rect_of(&d, &graph, "one.rs"), rect_of(&d, &graph, "two.rs")));
    }

    /// Sibling order is the tie-break in ranking, which drives wrapping and so
    /// every box size. Iterating a `HashMap` to establish it made the whole
    /// layout vary run to run -- and between two calls in one process, which
    /// is what this pins.
    #[test]
    fn the_same_graph_lays_out_identically_every_time() {
        let names: Vec<String> = (0..10).map(|i| format!("mod_{i:02}.rs")).collect();
        let mut rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> =
            vec![("root", Folder, None), ("inner", Folder, Some(0))];
        rows.extend(names.iter().map(|n| (n.as_str(), File, Some(1))));
        let refs: Vec<(usize, usize, entity_graph::ReferenceKind)> =
            (2..11).map(|i| (i, i + 1, Call)).collect();
        let graph = graph_from_parents(&rows, &refs);
        let picture = fully_expanded(&graph);

        let first = place(&graph, &picture, 80, Zoom::Close);
        for _ in 0..5 {
            let again = place(&graph, &picture, 80, Zoom::Close);
            assert_eq!((first.width, first.height), (again.width, again.height));
            let a: Vec<_> = first.nodes.iter().map(|n| (n.id, n.rect)).collect();
            let b: Vec<_> = again.nodes.iter().map(|n| (n.id, n.rect)).collect();
            assert_eq!(a, b, "layout differs between calls");
        }
    }

    #[test]
    fn a_wide_subtree_still_fits_the_viewport() {
        let mut rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> =
            vec![("root", Folder, None)];
        let deep: Vec<String> = (0..6).map(|i| format!("lvl{i}")).collect();
        for (i, name) in deep.iter().enumerate() {
            rows.push((name.as_str(), Folder, Some(i)));
        }
        let files: Vec<String> = (0..8).map(|i| format!("wide_file_name_{i:02}.rs")).collect();
        rows.extend(files.iter().map(|n| (n.as_str(), File, Some(deep.len()))));
        let graph = graph_from_parents(&rows, &[]);
        let d = place(&graph, &fully_expanded(&graph), 100, Zoom::Close);
        assert!(d.width <= 100, "six levels of nesting reached {} columns", d.width);
    }

    /// Minified vendor sources really do carry identifiers tens of thousands
    /// of characters long. Before the clamp, one of them sized a box to 30346
    /// columns and the next zoom overflowed the layout arithmetic outright.
    #[test]
    fn an_absurdly_long_name_does_not_size_the_thing_that_holds_it() {
        let huge = "x".repeat(30_324);
        let graph = graph_from_parents(
            &[
                ("root", Folder, None),
                ("inner", Folder, Some(0)),
                (huge.as_str(), File, Some(1)),
                ("ordinary.rs", File, Some(1)),
            ],
            &[],
        );
        let d = place(&graph, &fully_expanded(&graph), 120, Zoom::Close);
        assert!(d.width <= 120, "one long label widened the diagram to {}", d.width);
    }

    #[test]
    fn a_rank_too_wide_for_the_viewport_wraps_onto_another_row() {
        let names: Vec<String> = (0..12).map(|i| format!("file_{i:02}.rs")).collect();
        let mut rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> =
            vec![("root", Folder, None)];
        rows.extend(names.iter().map(|n| (n.as_str(), File, Some(0))));
        let graph = graph_from_parents(&rows, &[]);

        let narrow = place(&graph, &fully_expanded(&graph), 60, Zoom::Close);
        assert!(narrow.width <= 60, "wrapped layout is {} wide", narrow.width);
        let rows_used: HashSet<u16> =
            narrow.nodes.iter().filter(|n| !n.is_box).map(|n| n.rect.y).collect();
        assert!(rows_used.len() > 1, "a 12-node rank should not fit one 60-column row");

        let wide = place(&graph, &fully_expanded(&graph), 400, Zoom::Close);
        let wide_rows: HashSet<u16> =
            wide.nodes.iter().filter(|n| !n.is_box).map(|n| n.rect.y).collect();
        assert_eq!(wide_rows.len(), 1);
    }

    /// A parent with several children is centred over them, not left of
    /// them; and a chain ranks across a box rather than down it, but the
    /// root level always ranks down.
    #[test]
    fn ranks_are_centred_and_a_deep_chain_runs_across_its_box() {
        let mut rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> =
            vec![("root", Folder, None), ("hub.rs", File, Some(0))];
        let names: Vec<String> = (0..4).map(|i| format!("leaf_{i}.rs")).collect();
        rows.extend(names.iter().map(|n| (n.as_str(), File, Some(0))));
        let refs: Vec<(usize, usize, entity_graph::ReferenceKind)> =
            (2..6).map(|i| (1, i, Call)).collect();
        let fan = graph_from_parents(&rows, &refs);
        let d = place(&fan, &fully_expanded(&fan), 200, Zoom::Close);
        let hub = rect_of(&d, &fan, "hub.rs");
        let (first, last) = (rect_of(&d, &fan, "leaf_0.rs"), rect_of(&d, &fan, "leaf_3.rs"));
        assert!(hub.y < first.y, "the hub ranks above what it calls");
        let hub_mid = i32::from(hub.x) * 2 + i32::from(hub.width);
        let row_mid = i32::from(first.x) + i32::from(last.right());
        assert!((hub_mid - row_mid).abs() <= 2, "the hub is not centred over its row: {hub:?} vs {first:?}..{last:?}");

        // Six functions in a chain inside a file: across, not down.
        let mut rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> =
            vec![("root", Folder, None), ("chain.rs", File, Some(0))];
        let fns: Vec<String> = (0..6).map(|i| format!("step_{i}")).collect();
        rows.extend(fns.iter().map(|n| (n.as_str(), entity_graph::EntityKind::Function, Some(1))));
        let refs: Vec<(usize, usize, entity_graph::ReferenceKind)> =
            (2..7).map(|i| (i, i + 1, Call)).collect();
        let chain = graph_from_parents(&rows, &refs);
        let d = place(&chain, &fully_expanded(&chain), 200, Zoom::Close);
        let (a, b) = (rect_of(&d, &chain, "step_0"), rect_of(&d, &chain, "step_5"));
        assert!(b.x > a.right(), "a chain should run across: {a:?} then {b:?}");
        assert_eq!(a.y, b.y, "a chain across its box is one row");
        let file = rect_of(&d, &chain, "chain.rs");
        assert!(file.width > file.height * 2, "the box should be wide, not tall: {file:?}");

        // Told to flow down, the same chain is a column; and a chain too
        // wide for the room it has flows down whatever its shape would be.
        let sizes = vec![(12u16, 4u16); 6];
        let edges: Vec<(usize, usize)> = (0..5).map(|i| (i, i + 1)).collect();
        let m = Zoom::Close.metrics();
        let (down, w, h) = pack(&sizes, &edges, 200, &m, true);
        assert!(down.iter().all(|p| p.0 == 0) && h > w, "not a column: {down:?}");
        let (across, w, h) = pack(&sizes, &edges, 200, &m, false);
        assert!(across.iter().all(|p| p.1 == 0) && w > h, "not a row: {across:?}");
        let (narrow, ..) = pack(&sizes, &edges, 40, &m, false);
        assert_eq!(narrow, down, "no room across, so down");
    }

    /// A file holding a module, three functions and two types: the module
    /// sits across the top, the functions run down one side by who calls
    /// whom, the types down the other by who refers to whom, and the two
    /// columns start level. A level of one kind is ranked as it always was.
    #[test]
    fn functions_and_types_stand_in_two_columns_under_the_structure() {
        use entity_graph::EntityKind::{Class, Function, Module};
        let graph = graph_from_parents(
            &[
                ("root", Folder, None),
                ("lib.rs", File, Some(0)),
                ("inner", Module, Some(1)),
                ("Parser", Class, Some(1)),
                ("Token", Class, Some(1)),
                ("parse", Function, Some(1)),
                ("lex", Function, Some(1)),
                ("emit", Function, Some(1)),
                ("helper", Function, Some(2)),
            ],
            &[
                (5, 6, Call),
                (5, 7, Call),
                (3, 4, entity_graph::ReferenceKind::TypeRef),
                (5, 3, entity_graph::ReferenceKind::TypeRef),
            ],
        );
        let d = place(&graph, &fully_expanded(&graph), 200, Zoom::Close);
        let r = |name: &str| rect_of(&d, &graph, name);
        let (inner, parser, token) = (r("inner"), r("Parser"), r("Token"));
        let (parse, lex, emit) = (r("parse"), r("lex"), r("emit"));

        for below in [parser, token, parse, lex, emit] {
            assert!(below.y >= inner.bottom(), "{below:?} is not beneath the module {inner:?}");
        }
        assert!(parse.y < lex.y && parse.y < emit.y, "the caller is not above what it calls");
        assert_eq!(lex.y, emit.y, "two callees of one caller share a rank");
        assert!(parser.y < token.y, "the referring type is not above the one it refers to");
        assert_eq!(parse.y, parser.y, "the two columns do not start level");
        let functions_right = parse.right().max(lex.right()).max(emit.right());
        assert!(parser.x >= functions_right && token.x >= functions_right, "the types are not in a column beside the functions");

        // Nothing but functions: ranked as one level, no columns.
        let sizes = vec![(12u16, 4u16); 3];
        let kinds = vec![Function; 3];
        let m = Zoom::Close.metrics();
        assert_eq!(arrange(&sizes, &kinds, &[(0, 1)], 200, &m, true), pack(&sizes, &[(0, 1)], 200, &m, true));
    }

    /// Two folders side by side, each with two files, and one edge between
    /// the folders. Enough to expand one folder while the other stands still.
    fn two_folders() -> EntityGraph {
        graph_from_parents(
            &[
                ("root", Folder, None),
                ("left", Folder, Some(0)),
                ("right", Folder, Some(0)),
                ("l1.rs", File, Some(1)),
                ("l2.rs", File, Some(1)),
                ("r1.rs", File, Some(2)),
                ("r2.rs", File, Some(2)),
            ],
            &[(3, 5, Call), (3, 4, Call)],
        )
    }

    fn picture_at(graph: &EntityGraph, cursor: &coalesce::Cursor) -> Picture {
        crate::view::apply(graph, &cursor.coalesced(), &Default::default())
    }

    /// Expanding a node grows it from its own origin and its siblings make
    /// way; once kept, that is the arrangement. Collapsing it shrinks it in
    /// place and moves nothing else: the user did not touch the siblings,
    /// so they stay where they are, gap and all.
    #[test]
    fn expanding_pushes_siblings_once_and_collapsing_moves_nothing() {
        let graph = two_folders();
        let labels = Labels::new(&graph);
        let (root, left, right) = (EntityId(0), EntityId(1), EntityId(2));
        let mut cursor = coalesce::Cursor::new(&graph);
        cursor.move_down(root, &graph);

        let mut layout = Layout::new();
        let before = {
            let scene = Scene::new(&graph, &picture_at(&graph, &cursor));
            layout.settle(&scene, &labels, 200);
            let d = layout.materialize(&scene, &labels, Zoom::Close, &Options::default());
            layout.commit(&d, &scene);
            d
        };
        let left_was = before.rect_of(left).unwrap();
        let right_was = before.rect_of(right).unwrap();
        assert!(after(left_was, right_was), "fixture: the edge ranks right after left");

        cursor.move_down(left, &graph);
        let open = {
            let scene = Scene::new(&graph, &picture_at(&graph, &cursor));
            layout.settle(&scene, &labels, 200);
            let opts = Options { anchored: [left].into_iter().collect(), loose: Default::default() };
            let d = layout.materialize(&scene, &labels, Zoom::Close, &opts);
            layout.commit(&d, &scene);
            d
        };
        let left_now = open.rect_of(left).unwrap();
        let right_now = open.rect_of(right).unwrap();
        assert!(open.nodes.iter().any(|n| n.id == left && n.is_box), "left should be a box now");
        assert!(left_now.width > left_was.width && left_now.height > left_was.height);
        assert_eq!((left_now.x, left_now.y), (left_was.x, left_was.y), "the opened node moved");
        assert!(!overlap(left_now, right_now), "the grown box overlaps its sibling");
        assert!(after(left_now, right_now), "the siblings changed order");
        assert_eq!(right_now.y, right_was.y, "a sibling beside the box was pushed out of its row");
        for file in ["l1.rs", "l2.rs"] {
            assert!(contains(left_now, rect_of(&open, &graph, file)), "{file} is outside its box");
        }

        cursor.move_up(EntityId(3), &graph);
        let closed = {
            let scene = Scene::new(&graph, &picture_at(&graph, &cursor));
            layout.settle(&scene, &labels, 200);
            let d = layout.materialize(&scene, &labels, Zoom::Close, &Options::default());
            layout.commit(&d, &scene);
            d
        };
        let left_again = closed.rect_of(left).unwrap();
        assert_eq!(left_again, left_was, "the collapsed node is not back to what it was");
        assert_eq!(closed.rect_of(right).unwrap(), right_now, "collapsing moved a sibling");
    }

    /// The push keeps order and honours anchors: the one under the pointer
    /// stays put and the other makes way, by the gap and no less.
    #[test]
    fn overlapping_siblings_are_pushed_apart_and_an_anchored_one_stays() {
        let gap = (2, 2);
        let mut geos = vec![
            Geo { x: 0, y: 0, w: 10, h: 4 },
            Geo { x: 4, y: 1, w: 10, h: 4 },
        ];
        separate(&mut geos, &[false, false], &[0, 1], gap);
        assert!(geos[0].x + geos[0].w + gap.0 <= geos[1].x, "still overlapping: {geos:?}");
        assert!(geos[0].x < geos[1].x, "order flipped");
        assert_eq!(geos[0].y, 0, "a sideways overlap was pushed vertically");
        assert!(geos[0].x < 0 && geos[1].x > 4, "both should have moved");

        let mut anchored = vec![
            Geo { x: 0, y: 0, w: 10, h: 4 },
            Geo { x: 4, y: 1, w: 10, h: 4 },
        ];
        separate(&mut anchored, &[true, false], &[0, 1], gap);
        assert_eq!(anchored[0], Geo { x: 0, y: 0, w: 10, h: 4 }, "the anchored node moved");
        assert_eq!(anchored[1].x, 12, "the other should move the whole overlap plus gap");

        // Stacked rather than beside: the push goes down, not sideways.
        let mut stacked = vec![
            Geo { x: 0, y: 0, w: 10, h: 4 },
            Geo { x: 1, y: 3, w: 10, h: 4 },
        ];
        separate(&mut stacked, &[false, false], &[0, 1], gap);
        assert_eq!((stacked[0].x, stacked[1].x), (0, 1), "a vertical overlap moved sideways");
        assert!(stacked[0].y + stacked[0].h + gap.1 <= stacked[1].y);

        // A leaf a cell lower than a box that has just grown tall and wide
        // over it is still *beside* the box, and goes right -- the smallest
        // move would be up, out of its row.
        let mut grown = vec![
            Geo { x: 0, y: 0, w: 60, h: 20 },
            Geo { x: 50, y: 1, w: 12, h: 3 },
        ];
        separate(&mut grown, &[true, false], &[0, 1], gap);
        assert_eq!(grown[1], Geo { x: 62, y: 1, w: 12, h: 3 }, "pushed out of its row: {grown:?}");
    }

    /// The gap between two ranks is where every edge between them has to
    /// run, and they all take much the same taut line through it -- so a
    /// crowded gap had the whole picture's lines stacked on a few cells. It
    /// now opens out for its traffic, and only for its traffic: the same
    /// nodes with one edge between them stay as close as they ever were.
    #[test]
    fn a_gap_opens_out_for_the_edges_that_have_to_cross_it() {
        let m = Zoom::Close.metrics();
        let sizes = vec![(10u16, 4u16); 6];
        let ranks = vec![vec![0, 1, 2], vec![3, 4, 5]];
        let depth = |edges: &[(usize, usize)]| pack_down(&sizes, &ranks, edges, 200, &m).2;

        let quiet = depth(&[(0, 3)]);
        let busy = depth(&[(0, 3), (0, 4), (0, 5), (1, 3), (1, 4), (1, 5), (2, 3)]);
        assert!(busy > quiet, "a crowded gap got no more room than an empty one: {busy} vs {quiet}");

        // A gap opens by at most its own width again, however heavy it gets.
        let swamped: Vec<(usize, usize)> =
            (0..3).flat_map(|a| (3..6).map(move |b| (a, b))).cycle().take(200).collect();
        assert!(depth(&swamped) <= quiet + m.gap_y, "one gap swallowed the level");

        // Edges inside a rank cross no gap and ask for nothing.
        assert_eq!(depth(&[(0, 1), (1, 2), (3, 4)]), depth(&[]), "a same-rank edge widened a gap");
    }

    /// A file added to a folder that is already arranged. It used to be laid
    /// in a row under everything, so a new file sat at the foot of its folder
    /// however obviously it belonged higher up. Now the ranking says who it
    /// belongs between and the ranks those two already carry say how to put
    /// it there -- so it arrives among its siblings, and neither of them is
    /// renumbered to make room.
    #[test]
    fn a_file_that_turns_up_later_is_inserted_among_its_siblings() {
        let rows: Vec<(&str, entity_graph::EntityKind, Option<usize>)> = vec![
            ("root", Folder, None),
            ("a.rs", File, Some(0)),
            ("b.rs", File, Some(0)),
            ("c.rs", File, Some(0)),
        ];
        // b.rs calls c.rs, so the ranking has an opinion about the order.
        let graph = graph_from_parents(&rows, &[(2, 3, Call)]);
        let labels = Labels::new(&graph);
        let (a, b, c) = (EntityId(1), EntityId(2), EntityId(3));

        // Settle with only two of them in the picture, as though c.rs had
        // not been written yet.
        let mut without = fully_expanded(&graph);
        without.nodes.retain(|&n| n != c);
        without.edges.retain(|e| e.from != c && e.to != c);
        let mut layout = Layout::new();
        let first = Scene::new(&graph, &without);
        layout.settle(&first, &labels, 120);
        let (was_a, was_b) = (layout.rank_of(a).cloned(), layout.rank_of(b).cloned());
        assert!(was_a.is_some() && was_b.is_some(), "the first two should be ranked");
        assert_eq!(layout.rank_of(c), None, "c.rs is not in the picture yet");

        // c.rs appears. Only it is fresh.
        let whole = Scene::new(&graph, &fully_expanded(&graph));
        layout.settle(&whole, &labels, 120);
        assert_eq!(layout.rank_of(a).cloned(), was_a, "a.rs was renumbered to make room");
        assert_eq!(layout.rank_of(b).cloned(), was_b, "b.rs was renumbered to make room");
        assert!(layout.rank_of(c).is_some(), "c.rs was not ranked when it arrived");

        // It is drawn among them rather than in a row beneath the lot, and
        // the order remembered is the order drawn.
        let drawn = layout.materialize(&whole, &labels, Zoom::Close, &Options::default());
        let rect = |id| drawn.rect_of(id).expect("all three are drawn");
        let (ra, rb, rc) = (rect(a), rect(b), rect(c));
        assert!(rc.y < ra.bottom().max(rb.bottom()), "c.rs landed under everything: {rc:?}");

        let mut by_rank = [a, b, c];
        by_rank.sort_by_key(|&id| layout.rank_of(id).cloned());
        let mut by_place = [a, b, c];
        by_place.sort_by_key(|&id| (rect(id).y, rect(id).x));
        assert_eq!(by_rank, by_place, "the order remembered is not the order drawn");
    }

    /// Two nodes at the same place: there is no "differ on more" between two
    /// zeroes, so nothing about where they are says which should go first.
    /// The order the level remembers says it, and says the opposite when it
    /// is the opposite -- the fallback was the order the graph listed them
    /// in, which the reader neither chose nor can change.
    #[test]
    fn nodes_on_the_same_spot_come_apart_in_the_order_the_level_remembers() {
        let gap = (2, 2);
        let stacked = || vec![Geo { x: 8, y: 0, w: 10, h: 4 }, Geo { x: 8, y: 0, w: 10, h: 4 }];

        let mut in_order = stacked();
        separate(&mut in_order, &[false, false], &[0, 1], gap);
        assert!(in_order[0].x < in_order[1].x, "the first by order did not go first: {in_order:?}");

        let mut reversed = stacked();
        separate(&mut reversed, &[false, false], &[1, 0], gap);
        assert!(reversed[0].x > reversed[1].x, "the order was not what decided: {reversed:?}");

        // Whichever way round, they end up clear of each other by the gap.
        for pair in [in_order, reversed] {
            let (l, r) = if pair[0].x < pair[1].x { (pair[0], pair[1]) } else { (pair[1], pair[0]) };
            assert!(l.x + l.w + gap.0 <= r.x, "still overlapping: {pair:?}");
        }
    }

    /// A drag: the node moves by what the pointer moved, at the zoom being
    /// looked at; a drop keeps it there at every zoom; and while it is in
    /// flight nothing else at its level is pushed. Positions are read
    /// relative to a sibling, because the canvas is normalised to its own
    /// top-left and the box hugs its children -- the *screen* holds still
    /// through the camera, which is not this module's concern.
    #[test]
    fn a_nudge_moves_by_screen_cells_and_a_commit_keeps_it_there() {
        let graph = two_folders();
        let labels = Labels::new(&graph);
        let mut cursor = coalesce::Cursor::new(&graph);
        cursor.move_down(EntityId(0), &graph);
        let scene = Scene::new(&graph, &picture_at(&graph, &cursor));
        let (left, right) = (EntityId(1), EntityId(2));
        let mut layout = Layout::new();
        layout.settle(&scene, &labels, 200);
        let rest = layout.materialize(&scene, &labels, Zoom::Mid, &Options::default());
        let (left_was, right_was) = (rest.rect_of(left).unwrap(), rest.rect_of(right).unwrap());

        // Drag `left` down, well clear of anything, with its level loose.
        layout.nudge(&[left], (0, 12), Zoom::Mid);
        let opts = Options {
            anchored: [left].into_iter().collect(),
            loose: [Some(EntityId(0))].into_iter().collect(),
        };
        let flight = layout.materialize(&scene, &labels, Zoom::Mid, &opts);
        let apart = |d: &Diagram| {
            i32::from(d.rect_of(left).unwrap().y) - i32::from(d.rect_of(right).unwrap().y)
        };
        let was_apart = i32::from(left_was.y) - i32::from(right_was.y);
        assert_eq!(apart(&flight), was_apart + 12, "moved by other than the drag");
        assert_eq!(flight.rect_of(right).unwrap().width, right_was.width);

        // Drop: commit the levels that were loose, then everything is stable.
        let dropped = layout.materialize(&scene, &labels, Zoom::Mid, &Options { anchored: opts.anchored.clone(), loose: Default::default() });
        layout.commit(&dropped, &scene);
        let again = layout.materialize(&scene, &labels, Zoom::Mid, &Options::default());
        let a: Vec<_> = dropped.nodes.iter().map(|n| (n.id, n.rect)).collect();
        let b: Vec<_> = again.nodes.iter().map(|n| (n.id, n.rect)).collect();
        assert_eq!(a, b, "a committed drop should be a fixed point");

        // The same arrangement, drawn larger: the distance scales with it.
        let close = layout.materialize(&scene, &labels, Zoom::Close, &Options::default());
        let dy_close = i32::from(close.rect_of(left).unwrap().y) - i32::from(close.rect_of(right).unwrap().y);
        let dy_mid = i32::from(again.rect_of(left).unwrap().y) - i32::from(again.rect_of(right).unwrap().y);
        assert!(dy_close > dy_mid, "zooming in did not scale the drag: {dy_close} vs {dy_mid}");
    }

    /// A box hugs its children. Drag one down past the other and the box
    /// grows by exactly that; nothing snaps back, and the other child does
    /// not move relative to the box's top.
    #[test]
    fn a_box_hugs_its_children_when_one_is_dragged() {
        let graph = two_folders();
        let labels = Labels::new(&graph);
        let picture = fully_expanded(&graph);
        let scene = Scene::new(&graph, &picture);
        let mut layout = Layout::new();
        layout.settle(&scene, &labels, 200);
        let rest = layout.materialize(&scene, &labels, Zoom::Close, &Options::default());
        let (l1, l2, left) = (EntityId(3), EntityId(4), EntityId(1));
        let l1_was = rest.rect_of(l1).unwrap();
        let l2_was = rest.rect_of(l2).unwrap();
        let left_was = rest.rect_of(left).unwrap();
        assert_eq!(l1_was.y, l2_was.y, "fixture: two functions in a chain flow across, one row");

        layout.nudge(&[l1], (0, 7), Zoom::Close);
        let opts = Options {
            anchored: [l1].into_iter().collect(),
            loose: [Some(left), Some(EntityId(0))].into_iter().collect(),
        };
        let flight = layout.materialize(&scene, &labels, Zoom::Close, &opts);
        let l1_now = flight.rect_of(l1).unwrap();
        let l2_now = flight.rect_of(l2).unwrap();
        let left_now = flight.rect_of(left).unwrap();
        let gap = |a: Rect, b: Rect| i32::from(a.y) - i32::from(b.y);
        assert_eq!(gap(l1_now, l2_now), gap(l1_was, l2_was) + 7, "the dragged leaf did not move by the drag");
        assert!(contains(left_now, l1_now) && contains(left_now, l2_now), "the box let a child out");
        assert_eq!(left_now.height, left_was.height + 7, "the box did not hug its children");
        assert_eq!(gap(l2_now, left_now), gap(l2_was, left_was), "the undragged child moved relative to the box");
        assert!(contains(flight.rect_of(EntityId(0)).unwrap(), left_now));
    }

    #[test]
    fn positions_survive_a_renumbering_and_a_clear_forgets_them() {
        let mut layout = Layout::new();
        layout.pos.insert(EntityId(2), (3.0, 4.0));
        layout.pos.insert(EntityId(5), (7.0, 8.0));
        layout.migrate(&[None, None, Some(EntityId(9)), None, None, None]);
        assert!(!layout.is_placed(EntityId(2)) && !layout.is_placed(EntityId(5)));
        assert_eq!(layout.pos.get(&EntityId(9)), Some(&(3.0, 4.0)));
        layout.clear();
        assert!(!layout.is_placed(EntityId(9)));
    }

    /// The dragging handle of a box is its title rows and nothing else; a
    /// nested box's title wins over the box around it.
    #[test]
    fn a_box_is_grabbed_by_its_title_rows_and_the_innermost_wins() {
        let graph = two_folders();
        let d = place(&graph, &fully_expanded(&graph), 200, Zoom::Close);
        let (root, left) = (EntityId(0), EntityId(1));
        let left_rect = d.rect_of(left).unwrap();
        assert_eq!(d.handle_at((left_rect.x + 1, left_rect.y)), Some(left));
        assert_eq!(d.handle_at((left_rect.x + 1, left_rect.y + 1)), Some(left));
        assert_eq!(d.handle_at((1, 0)), Some(root));
        let l1 = rect_of(&d, &graph, "l1.rs");
        assert_eq!(d.handle_at((l1.x + 1, l1.y + 1)), None, "a leaf is not a box handle");
        assert_eq!(d.leaf_at((l1.x + 1, l1.y + 1)), Some(EntityId(3)));
        assert_eq!(d.leaves_in(Rect::new(l1.x, l1.y, 1, 1)), vec![EntityId(3)]);
    }
}
