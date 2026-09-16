use std::path::Path;
use std::process::Command;

use entity_graph::EntityKind;
use scip::types::Descriptor;
use scip::types::descriptor::Suffix;
use scip::types::symbol_information::Kind;

use crate::dialect::{Dialect, starts_with_word};
use crate::indexer::Indexer;
use crate::symbols::ParsedSymbol;

pub struct RustAnalyzer;

impl Dialect for RustAnalyzer {
    fn name(&self) -> &'static str {
        "rust-analyzer"
    }

    fn import_lines(&self, text: &str) -> Vec<bool> {
        text.lines()
            .map(|raw| starts_with_word(strip_pub(raw.trim_start()), "use"))
            .collect()
    }

    /// An `impl` block (`ns/impl#[Type][Trait]`) is never an entity. On
    /// rust-analyzer 1.98 it also has no definition occurrence, so no fixture
    /// observes this veto; it stays because `parent_symbol` below relies on the
    /// block never being a target, and a rust-analyzer that started emitting
    /// one would otherwise grow an `impl` Class and steal the methods.
    fn entity_kind(&self, sym: &ParsedSymbol, kind: Kind) -> Option<EntityKind> {
        if impl_index(sym.descriptors()).is_some() {
            return None;
        }
        sym.entity_kind(kind)
    }

    /// Methods are `ns/impl#[Self][Trait]method().`; the entity they belong to
    /// is `ns/Self#`. Trying that before the plain descriptor parent is safe:
    /// whenever it applies, the descriptor parent is the impl block, which
    /// `entity_kind` keeps out of the graph.
    fn parent_symbol(&self, sym: &ParsedSymbol) -> Option<String> {
        impl_self_type(sym).or_else(|| sym.parent())
    }
}

impl Indexer for RustAnalyzer {
    fn manifests(&self) -> &'static [&'static str] {
        &["Cargo.toml"]
    }

    fn tool(&self) -> &'static str {
        "rust-analyzer"
    }

    fn install_hint(&self) -> &'static str {
        "rustup component add rust-analyzer"
    }

    fn command(&self, out: &Path) -> Command {
        let mut cmd = Command::new(self.tool());
        cmd.args(["scip", "."]).arg("--output").arg(out);
        cmd
    }

}

fn strip_pub(line: &str) -> &str {
    let Some(rest) = line.strip_prefix("pub") else {
        return line;
    };
    let rest = match rest.strip_prefix('(') {
        Some(after) => after.split_once(')').map(|(_, r)| r).unwrap_or(""),
        None => rest,
    };
    if rest.starts_with(char::is_whitespace) {
        rest.trim_start()
    } else {
        line
    }
}

fn impl_index(descriptors: &[Descriptor]) -> Option<usize> {
    let is_type_param = |d: &Descriptor| d.suffix.enum_value() == Ok(Suffix::TypeParameter);
    let at = descriptors.iter().rposition(|d| !is_type_param(d))?;
    let head = &descriptors[at];
    (head.name == "impl"
        && head.suffix.enum_value() == Ok(Suffix::Type)
        && at + 1 < descriptors.len())
    .then_some(at)
}

fn impl_self_type(sym: &ParsedSymbol) -> Option<String> {
    let descriptors = sym.descriptors();
    let owner = &descriptors[..descriptors.len().checked_sub(1)?];
    let at = impl_index(owner)?;
    let self_type = owner.get(at + 1)?;
    let mut descriptors = owner[..at].to_vec();
    descriptors.push(Descriptor {
        name: self_type.name.clone(),
        suffix: Suffix::Type.into(),
        ..Default::default()
    });
    Some(sym.with_descriptors(descriptors))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_symbols_resolve_to_their_type_and_impl_blocks_are_not_entities() {
        let method =
            ParsedSymbol::parse("rust-analyzer cargo fx 0.1.0 geo/impl#[Point][Display]fmt().")
                .unwrap();
        assert_eq!(
            RustAnalyzer.parent_symbol(&method).as_deref(),
            Some("rust-analyzer cargo fx 0.1.0 geo/Point#")
        );
        assert_eq!(
            RustAnalyzer.entity_kind(&method, Kind::Method),
            Some(EntityKind::Function)
        );

        let block = ParsedSymbol::parse("rust-analyzer cargo fx 0.1.0 geo/impl#[Point]").unwrap();
        assert_eq!(RustAnalyzer.entity_kind(&block, Kind::TypeAlias), None);

        // Nothing but an impl block diverts parenting away from the descriptors.
        let plain = ParsedSymbol::parse("rust-analyzer cargo fx 0.1.0 geo/Point#magnitude().").unwrap();
        assert_eq!(
            RustAnalyzer.parent_symbol(&plain).as_deref(),
            Some("rust-analyzer cargo fx 0.1.0 geo/Point#")
        );
    }

    #[test]
    fn import_lines_are_use_statements_after_any_visibility() {
        let text = "use a::b;\npub use c::d;\npub(crate) use e::f;\nuser();\nlet used = 1;\n";
        assert_eq!(
            RustAnalyzer.import_lines(text),
            [true, true, true, false, false]
        );
    }
}
