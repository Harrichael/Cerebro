//! Substring search over the graph's files: their path, their name, and their
//! content lines. Text in, entity ids out -- one [`TextIndex`] is built from
//! every `File` entity the caller can produce a path and text for, and
//! reading the files is the caller's business, the same bargain [`goto`]
//! strikes.
//!
//! Three kinds share one mechanism: a doc list plus a trigram postings map
//! (`Postings`), one per kind. A query is a list of terms that must all hold;
//! a `file:`/`path:`/`content:` tag pins a term to one kind (see `search`).
//!
//! [`goto`]: crate::goto

use std::collections::{HashMap, HashSet};

use crate::{EntityGraph, EntityId, EntityKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Path,
    Content,
    /// A thing the graph knows about -- a class, a function, a module -- by
    /// name. The only one of these that searches the graph rather than the
    /// text of its files, and so the only one that can tell a definition
    /// from a line that happens to mention the word.
    Symbol,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Path => "path",
            Kind::Content => "content",
            Kind::Symbol => "symbol",
        }
    }
}

/// One match. `path` is the file's wire path regardless of which kind
/// matched (so a content hit can show its file alongside the line); `text`
/// is the thing the needle was found in (name / path / line) and `start`,
/// `end` locate the match within it, in chars. Chars because folding is
/// 1:1 on chars and the postings count in them; a consumer that needs bytes
/// or UTF-16 converts at its own edge, where it knows why.
pub struct Hit {
    pub kind: Kind,
    pub file: EntityId,
    pub path: String,
    pub line: Option<usize>,
    pub text: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy)]
pub struct Limits {
    pub file: usize,
    pub path: usize,
    pub content: usize,
    pub symbol: usize,
}

#[derive(Default)]
pub struct More {
    pub file: usize,
    pub path: usize,
    pub content: usize,
    pub symbol: usize,
}

pub struct SearchResult {
    pub query: String,
    pub hits: Vec<Hit>,
    pub more: More,
    /// What the query asked to be shown around each content hit, if it said.
    /// Carried out rather than acted on: the default belongs to whoever is
    /// drawing, and a default hidden in here is one the other consumer would
    /// inherit without ever asking for it.
    pub context: Option<usize>,
}

/// A run of consecutive lines of one file, with the matches inside it.
/// Matches whose shown lines run together are one of these, not several.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excerpt {
    pub file: EntityId,
    pub path: String,
    /// 0-indexed line of `text[0]`.
    pub first_line: usize,
    pub text: Vec<String>,
    /// Ascending, and every one lies within the lines held here.
    pub matched: Vec<Mark>,
}

/// Where the needle was found: `line` absolute and 0-indexed, `start` and
/// `end` byte offsets into that line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

/// Beyond this, "context" stops meaning context and starts meaning the file.
const MAX_CONTEXT: usize = 50;

struct FileDoc {
    id: EntityId,
    path: String,
    /// This file's run of `TextIndex::lines`. Without it the index holds
    /// every line but cannot say where one file's lines stop, which is what
    /// showing context around a match needs.
    lines: std::ops::Range<u32>,
    name: String,
}

/// One symbol-postings document: a named thing in the graph, and where it
/// was declared.
struct SymbolDoc {
    file_idx: u32,
    name: String,
    line: u32,
}

/// One content-postings document: a single line of a single file.
struct LineDoc {
    file_idx: u32,
    line: u32,
    text: String,
}

/// A doc list plus the trigram -> doc-index map used to shortlist candidates
/// for needles of 3+ chars. `texts` are case-folded so search is
/// case-insensitive without touching every candidate at query time.
struct Postings {
    texts: Vec<String>,
    grams: HashMap<[char; 3], Vec<u32>>,
}

impl Postings {
    fn build(texts: Vec<String>) -> Self {
        let mut grams: HashMap<[char; 3], Vec<u32>> = HashMap::new();
        for (i, text) in texts.iter().enumerate() {
            let i = i as u32;
            let chars: Vec<char> = text.chars().collect();
            if chars.len() < 3 {
                continue;
            }
            for w in chars.windows(3) {
                let list = grams.entry([w[0], w[1], w[2]]).or_default();
                // Docs are inserted in ascending order, and a doc can repeat
                // a trigram many times (e.g. "aaaa"); only the first
                // occurrence per doc should land in the list.
                if list.last() != Some(&i) {
                    list.push(i);
                }
            }
        }
        Postings { texts, grams }
    }

    /// `(doc, start_char)` of the first occurrence in each matching doc,
    /// ascending by doc. `needle_folded` must already be case-folded.
    fn find(&self, needle_folded: &str) -> Vec<(u32, usize)> {
        let needle_chars: Vec<char> = needle_folded.chars().collect();
        if needle_chars.len() < 3 {
            return self
                .texts
                .iter()
                .enumerate()
                .filter_map(|(i, t)| t.find(needle_folded).map(|b| (i as u32, char_count(t, b))))
                .collect();
        }

        let mut trigrams: Vec<[char; 3]> =
            needle_chars.windows(3).map(|w| [w[0], w[1], w[2]]).collect();
        trigrams.dedup();
        let mut lists: Vec<&Vec<u32>> = Vec::with_capacity(trigrams.len());
        for g in &trigrams {
            match self.grams.get(g) {
                Some(l) => lists.push(l),
                // A trigram of the needle appears in no doc at all, so the
                // needle cannot occur anywhere either.
                None => return Vec::new(),
            }
        }
        lists.sort_by_key(|l| l.len());
        let mut candidates = lists[0].clone();
        for l in &lists[1..] {
            if candidates.is_empty() {
                break;
            }
            candidates = intersect_sorted(&candidates, l, |&d| d);
        }

        // Trigram co-occurrence only means the needle's 3-grams are all
        // present somewhere in the doc, not that they line up in order; a
        // real substring search on each candidate is what actually decides.
        candidates
            .into_iter()
            .filter_map(|doc| {
                let t = &self.texts[doc as usize];
                t.find(needle_folded).map(|b| (doc, char_count(t, b)))
            })
            .collect()
    }
}

/// Merge-intersection of two lists ascending in `key`; on a tie the entry
/// from `a` is kept.
fn intersect_sorted<T: Copy>(a: &[T], b: &[T], key: impl Fn(&T) -> u32) -> Vec<T> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match key(&a[i]).cmp(&key(&b[j])) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

fn char_count(text: &str, byte_offset: usize) -> usize {
    text[..byte_offset].chars().count()
}

// `char::to_lowercase` can yield more than one char for a handful of special
// casings; taking only the first keeps folding 1:1 on char count, so a char
// offset into the folded text is always the same offset into the original
// (byte offsets do not carry over the same way — `İ` folds to a shorter `i`).
fn fold(s: &str) -> String {
    s.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect()
}

/// Byte offsets for a char span, because nvim's extmark columns and Rust's
/// own slicing both count bytes.
fn byte_span(text: &str, start_chars: usize, len_chars: usize) -> (usize, usize) {
    let at = |n: usize| text.char_indices().nth(n).map_or(text.len(), |(b, _)| b);
    (at(start_chars), at(start_chars + len_chars))
}

fn split_lines(text: &str) -> impl Iterator<Item = &str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    // A trailing newline produces one empty trailing piece that is not a line.
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines.into_iter().map(|l| l.strip_suffix('\r').unwrap_or(l))
}

/// One query term. `tag` is the kind the term was written for; a bare term
/// has none and is about whatever text a hit is itself made of.
struct Term {
    tag: Option<Kind>,
    needle: String,
    folded: String,
}

/// Whitespace-separated terms; double quotes keep a phrase together (and the
/// quotes themselves out), so `content:"fn respond"` is one term. A tag only
/// counts when written outside the quotes: `"file:x"` is a literal. A tag
/// with nothing after it is dropped.
///
/// `context:N` comes back beside the terms rather than as one of them: it
/// narrows nothing, it says how much of the file to show around what the
/// terms already found. Last one wins, and `context:` with a payload that is
/// not a number stays an ordinary term, as any other `word:word` does.
fn tokenize(q: &str) -> (Vec<Term>, Option<usize>) {
    let mut terms = Vec::new();
    let mut context = None;
    let mut chars = q.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let quoted_start = c == '"';
        let mut raw = String::new();
        let mut in_quote = false;
        while let Some(&c) = chars.peek() {
            if c == '"' {
                in_quote = !in_quote;
            } else if c.is_whitespace() && !in_quote {
                break;
            } else {
                raw.push(c);
            }
            chars.next();
        }
        if !quoted_start && let Some(rest) = strip_prefix_ci(&raw, "context:") {
            if rest.is_empty() {
                continue;
            }
            if let Ok(n) = rest.parse::<usize>() {
                context = Some(n);
                continue;
            }
        }
        let (tag, needle) = if quoted_start { (None, raw.as_str()) } else { split_tag(&raw) };
        if !needle.is_empty() {
            terms.push(Term { tag, needle: needle.to_string(), folded: fold(needle) });
        }
    }
    (terms, context)
}

fn split_tag(raw: &str) -> (Option<Kind>, &str) {
    for (prefix, kind) in [
        ("file:", Kind::File),
        ("path:", Kind::Path),
        ("content:", Kind::Content),
        ("symbol:", Kind::Symbol),
    ] {
        if let Some(rest) = strip_prefix_ci(raw, prefix) {
            return (Some(kind), rest);
        }
    }
    (None, raw)
}

// The three prefixes are ASCII, so a byte-wise ASCII-case-insensitive
// compare is safe: it only slices at a boundary it just confirmed is ASCII.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let bytes = prefix.len();
    if s.len() >= bytes && s.as_bytes()[..bytes].eq_ignore_ascii_case(prefix.as_bytes()) {
        Some(&s[bytes..])
    } else {
        None
    }
}

fn truncate<T>(mut items: Vec<T>, limit: usize) -> (Vec<T>, usize) {
    if items.len() > limit {
        let more = items.len() - limit;
        items.truncate(limit);
        (items, more)
    } else {
        (items, 0)
    }
}

pub struct TextIndex {
    files: Vec<FileDoc>,
    file_postings: Postings,
    path_postings: Postings,
    content_postings: Postings,
    symbol_postings: Postings,
    lines: Vec<LineDoc>,
    symbols: Vec<SymbolDoc>,
}

impl TextIndex {
    /// One document set per kind over every File entity of `graph`. `read`
    /// returns the file's wire path and text, or `None` to leave it out
    /// (unreadable, too big, not UTF-8).
    pub fn build(graph: &EntityGraph, mut read: impl FnMut(EntityId) -> Option<(String, String)>) -> Self {
        let mut symbols = Vec::new();
        let mut symbol_names_folded = Vec::new();
        let mut of_file: HashMap<EntityId, u32> = HashMap::new();
        let mut files = Vec::new();
        let mut file_names_folded = Vec::new();
        let mut paths_folded = Vec::new();
        let mut content_folded = Vec::new();
        let mut lines = Vec::new();

        for entity in &graph.entities {
            if entity.kind != EntityKind::File {
                continue;
            }
            let Some((mut path, text)) = read(entity.id) else { continue };
            // A single-file load has an empty relative path; the name is the
            // only thing left to match a `path:` query against.
            if path.is_empty() {
                path = entity.name.clone();
            }

            let file_idx = files.len() as u32;
            let first_line = lines.len() as u32;
            file_names_folded.push(fold(&entity.name));
            paths_folded.push(fold(&path));
            for (line_no, line) in split_lines(&text).enumerate() {
                content_folded.push(fold(line));
                lines.push(LineDoc { file_idx, line: line_no as u32, text: line.to_string() });
            }
            of_file.insert(entity.id, file_idx);
            files.push(FileDoc {
                id: entity.id,
                path,
                lines: first_line..lines.len() as u32,
                name: entity.name.clone(),
            });
        }

        // Symbols after the files, because a symbol is placed by the file
        // that holds it, and that mapping is what the loop above built.
        for entity in &graph.entities {
            if matches!(entity.kind, EntityKind::File | EntityKind::Folder) {
                continue;
            }
            let Some(file_idx) = graph.file_of(entity.id).and_then(|f| of_file.get(&f)).copied()
            else {
                continue;
            };
            symbol_names_folded.push(fold(&entity.name));
            symbols.push(SymbolDoc {
                file_idx,
                name: entity.name.clone(),
                line: entity.line_range.start as u32,
            });
        }

        TextIndex {
            files,
            file_postings: Postings::build(file_names_folded),
            path_postings: Postings::build(paths_folded),
            content_postings: Postings::build(content_folded),
            symbol_postings: Postings::build(symbol_names_folded),
            lines,
            symbols,
        }
    }

    /// Every term must hold for a hit. A term holds against the hit's own
    /// text (a bare term, or one tagged with the hit's kind) or, tagged with
    /// another kind, against the hit's file: `file:` its name, `path:` its
    /// path, `content:` any one of its lines. A kind only yields hits when at
    /// least one term is about its own text, so `file:x` alone never lists
    /// every line of the matching files. The span marked on a hit is the
    /// first own-text term's.
    /// The content hits of `hits`, each shown with `context` lines either
    /// side, in file and line order. Hits whose shown lines run together --
    /// overlapping, or merely leaving no line between them -- come back as
    /// one excerpt with several marks: drawing them apart would claim the
    /// file was cut where it was not. Hits of other kinds name no line and
    /// are left to the caller.
    pub fn excerpts(&self, hits: &[Hit], context: usize) -> Vec<Excerpt> {
        let context = context.min(MAX_CONTEXT);
        // `build` walks `graph.entities`, which is indexed by id, so `files`
        // comes out ascending by id and can be searched rather than scanned.
        let file_of = |id: EntityId| {
            self.files.binary_search_by_key(&id.0, |f| f.id.0).ok().map(|i| i as u32)
        };

        let mut found: Vec<(u32, Mark)> = hits
            .iter()
            .filter(|h| matches!(h.kind, Kind::Content | Kind::Symbol))
            .filter_map(|h| {
                let line = h.line?;
                let idx = file_of(h.file)?;
                // A content hit was found *in* the line, so its offsets are
                // already offsets into it. A symbol hit was found in a name,
                // and the line to show is where that name was declared -- so
                // the span is wherever the name sits on it.
                let (start, end) = match h.kind {
                    Kind::Symbol => {
                        let f = &self.files[idx as usize];
                        let at = (f.lines.start as usize).checked_add(line)?;
                        let text = &self.lines.get(at)?.text;
                        let at = text.find(h.text.as_str()).unwrap_or(0);
                        (at, at + h.text.len())
                    }
                    _ => byte_span(&h.text, h.start, h.end - h.start),
                };
                Some((idx, Mark { line, start, end }))
            })
            .collect();
        // Sorted here rather than trusting `search`'s order: an unstated
        // agreement between two public methods is a trap.
        found.sort_by_key(|&(idx, m)| (idx, m.line));

        let mut out: Vec<Excerpt> = Vec::new();
        let mut open: Option<(u32, usize, usize)> = None; // file, first, last
        for (idx, mark) in found {
            let f = &self.files[idx as usize];
            let last_line = (f.lines.end - f.lines.start) as usize;
            let Some(last_line) = last_line.checked_sub(1) else { continue };
            let lo = mark.line.saturating_sub(context);
            let hi = (mark.line + context).min(last_line);

            match open {
                // `last + 1` and not `last`: two windows with no line between
                // them are one unbroken run of the file.
                Some((of, first, last)) if of == idx && lo <= last + 1 => {
                    open = Some((of, first, hi.max(last)));
                    out.last_mut().expect("an open run has an excerpt").matched.push(mark);
                }
                _ => {
                    open = Some((idx, lo, hi));
                    out.push(Excerpt {
                        file: f.id,
                        path: f.path.clone(),
                        first_line: lo,
                        text: Vec::new(),
                        matched: vec![mark],
                    });
                }
            }
            if let (Some((_, first, last)), Some(e)) = (open, out.last_mut()) {
                let run = f.lines.start as usize;
                e.text = (first..=last).map(|n| self.lines[run + n].text.clone()).collect();
            }
        }
        out
    }

    pub fn search(&self, query: &str, limits: Limits) -> SearchResult {
        let query = query.trim();
        let (terms, context) = tokenize(query);
        let mut result =
            SearchResult { query: query.to_string(), hits: Vec::new(), more: More::default(), context };
        if terms.is_empty() {
            return result;
        }

        let postings = |k: Kind| match k {
            Kind::File => &self.file_postings,
            Kind::Path => &self.path_postings,
            Kind::Content => &self.content_postings,
            Kind::Symbol => &self.symbol_postings,
        };
        // The files a tagged term admits when it acts as a filter on another
        // kind's hits, as ascending file indices. Line docs are in file order,
        // so mapping them to files needs only a dedup.
        let filter_sets: Vec<Option<Vec<u32>>> = terms
            .iter()
            .map(|t| {
                let kind = t.tag?;
                let docs = postings(kind).find(&t.folded);
                let mut files: Vec<u32> = match kind {
                    Kind::Content => docs.iter().map(|&(d, _)| self.lines[d as usize].file_idx).collect(),
                    Kind::Symbol => docs.iter().map(|&(d, _)| self.symbols[d as usize].file_idx).collect(),
                    _ => docs.iter().map(|&(d, _)| d).collect(),
                };
                files.sort_unstable();
                files.dedup();
                Some(files)
            })
            .collect();

        // `(doc, start_char)` per hit of `kind`, plus the char length of the
        // marked term.
        let matches = |kind: Kind| -> (Vec<(u32, usize)>, usize) {
            let own: Vec<usize> =
                (0..terms.len()).filter(|&i| terms[i].tag.is_none() || terms[i].tag == Some(kind)).collect();
            let Some(&first) = own.first() else { return (Vec::new(), 0) };
            let p = postings(kind);
            let mut cands = p.find(&terms[first].folded);
            for &i in &own[1..] {
                if cands.is_empty() {
                    break;
                }
                cands = intersect_sorted(&cands, &p.find(&terms[i].folded), |m| m.0);
            }
            let file_of = |doc: u32| match kind {
                Kind::Content => self.lines[doc as usize].file_idx,
                Kind::Symbol => self.symbols[doc as usize].file_idx,
                _ => doc,
            };
            for (i, set) in filter_sets.iter().enumerate() {
                let Some(set) = set else { continue };
                if own.contains(&i) {
                    continue;
                }
                cands.retain(|&(doc, _)| set.binary_search(&file_of(doc)).is_ok());
            }
            (cands, terms[first].needle.chars().count())
        };
        let (mut symbol_matches, symbol_len) = matches(Kind::Symbol);
        let (mut file_matches, file_len) = matches(Kind::File);
        let (mut path_matches, path_len) = matches(Kind::Path);
        let (mut content_matches, content_len) = matches(Kind::Content);

        // Shortest name first: searching `new` should show `new` before
        // `renewal`, the same rule the file list already follows.
        symbol_matches.sort_by(|a, b| {
            let (sa, sb) = (&self.symbols[a.0 as usize], &self.symbols[b.0 as usize]);
            sa.name
                .chars()
                .count()
                .cmp(&sb.name.chars().count())
                .then_with(|| self.files[sa.file_idx as usize].path.cmp(&self.files[sb.file_idx as usize].path))
                .then_with(|| sa.line.cmp(&sb.line))
        });
        file_matches.sort_by(|a, b| {
            let (fa, fb) = (&self.files[a.0 as usize], &self.files[b.0 as usize]);
            fa.name.chars().count().cmp(&fb.name.chars().count()).then_with(|| fa.path.cmp(&fb.path))
        });
        path_matches.sort_by(|a, b| {
            let (fa, fb) = (&self.files[a.0 as usize], &self.files[b.0 as usize]);
            fa.path.chars().count().cmp(&fb.path.chars().count()).then_with(|| fa.path.cmp(&fb.path))
        });
        content_matches.sort_by(|a, b| {
            let (la, lb) = (&self.lines[a.0 as usize], &self.lines[b.0 as usize]);
            let (fa, fb) = (&self.files[la.file_idx as usize], &self.files[lb.file_idx as usize]);
            fa.path.cmp(&fb.path).then_with(|| la.line.cmp(&lb.line))
        });

        // A path hit marked inside the trailing filename segment, for a file
        // that is also a file hit, shows the same occurrence twice; drop it.
        // A mark in a directory segment ("main/" in "main/src/main.rs" for
        // "main") is a different occurrence and stays.
        let file_hit_docs: HashSet<u32> = file_matches.iter().map(|m| m.0).collect();
        path_matches.retain(|&(doc, start_chars)| {
            let f = &self.files[doc as usize];
            !file_hit_docs.contains(&doc)
                || start_chars < f.path.chars().count().saturating_sub(f.name.chars().count())
        });

        let (symbol_matches, symbol_more) = truncate(symbol_matches, limits.symbol);
        let (file_matches, file_more) = truncate(file_matches, limits.file);
        let (path_matches, path_more) = truncate(path_matches, limits.path);
        let (content_matches, content_more) = truncate(content_matches, limits.content);

        let hits = &mut result.hits;
        // Symbols first: a definition is a better answer than a line that
        // mentions the same word, and a list is read from the top.
        hits.extend(symbol_matches.into_iter().map(|(doc, start)| {
            let sym = &self.symbols[doc as usize];
            let f = &self.files[sym.file_idx as usize];
            Hit {
                kind: Kind::Symbol,
                file: f.id,
                path: f.path.clone(),
                line: Some(sym.line as usize),
                text: sym.name.clone(),
                start,
                end: start + symbol_len,
            }
        }));
        hits.extend(file_matches.into_iter().map(|(doc, start)| {
            let f = &self.files[doc as usize];
            let end = start + file_len;
            Hit { kind: Kind::File, file: f.id, path: f.path.clone(), line: None, text: f.name.clone(), start, end }
        }));
        hits.extend(path_matches.into_iter().map(|(doc, start)| {
            let f = &self.files[doc as usize];
            let end = start + path_len;
            Hit { kind: Kind::Path, file: f.id, path: f.path.clone(), line: None, text: f.path.clone(), start, end }
        }));
        hits.extend(content_matches.into_iter().map(|(doc, start)| {
            let l = &self.lines[doc as usize];
            let f = &self.files[l.file_idx as usize];
            let end = start + content_len;
            Hit {
                kind: Kind::Content,
                file: f.id,
                path: f.path.clone(),
                line: Some(l.line as usize),
                text: l.text.clone(),
                start,
                end,
            }
        }));
        result.more =
            More { file: file_more, path: path_more, content: content_more, symbol: symbol_more };
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Postings edge cases below trigram intersection: co-occurrence without
    /// containment is rejected by verification, a sub-3-char needle takes
    /// the linear-scan path, a repeated trigram collapses to one posting,
    /// and folding keeps char offsets 1:1 even when it changes byte length
    /// (case folding) or the text holds an astral char (UTF-16 offsets).
    #[test]
    fn postings_edge_cases() {
        let raw = ["Größe", "abc bcd cde", "aaaa", "xy", "😀 Foo"];
        let folded: Vec<String> = raw.iter().map(|s| fold(s)).collect();
        let p = Postings::build(folded);

        // Every trigram of "abcde" occurs in "abc bcd cde", but the
        // substring itself does not.
        assert!(p.find(&fold("abcde")).is_empty());

        // A 2-char needle bypasses the postings and scans linearly.
        assert_eq!(p.find(&fold("xy")), vec![(3, 0)]);

        // "aaaa" repeats the trigram "aaa" three times; it appears once.
        assert_eq!(p.grams[&['a', 'a', 'a']], vec![2]);

        // "ß" does not fold to "ss", so a needle built from a differently
        // cased original still lands on the same char offset.
        assert_eq!(p.find(&fold("ÖßE")), vec![(0, 2)]);

        // A match after an astral char: "😀" is one char, four bytes, and
        // two UTF-16 units. The postings count chars; whoever needs another
        // unit converts.
        assert_eq!(p.find(&fold("foo")), vec![(4, 2)]);
        assert_eq!(byte_span("😀 Foo", 2, 3), (5, 8));
    }

    /// The query grammar: whitespace splits terms, a tag binds only outside
    /// quotes, quotes join a phrase, a dangling tag is dropped.
    #[test]
    fn tokenize_terms() {
        let (terms, context) = tokenize(r#"class FILE:resolver content:"fn respond" "file:x" path:"#);
        let seen: Vec<(Option<Kind>, &str)> = terms.iter().map(|t| (t.tag, t.needle.as_str())).collect();
        assert_eq!(
            seen,
            vec![
                (None, "class"),
                (Some(Kind::File), "resolver"),
                (Some(Kind::Content), "fn respond"),
                (None, "file:x"),
            ]
        );

        // `context:` is not one of the terms; it is the answer to a different
        // question, and a payload that is not a number is just a word.
        assert_eq!(context, None);
        assert_eq!(tokenize("needle context:4").1, Some(4));
        assert_eq!(tokenize("context:4 needle context:7").1, Some(7), "the last one wins");
        assert_eq!(tokenize("needle context:").1, None, "a dangling tag is dropped");
        assert_eq!(tokenize("needle context:").0.len(), 1);
        let (terms, ctx) = tokenize("context:abc");
        assert_eq!((terms.len(), ctx), (1, None), "a payload that is not a number is a term");
        assert_eq!(tokenize(r#""context:4""#).1, None, "quoted, it is a literal");
    }

    use crate::EntityKind::{Class, File, Folder, Function};
    use crate::test_support::graph_from_parents;

    const LIMITS: Limits = Limits { file: 20, path: 20, content: 200, symbol: 20 };

    /// `files` as (name, text); every file hangs off one folder.
    fn index(files: &[(&str, String)]) -> TextIndex {
        let mut rows: Vec<(&str, crate::EntityKind, Option<usize>)> = vec![("root", Folder, None)];
        rows.extend(files.iter().map(|(name, _)| (*name, File, Some(0))));
        let graph = graph_from_parents(&rows, &[]);
        let owned: Vec<(String, String)> =
            files.iter().map(|(n, t)| (n.to_string(), t.clone())).collect();
        TextIndex::build(&graph, |id| owned.get(id.0.wrapping_sub(1)).cloned())
    }

    /// `n` numbered lines, with the needle on each of `on`.
    fn lines(n: usize, on: &[usize]) -> String {
        (0..n)
            .map(|i| {
                if on.contains(&i) { format!("line {i} needle here") } else { format!("line {i}") }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn found(index: &TextIndex, context: usize) -> Vec<Excerpt> {
        index.excerpts(&index.search("needle", LIMITS).hits, context)
    }

    /// Matches near enough to share the lines they show are one excerpt, and
    /// the run grows as far as the chain reaches -- 10, 13 and 16 arrive as
    /// three separate matches and come back as one block. A match at the top
    /// of the file shows what there is rather than reaching past it.
    #[test]
    fn context_windows_merge_when_they_run_together() {
        let text = lines(20, &[1, 10, 13, 16]);
        let idx = index(&[("a.rs", text.clone())]);
        let got = found(&idx, 2);

        assert_eq!(got.len(), 2, "{got:#?}");

        assert_eq!(got[0].first_line, 0, "a match on line 1 cannot show a line before line 0");
        assert_eq!(got[0].text.len(), 4);
        assert_eq!(got[0].matched.iter().map(|m| m.line).collect::<Vec<_>>(), vec![1]);

        assert_eq!(got[1].first_line, 8);
        assert_eq!(got[1].text.len(), 11, "8..=18 is one run, not three blocks");
        assert_eq!(got[1].matched.iter().map(|m| m.line).collect::<Vec<_>>(), vec![10, 13, 16]);

        // The lines shown are the file's own, in order -- the assertion that
        // catches an off-by-one in the run bounds.
        let all: Vec<&str> = text.split('\n').collect();
        assert_eq!(got[1].text, all[8..=18].to_vec());
        assert_eq!(got[0].path, "a.rs");
    }

    /// The boundary the merge turns on, as data: windows that leave no line
    /// between them are one run; a single skipped line makes two. Holds at
    /// `context:0`, where the window is the matched line itself.
    #[test]
    fn touching_windows_and_gaps() {
        let touching = index(&[("a.rs", lines(30, &[10, 15]))]);
        let got = found(&touching, 2);
        assert_eq!(got.len(), 1, "8..12 and 13..17 skip no line: {got:#?}");
        assert_eq!((got[0].first_line, got[0].text.len()), (8, 10));

        let gapped = index(&[("a.rs", lines(30, &[10, 16]))]);
        assert_eq!(found(&gapped, 2).len(), 2, "line 13 is skipped, so the file was cut");

        let abutting = index(&[("a.rs", lines(30, &[10, 11]))]);
        let got = found(&abutting, 0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].matched.len(), 2);
        assert_eq!(found(&index(&[("a.rs", lines(30, &[10, 12]))]), 0).len(), 2);
    }

    /// Two files whose windows would overlap by line number are still two
    /// excerpts, and a mark's columns are bytes, not chars.
    #[test]
    fn excerpts_stay_within_their_file_and_mark_bytes() {
        let a = "0\n1\nlet é = needle;\n3\n4";
        let idx = index(&[("a.rs", a.to_string()), ("b.rs", lines(10, &[2]))]);
        let got = found(&idx, 5);

        assert_eq!(got.len(), 2, "{got:#?}");
        assert_eq!(got[0].path, "a.rs");
        assert_eq!(got[1].path, "b.rs");
        assert!(got[1].text.iter().all(|l| !l.contains('é')), "a file's lines leaked into another");

        let m = got[0].matched[0];
        let line = &got[0].text[m.line - got[0].first_line];
        assert_eq!(&line[m.start..m.end], "needle", "columns must be byte offsets");
        assert_eq!(m.start, line.find("needle").unwrap());
    }


    /// The point of `symbol:`: it searches what the graph knows is there, so
    /// it answers with the thing itself rather than with every line that
    /// happens to say its name. A plain term still finds both, symbols first.
    #[test]
    fn a_symbol_search_finds_the_definition_and_not_the_mentions() {
        let text = "// a widget is made here\nstruct Widget;\n// another widget mention\nfn render() {}\n";
        let mut graph = graph_from_parents(
            &[("root", Folder, None), ("ui.rs", File, Some(0)), ("Widget", Class, Some(1)), ("render", Function, Some(1))],
            &[],
        );
        graph.entities[2].line_range = 1..2;
        graph.entities[3].line_range = 3..4;
        let idx = TextIndex::build(&graph, |id| {
            (id == EntityId(1)).then(|| ("ui.rs".to_string(), text.to_string()))
        });

        let tagged = idx.search("symbol:widget", LIMITS);
        let kinds: Vec<_> = tagged.hits.iter().map(|h| (h.kind, h.text.as_str(), h.line)).collect();
        assert_eq!(kinds, vec![(Kind::Symbol, "Widget", Some(1))], "only the declaration");

        // A bare term finds the lines too, with the definition first.
        let bare = idx.search("widget", LIMITS);
        assert_eq!(bare.hits[0].kind, Kind::Symbol);
        assert!(
            bare.hits.iter().filter(|h| h.kind == Kind::Content).count() >= 2,
            "the mentions are still there: {:?}",
            bare.hits.iter().map(|h| h.kind).collect::<Vec<_>>()
        );

        // Shown, a symbol hit is its declaration line, marked where the name
        // sits on it -- not where it sat in the name.
        let ex = idx.excerpts(&tagged.hits, 0);
        assert_eq!(ex.len(), 1);
        assert_eq!(ex[0].text, vec!["struct Widget;"]);
        let m = ex[0].matched[0];
        assert_eq!(&ex[0].text[0][m.start..m.end], "Widget");
    }

}
