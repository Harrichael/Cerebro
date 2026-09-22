//! Fixture helpers shared by every crate that tests against an
//! [`EntityGraph`], so there is one way to hand-build a graph in tests.

use std::path::PathBuf;

use crate::{Entity, EntityGraph, EntityId, EntityKind, Reference, ReferenceKind};

/// A bare entity with no children and a synthetic `<name>.rs` path; the
/// containment is wired up by `graph_from_parents`, which is what callers use.
fn make_entity(id: usize, name: &str, kind: EntityKind, parent: Option<EntityId>) -> Entity {
    Entity {
        id: EntityId(id),
        kind,
        noun: None,
        name: name.to_string(),
        parent,
        children: Vec::new(),
        path: PathBuf::from(format!("{name}.rs")),
        byte_range: 0..0,
        line_range: 0..0,
        is_test: false,
    }
}

/// Build a graph from `(name, kind, parent)` rows, where the row index is the
/// entity id, plus `(from, to, kind)` references. Children lists are derived
/// from the parents so the containment topology is consistent by construction.
pub fn graph_from_parents(
    rows: &[(&str, EntityKind, Option<usize>)],
    references: &[(usize, usize, ReferenceKind)],
) -> EntityGraph {
    let mut entities: Vec<Entity> = rows
        .iter()
        .enumerate()
        .map(|(id, (name, kind, parent))| make_entity(id, name, *kind, parent.map(EntityId)))
        .collect();
    for id in 0..entities.len() {
        if let Some(parent) = entities[id].parent {
            entities[parent.0].children.push(EntityId(id));
        }
    }
    let references = references
        .iter()
        .map(|&(from, to, kind)| Reference { from: EntityId(from), to: EntityId(to), kind, sites: Vec::new() })
        .collect();
    EntityGraph { entities, references }
}
