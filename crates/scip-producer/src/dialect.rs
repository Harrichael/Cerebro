//! How to read one indexer's SCIP output where the protocol leaves the answer
//! open. The builder asks these questions and nothing else about the tool that
//! produced the index; the answers live in `indexers/`.

use entity_graph::EntityKind;
use scip::types::ToolInfo;
use scip::types::symbol_information::Kind;

use crate::source::ColumnUnit;
use crate::symbols::ParsedSymbol;

/// `Sync` so adapters can be unit structs in a `static` registry.
pub trait Dialect: Sync {
    /// The `tool_info.name` the indexer writes. It is the only key
    /// [`for_tool`] can select on: the index it is handed need not be one this
    /// crate generated.
    fn name(&self) -> &'static str;

    /// Which lines are import statements. No indexer we have met sets the
    /// `Import` symbol role, so import edges are recognised from source text,
    /// and import syntax is the one thing here that is plainly per-language.
    /// Required rather than defaulted: an adapter that has not answered this
    /// loses every Import edge without anything failing.
    fn import_lines(&self, text: &str) -> Vec<bool>;

    /// Column unit when a document leaves `position_encoding` unspecified.
    /// The proto forbids new indexers from doing so, which is exactly why the
    /// protocol cannot say what an old one meant. Bytes is what every such
    /// indexer but scip-typescript counts.
    fn unspecified_column_unit(&self) -> ColumnUnit {
        ColumnUnit::Utf8
    }

    /// Whether a defined symbol is an entity, and of what kind. The default is
    /// the protocol reading; override to veto symbols the indexer defines that
    /// the model has no entity for.
    fn entity_kind(&self, sym: &ParsedSymbol, kind: Kind) -> Option<EntityKind> {
        sym.entity_kind(kind)
    }

    /// The symbol whose entity owns `sym`. The default strips the last
    /// descriptor; override when the indexer's symbol shape puts something
    /// between an item and its owner.
    fn parent_symbol(&self, sym: &ParsedSymbol) -> Option<String> {
        sym.parent()
    }
}

/// What the protocol alone guarantees, for an index from a tool we have no
/// adapter for. Entities, Call and TypeRef edges survive; Import edges do not,
/// and no structure beyond the descriptor grammar is inferred.
pub struct Plain;

impl Dialect for Plain {
    fn name(&self) -> &'static str {
        "scip"
    }

    fn import_lines(&self, text: &str) -> Vec<bool> {
        vec![false; text.lines().count()]
    }
}

pub fn for_tool(tool: &ToolInfo) -> &'static dyn Dialect {
    crate::indexers::ALL
        .iter()
        .find(|i| i.name() == tool.name)
        .map(|i| *i as &dyn Dialect)
        .unwrap_or(&Plain)
}

/// `line` opens with `word` as a whole token, so `use` matches `use a::b;` and
/// not `user()`. The words themselves belong to the adapters.
pub(crate) fn starts_with_word(line: &str, word: &str) -> bool {
    line.strip_prefix(word)
        .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_marks_no_line_as_an_import() {
        let text = "use a::b;\nimport { x } from \"./y\";\nimport \"fmt\"\n";
        assert_eq!(Plain.import_lines(text), [false, false, false]);
    }
}
