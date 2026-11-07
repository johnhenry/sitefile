//! Lines and positions. LSP positions are 0-based lines and UTF-16 code-unit
//! columns (the default `positionEncoding`), which is also how
//! [`crate::error::SourceSpan`] counts columns (1-based), so a span converts by
//! subtracting one from each part.

use serde_json::{json, Value};

/// The document split into physical lines the way the scanner splits them
/// (`\n`, with a trailing `\r` removed).
#[derive(Debug, Clone)]
pub struct Lines {
    lines: Vec<String>,
}

impl Lines {
    pub fn new(text: &str) -> Lines {
        Lines {
            lines: text
                .split('\n')
                .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
                .collect(),
        }
    }
    pub fn len(&self) -> usize {
        self.lines.len()
    }
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
    /// Line `n` (0-based), or `""` past the end.
    pub fn get(&self, n: u32) -> &str {
        self.lines.get(n as usize).map(String::as_str).unwrap_or("")
    }
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(String::as_str)
    }
    /// Length of line `n` in UTF-16 units.
    pub fn width(&self, n: u32) -> u32 {
        utf16_len(self.get(n))
    }
}

pub fn utf16_len(s: &str) -> u32 {
    s.chars().map(|c| c.len_utf16() as u32).sum()
}

/// Byte offset of UTF-16 column `col` in `s` (clamped to the end; a column in
/// the middle of a surrogate pair rounds down to the character's start).
pub fn utf16_to_byte(s: &str, col: u32) -> usize {
    let mut acc = 0u32;
    for (i, c) in s.char_indices() {
        let w = c.len_utf16() as u32;
        if acc + w > col {
            return i;
        }
        acc += w;
    }
    s.len()
}

/// UTF-16 column of byte offset `byte` in `s`.
pub fn byte_to_utf16(s: &str, byte: usize) -> u32 {
    utf16_len(&s[..byte.min(s.len())])
}

/// Byte offset into a whole document of an LSP position (clamped).
pub fn offset_of(text: &str, line: u32, character: u32) -> usize {
    let mut start = 0usize;
    for _ in 0..line {
        match text[start..].find('\n') {
            Some(nl) => start += nl + 1,
            None => return text.len(),
        }
    }
    let end = text[start..]
        .find('\n')
        .map(|nl| start + nl)
        .unwrap_or(text.len());
    let line_text = &text[start..end];
    let line_text = line_text.strip_suffix('\r').unwrap_or(line_text);
    start + utf16_to_byte(line_text, character)
}

/// Apply one `TextDocumentContentChangeEvent` range edit.
pub fn apply_edit(text: &mut String, start: (u32, u32), end: (u32, u32), new_text: &str) {
    let a = offset_of(text, start.0, start.1);
    let b = offset_of(text, end.0, end.1).max(a);
    text.replace_range(a..b, new_text);
}

pub fn position(line: u32, character: u32) -> Value {
    json!({ "line": line, "character": character })
}

pub fn range(line: u32, start: u32, end_line: u32, end: u32) -> Value {
    json!({ "start": position(line, start), "end": position(end_line, end) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_columns_with_non_ascii() {
        // é: 1 unit (2 bytes); 😀: 2 units (4 bytes).
        let s = "a é😀 b";
        assert_eq!(utf16_len(s), 7);
        assert_eq!(utf16_to_byte(s, 2), 2); // é
        assert_eq!(utf16_to_byte(s, 3), 4); // 😀
        assert_eq!(utf16_to_byte(s, 4), 4); // inside the pair: rounds down
        assert_eq!(utf16_to_byte(s, 5), 8); // the space after 😀
        assert_eq!(utf16_to_byte(s, 99), s.len());
        assert_eq!(byte_to_utf16(s, 8), 5);
        assert_eq!(byte_to_utf16(s, 4), 3);
    }

    #[test]
    fn lines_strip_cr() {
        let l = Lines::new("FROM scratch\r\nCOPY é x\n");
        assert_eq!(l.len(), 3);
        assert_eq!(l.get(1), "COPY é x");
        assert_eq!(l.width(1), 8);
        assert_eq!(l.get(7), "");
    }

    #[test]
    fn incremental_edits() {
        let mut t = String::from("COPY 😀 /a\nEXPORT /\n");
        // Replace "/a" (UTF-16 columns 8..10 on line 0) with "/b.txt".
        apply_edit(&mut t, (0, 8), (0, 10), "/b.txt");
        assert_eq!(t, "COPY 😀 /b.txt\nEXPORT /\n");
        apply_edit(&mut t, (1, 0), (1, 0), "# x\n");
        assert_eq!(t, "COPY 😀 /b.txt\n# x\nEXPORT /\n");
        apply_edit(&mut t, (0, 14), (2, 0), "\n");
        assert_eq!(t, "COPY 😀 /b.txt\nEXPORT /\n");
        assert_eq!(offset_of("a\r\nb", 0, 5), 1);
        assert_eq!(offset_of("a\nb", 9, 0), 3);
    }
}
