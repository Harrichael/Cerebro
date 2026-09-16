use scip::types::PositionEncoding;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub start: Pos,
    pub end: Pos,
}

impl Span {
    /// SCIP ranges are `[line, start_col, end_col]` or
    /// `[start_line, start_col, end_line, end_col]`, end exclusive.
    pub fn from_scip(range: &[i32]) -> Option<Span> {
        let n = |i: usize| usize::try_from(range[i]).ok();
        match range.len() {
            3 => Some(Span {
                start: Pos {
                    line: n(0)?,
                    col: n(1)?,
                },
                end: Pos {
                    line: n(0)?,
                    col: n(2)?,
                },
            }),
            4 => Some(Span {
                start: Pos {
                    line: n(0)?,
                    col: n(1)?,
                },
                end: Pos {
                    line: n(2)?,
                    col: n(3)?,
                },
            }),
            _ => None,
        }
    }

    pub fn contains(&self, other: &Span) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    pub fn contains_pos(&self, pos: Pos) -> bool {
        self.start <= pos && pos < self.end
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColumnUnit {
    Utf8,
    Utf16,
    Utf32,
}

impl ColumnUnit {
    /// Indexers written before `position_encoding` existed leave it
    /// unspecified. The protocol cannot say what they meant by it, so
    /// `unspecified` is the answer the indexer's own adapter gives.
    pub fn resolve(encoding: PositionEncoding, unspecified: ColumnUnit) -> ColumnUnit {
        match encoding {
            PositionEncoding::UTF8CodeUnitOffsetFromLineStart => ColumnUnit::Utf8,
            PositionEncoding::UTF16CodeUnitOffsetFromLineStart => ColumnUnit::Utf16,
            PositionEncoding::UTF32CodeUnitOffsetFromLineStart => ColumnUnit::Utf32,
            PositionEncoding::UnspecifiedPositionEncoding => unspecified,
        }
    }
}

pub struct SourceFile {
    text: String,
    line_starts: Vec<usize>,
    unit: ColumnUnit,
    import_lines: Vec<bool>,
}

impl SourceFile {
    pub fn new(text: String, unit: ColumnUnit, import_lines: Vec<bool>) -> SourceFile {
        let mut line_starts = vec![0];
        line_starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        SourceFile {
            text,
            line_starts,
            unit,
            import_lines,
        }
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn line_count(&self) -> usize {
        self.text.lines().count()
    }

    pub fn is_import_line(&self, line: usize) -> bool {
        self.import_lines.get(line).copied().unwrap_or(false)
    }

    pub fn byte_offset(&self, pos: Pos) -> usize {
        let Some(&start) = self.line_starts.get(pos.line) else {
            return self.text.len();
        };
        let end = self
            .line_starts
            .get(pos.line + 1)
            .copied()
            .unwrap_or(self.text.len());
        let line = &self.text[start..end];
        let within = match self.unit {
            ColumnUnit::Utf8 => pos.col.min(line.len()),
            ColumnUnit::Utf16 => advance(line, pos.col, char::len_utf16),
            ColumnUnit::Utf32 => advance(line, pos.col, |_| 1),
        };
        start + within
    }
}

fn advance(line: &str, units: usize, width: impl Fn(char) -> usize) -> usize {
    let mut seen = 0;
    for (byte, ch) in line.char_indices() {
        if seen >= units {
            return byte;
        }
        seen += width(ch);
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_columns_land_after_multibyte_chars() {
        // "🚀" is 4 bytes, 2 UTF-16 units, 1 char.
        let text = "a🚀b\nc".to_string();
        let utf16 = SourceFile::new(text.clone(), ColumnUnit::Utf16, Vec::new());
        assert_eq!(utf16.byte_offset(Pos { line: 0, col: 3 }), 5);
        assert_eq!(utf16.byte_offset(Pos { line: 1, col: 0 }), 7);
        let utf32 = SourceFile::new(text.clone(), ColumnUnit::Utf32, Vec::new());
        assert_eq!(utf32.byte_offset(Pos { line: 0, col: 2 }), 5);
        let utf8 = SourceFile::new(text, ColumnUnit::Utf8, Vec::new());
        assert_eq!(utf8.byte_offset(Pos { line: 0, col: 5 }), 5);
        assert_eq!(utf8.byte_offset(Pos { line: 9, col: 0 }), 8);
    }
}
