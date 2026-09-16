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

    fn descriptors(&self) -> &[Descriptor] {
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

    /// Whether this symbol denotes a rust-analyzer `impl` block itself
    /// (`.../impl#[Type][Trait]`), which is never an entity of its own.
    pub fn is_rust_impl(&self) -> bool {
        rust_impl_index(self.descriptors()).is_some()
    }

    /// The symbol with the last descriptor removed.
    pub fn parent(&self) -> Option<String> {
        let n = self.descriptors().len();
        (n > 1).then(|| self.with_descriptors(self.descriptors()[..n - 1].to_vec()))
    }

    /// rust-analyzer names methods `ns/impl#[Self][Trait]method().`; the
    /// entity a method belongs to is `ns/Self#`.
    pub fn rust_impl_self_type(&self) -> Option<String> {
        let n = self.descriptors().len();
        let owner = &self.descriptors()[..n.checked_sub(1)?];
        let at = rust_impl_index(owner)?;
        let self_type = owner.get(at + 1)?;
        let mut descriptors = owner[..at].to_vec();
        descriptors.push(Descriptor {
            name: self_type.name.clone(),
            suffix: Suffix::Type.into(),
            ..Default::default()
        });
        Some(self.with_descriptors(descriptors))
    }

    fn with_descriptors(&self, descriptors: Vec<Descriptor>) -> String {
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

/// Whether a symbol denotes a package: a container spanning every file of a
/// directory rather than one file. Only the indexer's own kind can say so.
/// The descriptor suffix cannot: SCIP's `Package` suffix is a deprecated
/// alias for `Namespace` (both proto value 1), so it matches a Rust `mod` or
/// a TypeScript file module just as readily as a Go package.
pub fn is_package(kind: Kind) -> bool {
    kind == Kind::Package
}

fn rust_impl_index(descriptors: &[Descriptor]) -> Option<usize> {
    let is_type_param = |d: &Descriptor| d.suffix.enum_value() == Ok(Suffix::TypeParameter);
    let at = descriptors.iter().rposition(|d| !is_type_param(d))?;
    let head = &descriptors[at];
    (head.name == "impl"
        && head.suffix.enum_value() == Ok(Suffix::Type)
        && at + 1 < descriptors.len())
    .then_some(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_analyzer_method_symbols_resolve_to_their_type() {
        let method =
            ParsedSymbol::parse("rust-analyzer cargo fx 0.1.0 geo/impl#[Point][Display]fmt().")
                .unwrap();
        assert_eq!(
            method.rust_impl_self_type().as_deref(),
            Some("rust-analyzer cargo fx 0.1.0 geo/Point#")
        );
        assert_eq!(
            method.parent().as_deref(),
            Some("rust-analyzer cargo fx 0.1.0 geo/impl#[Point][Display]")
        );
        assert_eq!(method.last_name(), "fmt");
        assert!(!method.is_rust_impl());

        let block = ParsedSymbol::parse("rust-analyzer cargo fx 0.1.0 geo/impl#[Point]").unwrap();
        assert!(block.is_rust_impl());

        let plain = ParsedSymbol::parse(
            "scip-typescript npm fx 0.1.0 src/`geometry.ts`/Point#magnitude().",
        )
        .unwrap();
        assert!(plain.rust_impl_self_type().is_none());
        assert_eq!(
            plain.parent().as_deref(),
            Some("scip-typescript npm fx 0.1.0 src/`geometry.ts`/Point#")
        );
        assert_eq!(
            plain.entity_kind(Kind::UnspecifiedKind),
            Some(EntityKind::Function)
        );
        assert!(ParsedSymbol::parse("local 3").is_none());
    }
}
