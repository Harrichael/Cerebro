//! What an identifier in a source line refers to.
//!
//! A reference records the lines it was seen on ([`Site`]), so the entity an
//! identifier names can be recovered from where it was written: take the
//! references sited on that line of that file, and keep the one the clicked
//! token names.
//!
//! The rule is spelled out in the server's `ui/CONTRACT.md`, under `sites`,
//! and `ui/goto.js` is the other implementation of it. Both answer to the
//! same examples; if this changes, that changes.
//!
//! Text in, entity ids out. Reading the file is the caller's business.

use crate::{EntityGraph, EntityId, Reference};

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The identifier spanning byte `col` of `text`, or `None` off one.
///
/// A caret sits *between* characters, so the one to its right is what was
/// pointed at -- unless that is where a word ends, in which case it is the
/// one to its left. Without that, clicking off the end of a name finds
/// nothing, which is exactly where a reader's cursor tends to stop.
pub fn token_at(text: &str, col: usize) -> Option<&str> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let right = chars.iter().position(|(at, _)| *at >= col);
    let hit = match right {
        Some(r) if is_ident(chars[r].1) => r,
        _ => {
            let end = right.unwrap_or(chars.len());
            end.checked_sub(1).filter(|&before| is_ident(chars[before].1))?
        }
    };
    let mut start = hit;
    while start > 0 && is_ident(chars[start - 1].1) {
        start -= 1;
    }
    let mut end = hit + 1;
    while end < chars.len() && is_ident(chars[end].1) {
        end += 1;
    }
    Some(&text[chars[start].0..chars.get(end).map_or(text.len(), |(at, _)| *at)])
}

/// Does `name` occur in `text` as a whole word? `focus` is not in `focused`.
fn has_token(text: &str, name: &str) -> bool {
    !name.is_empty()
        && text.match_indices(name).any(|(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + name.len()..].chars().next();
            !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
        })
}

/// The entities the identifier at byte `col` of `text` may refer to, where
/// `text` is line `line` (0-indexed) of `file`.
///
/// The target's name has to be the clicked token. The exception is a target
/// whose name does not occur on the line at all -- an alias, a file imported
/// by its stem -- which is reachable from any identifier there. Several ids
/// come back only when the graph itself has several same-named targets on
/// that line; the caller decides whether its view still tells them apart.
pub fn reference_targets(
    graph: &EntityGraph,
    file: EntityId,
    line: usize,
    text: &str,
    col: usize,
) -> Vec<EntityId> {
    let Some(token) = token_at(text, col) else { return Vec::new() };
    // Matched on the File entity, never on a path: a hierarchy path and a
    // filesystem path are not the same shape.
    let sited: Vec<&Reference> = graph
        .references
        .iter()
        .filter(|r| r.sites.iter().any(|s| s.line == line) && graph.file_of(r.from) == Some(file))
        .collect();
    let name_of = |r: &Reference| graph.get(r.to).map(|e| e.name.as_str());

    let named: Vec<EntityId> =
        sited.iter().filter(|r| name_of(r) == Some(token)).map(|r| r.to).collect();
    let picked = match named.is_empty() {
        false => named,
        true => sited
            .iter()
            .filter(|r| !name_of(r).is_some_and(|name| has_token(text, name)))
            .map(|r| r.to)
            .collect(),
    };

    let mut seen = Vec::new();
    for id in picked {
        if !seen.contains(&id) {
            seen.push(id);
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::graph_from_parents;
    use crate::{EntityKind, ReferenceKind, Site};

    /// Two files, each with a function, and a caller that refers to both.
    /// Ids: 0 proj, 1 a.rs, 2 focus, 3 focused, 4 b.rs, 5 caller.
    fn graph(refs: &[(usize, usize, usize)]) -> EntityGraph {
        let rows = [
            ("proj", EntityKind::Folder, None),
            ("a.rs", EntityKind::File, Some(0)),
            ("focus", EntityKind::Function, Some(1)),
            ("focused", EntityKind::Function, Some(1)),
            ("b.rs", EntityKind::File, Some(0)),
            ("caller", EntityKind::Function, Some(4)),
        ];
        let mut graph = graph_from_parents(&rows, &[]);
        graph.references = refs
            .iter()
            .map(|&(from, to, line)| Reference {
                from: EntityId(from),
                to: EntityId(to),
                kind: ReferenceKind::Call,
                sites: vec![Site { line }],
            })
            .collect();
        graph
    }

    fn targets(graph: &EntityGraph, text: &str, col: usize) -> Vec<EntityId> {
        reference_targets(graph, EntityId(4), 7, text, col)
    }

    /// Wherever on the word the cursor lands, including the space just past
    /// its end, the word is what was meant.
    #[test]
    fn a_caret_anywhere_on_a_word_finds_that_word() {
        let line = "    focus(x);";
        // Start, middle, and the closing bracket just past the end of it.
        for col in [4, 6, 9] {
            assert_eq!(token_at(line, col), Some("focus"), "column {col}");
        }
        assert_eq!(token_at(line, 10), Some("x"), "a one-character name is a name");
        assert_eq!(token_at(line, 12), None, "punctuation on both sides is not a word");
        assert_eq!(token_at(line, line.len()), None, "off the end of the line");
        assert_eq!(token_at(line, 1), None, "in the indent");
        assert_eq!(token_at("", 0), None);
        // Bytes, not characters: an identifier after one is still found whole.
        assert_eq!(token_at("// π then focus", 13), Some("focus"));
    }

    /// The whole point: one line can call two things, and which one you asked
    /// about is the word under the cursor.
    #[test]
    fn two_calls_on_a_line_are_told_apart_by_the_word_under_the_cursor() {
        let graph = graph(&[(5, 2, 7), (5, 3, 7)]);
        let line = "    focus(); focused();";
        assert_eq!(targets(&graph, line, 4), vec![EntityId(2)]);
        assert_eq!(targets(&graph, line, 13), vec![EntityId(3)]);
    }

    /// `focus` is not `refocus`. Once a target is named on the line as a
    /// whole word, it answers to that word and to nothing else there -- a
    /// substring match would make every identifier containing it a way in.
    #[test]
    fn a_target_named_on_the_line_answers_only_to_its_own_word() {
        let graph = graph(&[(5, 2, 7)]);
        let line = "    focus(); // refocus later";
        assert_eq!(targets(&graph, line, 4), vec![EntityId(2)]);
        assert!(targets(&graph, line, 17).is_empty(), "`refocus` resolved to `focus`");
        assert!(targets(&graph, line, 25).is_empty(), "an unrelated word resolved to it");
    }

    /// A target whose name is nowhere on the line -- an alias, a file
    /// imported by its stem -- is reachable from any identifier there.
    #[test]
    fn a_target_not_named_on_the_line_is_reachable_from_anything_on_it() {
        let graph = graph(&[(5, 2, 7)]);
        assert_eq!(targets(&graph, "    use crate::renamed;", 11), vec![EntityId(2)]);
    }

    /// References belonging to another file, or to another line, are not
    /// about this identifier however alike they look.
    #[test]
    fn references_from_elsewhere_are_not_on_this_line() {
        let elsewhere = graph(&[(2, 3, 7)]); // sited on line 7, but of a.rs
        assert!(targets(&elsewhere, "    focused();", 4).is_empty());

        let other_line = graph(&[(5, 2, 9)]);
        assert!(targets(&other_line, "    focus();", 4).is_empty());
    }
}
