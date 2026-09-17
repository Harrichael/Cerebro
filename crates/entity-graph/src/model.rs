use std::path::PathBuf;

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
