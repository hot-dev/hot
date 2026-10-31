//! Positions.
//!
//! The server deals in three position systems:
//!
//! * LSP positions: a line and a UTF-16 code-unit column (the encoding every
//!   client supports, and the only one this server advertises);
//! * byte offsets into the document text, for slicing it;
//! * the Hot lexer's positions: char (Unicode scalar) offsets, and 1-based
//!   line and char columns in compiler locations.
//!
//! [`LineIndex`] converts between them. Every position that comes in from the
//! client goes through it before it touches the text, and every position that
//! goes out is built by it, so a column never indexes a string directly.

use tower_lsp_server::ls_types::{Position, Range};

/// Line starts of one text, for position conversion.
///
/// Lines end at `\n`; a `\r` before it belongs to no column a client can
/// address, and columns past the end of a line clamp to its end.
#[derive(Debug, Clone)]
pub struct LineIndex {
    /// Byte offset where each line starts.
    line_bytes: Vec<usize>,
    /// Char offset where each line starts.
    line_chars: Vec<usize>,
    /// Byte length of the text.
    len_bytes: usize,
    /// Char length of the text.
    len_chars: usize,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut line_bytes = vec![0];
        let mut line_chars = vec![0];
        let mut chars = 0;
        for (byte, ch) in text.char_indices() {
            chars += 1;
            if ch == '\n' {
                line_bytes.push(byte + 1);
                line_chars.push(chars);
            }
        }
        Self {
            line_bytes,
            line_chars,
            len_bytes: text.len(),
            len_chars: chars,
        }
    }

    pub fn line_count(&self) -> usize {
        self.line_bytes.len()
    }

    /// The text of a 0-based line, without its line terminator.
    pub fn line<'a>(&self, text: &'a str, line: usize) -> &'a str {
        let Some(&start) = self.line_bytes.get(line) else {
            return "";
        };
        let end = self
            .line_bytes
            .get(line + 1)
            .map(|next| next - 1)
            .unwrap_or(self.len_bytes);
        let line_text = &text[start..end.max(start)];
        line_text.strip_suffix('\r').unwrap_or(line_text)
    }

    /// The byte offset of an LSP position. A column past the end of the line
    /// is the end of the line, a line past the end is the end of the text,
    /// and a column inside a surrogate pair rounds down to the char.
    pub fn offset(&self, text: &str, position: Position) -> usize {
        let line = position.line as usize;
        let Some(&start) = self.line_bytes.get(line) else {
            return self.len_bytes;
        };
        let line_text = self.line(text, line);
        let mut units = 0usize;
        for (byte, ch) in line_text.char_indices() {
            let next = units + ch.len_utf16();
            if next > position.character as usize {
                return start + byte;
            }
            units = next;
        }
        start + line_text.len()
    }

    /// The LSP position of a byte offset. An offset inside a char is the
    /// position of that char.
    pub fn position(&self, text: &str, offset: usize) -> Position {
        let offset = offset.min(self.len_bytes);
        let line = match self.line_bytes.binary_search(&offset) {
            Ok(line) => line,
            Err(next) => next - 1,
        };
        let start = self.line_bytes[line];
        let mut end = offset;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let character: usize = text[start..end].chars().map(char::len_utf16).sum();
        Position::new(line as u32, character as u32)
    }

    pub fn range(&self, text: &str, start: usize, end: usize) -> Range {
        Range::new(self.position(text, start), self.position(text, end))
    }

    /// The byte offset of a char offset (the lexer's positions).
    pub fn char_to_byte(&self, text: &str, char_offset: usize) -> usize {
        if char_offset >= self.len_chars {
            return self.len_bytes;
        }
        let line = match self.line_chars.binary_search(&char_offset) {
            Ok(line) => line,
            Err(next) => next - 1,
        };
        let skip = char_offset - self.line_chars[line];
        let start = self.line_bytes[line];
        text[start..]
            .char_indices()
            .nth(skip)
            .map(|(byte, _)| start + byte)
            .unwrap_or(self.len_bytes)
    }

    /// The char offset of a byte offset (rounding down inside a char).
    pub fn byte_to_char(&self, text: &str, byte_offset: usize) -> usize {
        let mut byte_offset = byte_offset.min(self.len_bytes);
        while !text.is_char_boundary(byte_offset) {
            byte_offset -= 1;
        }
        let line = match self.line_bytes.binary_search(&byte_offset) {
            Ok(line) => line,
            Err(next) => next - 1,
        };
        self.line_chars[line] + text[self.line_bytes[line]..byte_offset].chars().count()
    }

    /// The char offset of an LSP position.
    pub fn char_offset(&self, text: &str, position: Position) -> usize {
        self.byte_to_char(text, self.offset(text, position))
    }

    /// The LSP range of `length` chars starting at a char offset.
    pub fn char_range(&self, text: &str, char_offset: usize, length: usize) -> Range {
        let start = self.char_to_byte(text, char_offset);
        let end = self.char_to_byte(text, char_offset + length);
        self.range(text, start, end)
    }

    /// The LSP range of a compiler location: a 1-based line, a 1-based char
    /// column and a length in chars. A zero length still covers one char,
    /// so the range is visible.
    pub fn hot_range(&self, text: &str, line: usize, column: usize, length: usize) -> Range {
        let line = line.max(1) - 1;
        if line >= self.line_count() {
            let end = self.position(text, self.len_bytes);
            return Range::new(end, end);
        }
        let line_start = self.line_chars[line];
        let line_len = self.line(text, line).chars().count();
        let column = (column.max(1) - 1).min(line_len);
        let start = line_start + column;
        let mut range = self.char_range(text, start, length.max(1));
        // A one-char range at the end of a line would cover the newline.
        if length == 0 && column == line_len {
            range.end = range.start;
        }
        range
    }
}

/// An open document: its text, the client's version of it, and its lines.
#[derive(Debug, Clone)]
pub struct Document {
    pub text: String,
    pub version: i32,
    pub lines: LineIndex,
}

impl Document {
    pub fn new(text: String, version: i32) -> Self {
        let lines = LineIndex::new(&text);
        Self {
            text,
            version,
            lines,
        }
    }

    /// The char offset of an LSP position.
    pub fn char_offset(&self, position: Position) -> usize {
        self.lines.char_offset(&self.text, position)
    }

    /// The LSP range of `length` chars at a char offset.
    pub fn char_range(&self, char_offset: usize, length: usize) -> Range {
        self.lines.char_range(&self.text, char_offset, length)
    }

    /// The range of the whole text.
    pub fn full_range(&self) -> Range {
        Range::new(
            Position::new(0, 0),
            self.lines.position(&self.text, self.text.len()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str) -> u32 {
        s.encode_utf16().count() as u32
    }

    #[test]
    fn utf16_columns_map_to_char_boundaries() {
        // é is 2 bytes and 1 UTF-16 unit, — is 3 bytes and 1 unit, 🎉 is 4
        // bytes and 2 units.
        let text = "a é — 🎉 z\nnext";
        let index = LineIndex::new(text);
        let z = text.find('z').unwrap();
        assert_eq!(index.offset(text, Position::new(0, utf16("a é — 🎉 "))), z);
        assert_eq!(
            index.position(text, z),
            Position::new(0, utf16("a é — 🎉 "))
        );
        // Inside the surrogate pair of 🎉: the emoji's own start.
        let emoji = text.find('🎉').unwrap();
        assert_eq!(
            index.offset(text, Position::new(0, utf16("a é — ") + 1)),
            emoji
        );
        // Past the end of the line: the end of the line, not the next line.
        assert_eq!(
            index.offset(text, Position::new(0, 400)),
            text.find('\n').unwrap()
        );
        // Past the last line: the end of the text.
        assert_eq!(index.offset(text, Position::new(9, 0)), text.len());
        // Every column of the line is a char boundary.
        for column in 0..40 {
            let offset = index.offset(text, Position::new(0, column));
            assert!(text.is_char_boundary(offset), "column {column}");
        }
    }

    #[test]
    fn char_offsets_round_trip() {
        let text = "héllo\n🎉x\r\ny";
        let index = LineIndex::new(text);
        for (char_offset, (byte, _)) in text.char_indices().enumerate() {
            assert_eq!(index.char_to_byte(text, char_offset), byte);
            assert_eq!(index.byte_to_char(text, byte), char_offset);
        }
        assert_eq!(index.char_to_byte(text, 999), text.len());
        // A CRLF line ends before the \r.
        assert_eq!(index.line(text, 1), "🎉x");
        assert_eq!(
            index.offset(text, Position::new(1, 99)),
            text.find('\r').unwrap()
        );
    }

    #[test]
    fn compiler_locations_become_utf16_ranges() {
        // The lexer counts chars: `nope-fn` starts at column 28 (1-based
        // char column) but at UTF-16 column 29, after the emoji.
        let line = "bad fn (): Int { add(\"日本🎉\", nope-fn(1)) }";
        let text = format!("::c ns\n{line}\n");
        let index = LineIndex::new(&text);
        let column = line[..line.find("nope-fn").unwrap()].chars().count() + 1;
        let range = index.hot_range(&text, 2, column, "nope-fn".len());
        assert_eq!(
            range.start,
            Position::new(1, utf16(&line[..line.find("nope-fn").unwrap()]))
        );
        assert_eq!(range.end.character - range.start.character, 7);
        // A location past the end clamps instead of panicking.
        let range = index.hot_range(&text, 99, 99, 3);
        assert_eq!(range.start, range.end);
        let range = index.hot_range(&text, 2, 999, 0);
        assert_eq!(range.start.line, 1);
    }
}
