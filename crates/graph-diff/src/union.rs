use std::collections::HashMap;

use entity_graph::{Entity, EntityGraph, EntityId, Reference};

use crate::Status;
use crate::matching::Matching;

/// Where a union entity came from. At least one side is always present; both
/// means the entity was matched.
#[derive(Clone, Copy)]
pub(crate) struct Origin {
    pub old: Option<EntityId>,
    pub new: Option<EntityId>,
}

pub(crate) struct Union {
    pub graph: EntityGraph,
    pub origin: Vec<Origin>,
    pub reference_status: Vec<Status>,
}

/// The union tree: new's entities in new's order, and under each matched
/// parent the old children that have no counterpart, appended after the new
/// ones and carried over with their whole subtree.
pub(crate) fn build(old: &EntityGraph, new: &EntityGraph, m: &Matching) -> Union {
    let mut b = Builder {
        old,
        new,
        m,
        entities: Vec::new(),
        origin: Vec::new(),
        of_new: HashMap::new(),
        of_old: HashMap::new(),
    };
    b.emit_new(m.new_root, None);
    let Builder { entities, origin, of_new, of_old, .. } = b;

    let mut graph = EntityGraph { entities, references: Vec::new() };
    // Each side's flags are self-consistent, but a removed old child under a
    // parent that became a test on the new side would not be; ids are
    // parent-before-child, so one forward pass restores inheritance.
    for i in 0..graph.entities.len() {
        if let Some(p) = graph.entities[i].parent {
            graph.entities[i].is_test |= graph.entities[p.0].is_test;
        }
    }
    let reference_status = merge_references(&mut graph, old, new, &of_old, &of_new);
    Union { graph, origin, reference_status }
}

struct Builder<'a> {
    old: &'a EntityGraph,
    new: &'a EntityGraph,
    m: &'a Matching,
    entities: Vec<Entity>,
    origin: Vec<Origin>,
    of_new: HashMap<EntityId, EntityId>,
    /// Matched old entities map to their new counterpart's union id, so a
    /// reference is remapped without caring which side it came from.
    of_old: HashMap<EntityId, EntityId>,
}

impl Builder<'_> {
    fn push(&mut self, source: &Entity, parent: Option<EntityId>, origin: Origin) -> EntityId {
        let id = EntityId(self.entities.len());
        self.entities.push(Entity {
            id,
            kind: source.kind,
            noun: source.noun,
            name: source.name.clone(),
            parent,
            children: Vec::new(),
            path: source.path.clone(),
            byte_range: source.byte_range.clone(),
            line_range: source.line_range.clone(),
            is_test: source.is_test,
        });
        self.origin.push(origin);
        id
    }

    fn emit_new(&mut self, new_id: EntityId, parent: Option<EntityId>) -> EntityId {
        let matched = self.m.new_to_old.get(&new_id).copied();
        let id = self.push(
            &self.new.entities[new_id.0],
            parent,
            Origin { old: matched, new: Some(new_id) },
        );
        self.of_new.insert(new_id, id);
        if let Some(old_id) = matched {
            self.of_old.insert(old_id, id);
        }

        let mut children = Vec::new();
        for child in self.new.entities[new_id.0].children.clone() {
            children.push(self.emit_new(child, Some(id)));
        }
        if let Some(old_id) = matched {
            for child in self.old.entities[old_id.0].children.clone() {
                if !self.m.old_to_new.contains_key(&child) {
                    children.push(self.emit_old(child, Some(id)));
                }
            }
        }
        self.entities[id.0].children = children;
        id
    }

    fn emit_old(&mut self, old_id: EntityId, parent: Option<EntityId>) -> EntityId {
        let id = self.push(
            &self.old.entities[old_id.0],
            parent,
            Origin { old: Some(old_id), new: None },
        );
        self.of_old.insert(old_id, id);
        let mut children = Vec::new();
        for child in self.old.entities[old_id.0].children.clone() {
            children.push(self.emit_old(child, Some(id)));
        }
        self.entities[id.0].children = children;
        id
    }
}

/// References keyed by `(from, to, kind)` in union ids: new's first, then the
/// old-only ones. Sites come from the side that has the reference, and from
/// new when both do.
fn merge_references(
    graph: &mut EntityGraph,
    old: &EntityGraph,
    new: &EntityGraph,
    of_old: &HashMap<EntityId, EntityId>,
    of_new: &HashMap<EntityId, EntityId>,
) -> Vec<Status> {
    let mut status = Vec::new();
    let mut slot_of: HashMap<(EntityId, EntityId, entity_graph::ReferenceKind), usize> =
        HashMap::new();

    for r in &new.references {
        let (Some(&from), Some(&to)) = (of_new.get(&r.from), of_new.get(&r.to)) else {
            continue;
        };
        slot_of.insert((from, to, r.kind), graph.references.len());
        graph.references.push(Reference { from, to, kind: r.kind, sites: r.sites.clone() });
        status.push(Status::Added);
    }
    for r in &old.references {
        let (Some(&from), Some(&to)) = (of_old.get(&r.from), of_old.get(&r.to)) else {
            continue;
        };
        match slot_of.get(&(from, to, r.kind)) {
            Some(&slot) => status[slot] = Status::Same,
            None => {
                slot_of.insert((from, to, r.kind), graph.references.len());
                graph.references.push(Reference { from, to, kind: r.kind, sites: r.sites.clone() });
                status.push(Status::Removed);
            }
        }
    }
    status
}
