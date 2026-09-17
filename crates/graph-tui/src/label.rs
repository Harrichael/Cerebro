//! What a node reads as.
//!
//! Both the placer and the renderer need this: one to size a box, the other to
//! fill it, and a box sized from one string and filled with another truncates.
//! The browser learned that the hard way and keeps its width estimate next to
//! its label text for the same reason (`ui/common.js`, `leafSubText`).
//!
//! Sizes are computed once per graph, not per node: a folder's total is the
//! sum over its subtree, and re-walking that for every box on screen would be
//! quadratic in the depth of the tree.

use entity_graph::{EntityGraph, EntityId, EntityKind};

pub struct Labels<'a> {
    pub graph: &'a EntityGraph,
    /// `(loc, test_loc)` per entity, indexed by id; see
    /// [`EntityGraph::loc_per_entity`].
    loc: Vec<(usize, usize)>,
}

impl<'a> Labels<'a> {
    /// One walk of the tree. Cheap next to laying the diagram out, and the
    /// caller is free to keep the result around rather than rebuild it.
    pub fn new(graph: &'a EntityGraph) -> Self {
        Labels { loc: graph.loc_per_entity(|_| false), graph }
    }

    pub fn name(&self, id: EntityId) -> &'a str {
        self.graph.get(id).map_or("?", |e| e.name.as_str())
    }

    pub fn kind(&self, id: EntityId) -> Option<EntityKind> {
        self.graph.get(id).map(|e| e.kind)
    }

    /// How big the thing is: a line count for what holds code in bulk, and
    /// the line range for what does not. A function's range already says its
    /// size, so counting it again would only take up room.
    pub fn size(&self, id: EntityId) -> String {
        let Some(e) = self.graph.get(id) else { return String::new() };
        let loc = self.loc.get(id.0).map_or(0, |l| l.0);
        match e.kind {
            EntityKind::Folder | EntityKind::File if loc > 0 => format!("{} loc", thousands(loc)),
            EntityKind::Folder => String::new(),
            _ if e.line_range != (0..0) => {
                format!("{}–{}", e.line_range.start + 1, e.line_range.end + 1)
            }
            _ => String::new(),
        }
    }

    /// The line under a leaf's name: what kind of thing it is, how big, and
    /// whether it is test code. Without it every leaf is a bare word and the
    /// view cannot say whether `parse` is a file, a class or a function.
    pub fn detail(&self, id: EntityId) -> String {
        let Some(e) = self.graph.get(id) else { return String::new() };
        [kind_word(e.kind).to_string(), self.size(id), if e.is_test { "test".into() } else { String::new() }]
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// A box has one row to spend, so it carries its size beside its name
    /// rather than under it.
    pub fn box_label(&self, id: EntityId) -> String {
        match self.size(id).as_str() {
            "" => self.name(id).to_string(),
            size => format!("{} · {size}", self.name(id)),
        }
    }
}

pub fn kind_word(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Folder => "folder",
        EntityKind::Module => "module",
        EntityKind::File => "file",
        EntityKind::Class => "class",
        EntityKind::Function => "function",
    }
}

fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use entity_graph::EntityKind::{File, Folder, Function};
    use entity_graph::test_support::graph_from_parents;

    /// The reason this exists: a bare name cannot say whether `parse` is a
    /// file or a function, and the browser view always said which.
    #[test]
    fn a_leaf_says_what_kind_of_thing_it_is_and_how_big() {
        let mut graph = graph_from_parents(
            &[
                ("src", Folder, None),
                ("lib.rs", File, Some(0)),
                ("parse", Function, Some(1)),
                ("it.rs", File, Some(0)),
            ],
            &[],
        );
        graph.entities[1].line_range = 0..1999;
        graph.entities[2].line_range = 9..29;
        graph.entities[3].line_range = 0..9;
        graph.entities[3].is_test = true;
        let l = Labels::new(&graph);

        assert_eq!(l.detail(EntityId(1)), "file · 2,000 loc");
        assert_eq!(l.detail(EntityId(2)), "function · 10–30", "a range says a function's size");
        assert_eq!(l.detail(EntityId(3)), "file · 10 loc · test");
        // A folder has no range of its own; its size is the sum beneath it.
        assert_eq!(l.box_label(EntityId(0)), "src · 2,010 loc");
    }

    #[test]
    fn a_folder_with_nothing_measurable_under_it_just_gives_its_name() {
        let graph = graph_from_parents(&[("empty", Folder, None)], &[]);
        let l = Labels::new(&graph);
        assert_eq!(l.box_label(EntityId(0)), "empty");
        assert_eq!(l.detail(EntityId(0)), "folder");
    }
}
