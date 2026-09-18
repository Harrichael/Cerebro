use std::path::{Path, PathBuf};

/// Unique identifier for an entity within the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntityId(pub usize);

/// Unique identifier for a reference within the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReferenceId(pub usize);

/// Kind of code construct that an entity represents.
/// These form a hierarchy from coarsest (Folder) to finest (Function).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityKind {
    Folder,
    Module,
    File,
    Class,
    Function,
}

impl std::fmt::Display for EntityKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            EntityKind::Folder => "folder",
            EntityKind::Module => "module",
            EntityKind::File => "file",
            EntityKind::Class => "class/struct",
            EntityKind::Function => "fn/method",
        };
        write!(f, "{s}")
    }
}

/// Kind of symbolic relationship between two entities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReferenceKind {
    /// A function or method call.
    Call,
    /// An import or `use` statement.
    Import,
    /// A type annotation or type usage.
    TypeRef,
    /// A variable read or write.
    VarRef,
    /// Any other symbolic reference.
    Generic,
}

impl std::fmt::Display for ReferenceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ReferenceKind::Call => "call",
            ReferenceKind::Import => "import",
            ReferenceKind::TypeRef => "type_ref",
            ReferenceKind::VarRef => "var_ref",
            ReferenceKind::Generic => "generic",
        };
        write!(f, "{s}")
    }
}

/// A directed symbolic reference edge from one entity to another.
///
/// Edges are unique on `(from, to, kind)`; every textual occurrence that
/// contributed to the edge is kept in `sites`, sorted and deduplicated.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Reference {
    pub from: EntityId,
    pub to: EntityId,
    pub kind: ReferenceKind,
    pub sites: Vec<Site>,
}

/// One occurrence of a reference: a 0-indexed line in the source file of the
/// reference's `from` entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Site {
    pub line: usize,
}

/// A single entity (code construct) in the graph.
#[derive(Debug, Clone)]
pub struct Entity {
    pub id: EntityId,
    pub kind: EntityKind,
    pub name: String,

    // CONTAINMENT RELATIONS
    pub parent: Option<EntityId>,
    pub children: Vec<EntityId>,

    // SOURCE LOCATION
    pub path: PathBuf,
    // !! Ranges are 0, 0 if Not Applicable for EntityKind !!
    pub byte_range: std::ops::Range<usize>,
    pub line_range: std::ops::Range<usize>,

    /// Test code, by the conventions in [`crate::test_code`]. Inherited: every
    /// descendant of a test entity is a test entity.
    pub is_test: bool,
}

/// Graph of code entities and their symbolic references.
///
/// Stores two independent structures:
///
/// 1. **Contains topology** — the parent/child relationships captured in each
///    [`Entity`], forming the syntactic hierarchy (Folder → Module → File →
///    Class → Function).
///
/// 2. **Reference graph** — directed edges representing symbolic relationships
///    (calls, imports, type refs, etc.) between entities, independent of
///    containment.
pub struct EntityGraph {
    /// Arena-allocated entities; index == entity.id.0
    pub entities: Vec<Entity>,
    /// Directed symbolic reference edges.
    pub references: Vec<Reference>,
}

impl EntityGraph {
    /// Get an entity by ID.
    pub fn get(&self, id: EntityId) -> Option<&Entity> {
        self.entities.get(id.0)
    }

    /// Filesystem path of the file containing `id`, relative to the loaded
    /// project root. Relies on the producer contract: a File's hierarchy path
    /// is the root name followed by its relative path, so dropping the first
    /// component yields the relative path. An empty path means the loaded root
    /// is itself the file (single-file load). `None` when `id` is unknown or
    /// sits above every File (a Folder).
    pub fn file_path(&self, id: EntityId) -> Option<PathBuf> {
        Some(self.entities[self.file_of(id)?.0].path.components().skip(1).collect())
    }

    /// The File containing `id` (or `id` itself); `None` above every File.
    pub fn file_of(&self, id: EntityId) -> Option<EntityId> {
        let mut cur = self.get(id)?;
        while cur.kind != EntityKind::File {
            cur = self.get(cur.parent?)?;
        }
        Some(cur.id)
    }

    /// The File entity whose source is `rel`, a path relative to the project
    /// root -- the same shape [`Self::file_path`] hands back.
    pub fn file_at_path(&self, rel: &Path) -> Option<EntityId> {
        self.entities
            .iter()
            .filter(|e| e.kind == EntityKind::File)
            .find(|e| self.file_path(e.id).as_deref() == Some(rel))
            .map(|e| e.id)
    }

    /// The smallest entity inside `file` holding `line`, or the file itself
    /// when nothing smaller does. Lines are 0-indexed, and `line_range` is
    /// inclusive at both ends -- a one-line function is `n..n`, which is why
    /// `0..0` is how "no range at all" is spelled and is skipped here.
    pub fn innermost_at(&self, file: EntityId, line: usize) -> Option<EntityId> {
        let file = self.get(file).filter(|e| e.kind == EntityKind::File)?;
        let mut best = (file.id, usize::MAX);
        let mut stack: Vec<EntityId> = file.children.clone();
        while let Some(id) = stack.pop() {
            let Some(e) = self.get(id) else { continue };
            stack.extend(e.children.iter().copied());
            if e.line_range == (0..0) || line < e.line_range.start || line > e.line_range.end {
                continue;
            }
            let span = e.line_range.end - e.line_range.start;
            if span < best.1 {
                best = (id, span);
            }
        }
        Some(best.0)
    }

    /// `(loc, test_loc)` per entity, indexed by [`EntityId`]. `loc` is the
    /// inclusive line range when the entity has one, otherwise (folders) the
    /// sum over its children; `test_loc` is how much of that is test code, so
    /// a view hiding tests can subtract it. Post-order over the forest, so
    /// every child is settled before its parent is read.
    ///
    /// `skip` leaves a child out of its parent's sum while letting the child
    /// keep its own total. A diff view needs exactly that: the graph is the
    /// union of both sides, but a folder's size is what the working tree holds
    /// now, while a deleted file still has the size it had. Pass `|_| false`
    /// when there is no such distinction to make.
    pub fn loc_per_entity(&self, skip: impl Fn(EntityId) -> bool) -> Vec<(usize, usize)> {
        let mut loc = vec![(0, 0); self.entities.len()];
        let mut stack: Vec<(EntityId, bool)> =
            self.entities.iter().filter(|e| e.parent.is_none()).map(|e| (e.id, false)).collect();
        while let Some((id, children_done)) = stack.pop() {
            let e = &self.entities[id.0];
            if !children_done {
                stack.push((id, true));
                stack.extend(e.children.iter().map(|&c| (c, false)));
                continue;
            }
            let current = || e.children.iter().filter(|&&c| skip(id) || !skip(c)).map(|c| loc[c.0]);
            let own = if e.line_range != (0..0) {
                e.line_range.end - e.line_range.start + 1
            } else {
                current().map(|l| l.0).sum()
            };
            let test = if e.is_test { own } else { current().map(|l| l.1).sum() };
            loc[id.0] = (own, test);
        }
        loc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::graph_from_parents;

    /// Ranges are inclusive at both ends, so a line at the very last line of
    /// a function is still inside it; and the *smallest* thing holding a line
    /// is the answer, since a function sits inside a class sits inside a file.
    #[test]
    fn the_smallest_thing_holding_a_line_is_what_is_at_it() {
        let rows = [
            ("proj", EntityKind::Folder, None),
            ("lib.rs", EntityKind::File, Some(0)),
            ("Widget", EntityKind::Class, Some(1)),
            ("draw", EntityKind::Function, Some(2)),
            ("helper", EntityKind::Function, Some(1)),
        ];
        let mut graph = graph_from_parents(&rows, &[]);
        let ranges = [(1, 0..40), (2, 5..20), (3, 8..12), (4, 30..35)];
        for (id, range) in ranges {
            graph.entities[id].line_range = range;
        }
        let file = EntityId(1);
        let at = |line| graph.innermost_at(file, line).expect("the file is a file");

        assert_eq!(at(9), EntityId(3), "inside the method");
        assert_eq!(at(12), EntityId(3), "its last line is still inside it");
        assert_eq!(at(13), EntityId(2), "past it, but still inside the class");
        assert_eq!(at(30), EntityId(4));
        assert_eq!(at(0), file, "nothing smaller holds it, so the file does");
        assert_eq!(graph.innermost_at(EntityId(0), 9), None, "a folder is not a file");
    }

    /// Paths come back relative to the project root, and go back in the same
    /// shape -- a pane that opened a file has to be able to say which entity
    /// it was.
    #[test]
    fn a_file_is_found_by_the_path_it_reports() {
        let rows = [
            ("proj", EntityKind::Folder, None),
            ("src", EntityKind::Folder, Some(0)),
            ("main.rs", EntityKind::File, Some(1)),
        ];
        let graph = graph_from_parents(&rows, &[]);
        let rel = graph.file_path(EntityId(2)).expect("a file has a path");
        assert_eq!(graph.file_at_path(&rel), Some(EntityId(2)));
        assert_eq!(graph.file_at_path(Path::new("nowhere.rs")), None);
    }
}
