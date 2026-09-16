use entity_graph::EntityKind;
use scip::symbol::{format_symbol, parse_symbol};
use scip::types::descriptor::Suffix;
use scip::types::symbol_information::Kind;
use scip::types::{Descriptor, Symbol};

/// A parsed non-local SCIP symbol. `canonical` is the re-serialised form,
/// which every lookup keys on so that spelling differences between an
/// indexer's string and our derived parent strings cannot cause misses.
pub struct ParsedSymbol {
    symbol: Symbol,
    pub canonical: String,
}

impl ParsedSymbol {
    pub fn parse(raw: &str) -> Option<ParsedSymbol> {
        if raw.starts_with("local ") {
            return None;
        }
        let symbol = parse_symbol(raw).ok()?;
        if symbol.descriptors.is_empty() {
            return None;
        }
        let canonical = format_symbol(symbol.clone());
        Some(ParsedSymbol { symbol, canonical })
    }

    pub(crate) fn descriptors(&self) -> &[Descriptor] {
        &self.symbol.descriptors
    }

    fn last(&self) -> &Descriptor {
        self.descriptors()
            .last()
            .expect("non-empty by construction")
    }

    fn last_suffix(&self) -> Option<Suffix> {
        self.last().suffix.enum_value().ok()
    }

    pub fn last_name(&self) -> &str {
        &self.last().name
    }

    /// The symbol with the last descriptor removed.
    pub fn parent(&self) -> Option<String> {
        let n = self.descriptors().len();
        (n > 1).then(|| self.with_descriptors(self.descriptors()[..n - 1].to_vec()))
    }

    pub(crate) fn with_descriptors(&self, descriptors: Vec<Descriptor>) -> String {
        format_symbol(Symbol {
            descriptors,
            ..self.symbol.clone()
        })
    }

    /// Entity kind from the indexer's classification when it gives one,
    /// otherwise from the descriptor suffix. `None` means "not an entity"
    /// (fields, parameters, terms, ...).
    pub fn entity_kind(&self, kind: Kind) -> Option<EntityKind> {
        use Kind::*;
        match kind {
            Function | Method | StaticMethod | AbstractMethod | TraitMethod | SingletonMethod
            | Constructor | Macro | Getter | Setter | Accessor => Some(EntityKind::Function),
            Struct | Class | Enum | Trait | Interface | Type | TypeAlias | Union | Protocol
            | Object | Mixin | Delegate => Some(EntityKind::Class),
            Module | Namespace | Package | Library => Some(EntityKind::Module),
            UnspecifiedKind => match self.last_suffix()? {
                Suffix::Method | Suffix::Macro => Some(EntityKind::Function),
                Suffix::Type => Some(EntityKind::Class),
                Suffix::Namespace | Suffix::Package => Some(EntityKind::Module),
                _ => None,
            },
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols_parse_to_a_descriptor_parent_and_a_kind() {
        let method = ParsedSymbol::parse(
            "scip npm fx 0.1.0 src/`geometry.ts`/Point#magnitude().",
        )
        .unwrap();
        assert_eq!(
            method.parent().as_deref(),
            Some("scip npm fx 0.1.0 src/`geometry.ts`/Point#")
        );
        assert_eq!(method.last_name(), "magnitude");
        // An indexer that never sets `kind` leaves the suffix to carry it.
        assert_eq!(
            method.entity_kind(Kind::UnspecifiedKind),
            Some(EntityKind::Function)
        );
        assert_eq!(method.entity_kind(Kind::Field), None);

        let top = ParsedSymbol::parse("scip npm fx 0.1.0 src/").unwrap();
        assert_eq!(top.parent(), None);
        assert!(ParsedSymbol::parse("local 3").is_none());
    }
}
