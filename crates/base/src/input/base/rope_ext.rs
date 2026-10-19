use std::ops::Range;

use ropey::{LineType, Rope, RopeSlice};
use sum_tree::Bias;
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};

/// Parser-independent byte/row/column position used for incremental edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Point {
    pub row: usize,
    pub column: usize,
}

impl Point {
    pub fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }
}

/// Parser-independent description of an incremental text replacement.
#[derive(Debug, Clone, Copy)]
pub struct InputEdit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub start_position: Point,
    pub old_end_position: Point,
    pub new_end_position: Point,
}

use lsp_types::Position;

/// An iterator over the lines of a `Rope`.
pub struct RopeLines<'a> {
    rope: &'a Rope,
    row: usize,
    end_row: usize,
}

impl<'a> RopeLines<'a> {
    /// Create a new `RopeLines` iterator.
    pub fn new(rope: &'a Rope) -> Self {
        let end_row = rope.lines_len();
        Self {
            row: 0,
            end_row,
            rope,
        }
    }
}
impl<'a> Iterator for RopeLines<'a> {
    type Item = RopeSlice<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.row >= self.end_row {
            return None;
        }

        let line = self.rope.slice_line(self.row);
        self.row += 1;
        Some(line)
    }

    #[inline]
    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.row = self.row.saturating_add(n);
        self.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.end_row - self.row;
        (len, Some(len))
    }
}

impl std::iter::ExactSizeIterator for RopeLines<'_> {}
impl std::iter::FusedIterator for RopeLines<'_> {}

/// An extension trait for [`Rope`] to provide additional utility methods.
pub trait RopeExt {
    /// Start offset of the line at the given row (0-based) index.
    ///
    /// # Example
    ///
    /// ```
    /// use gpui_base::input::{Rope, RopeExt};
    ///
    /// let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
    /// assert_eq!(rope.line_start_offset(0), 0);
    /// assert_eq!(rope.line_start_offset(1), 6);
    /// ```
    fn line_start_offset(&self, row: usize) -> usize;

    /// The end offset of the line at the given row (0-based) index: where its
    /// terminator begins, the `\n` or the `\r` of a `\r\n`.
    ///
    /// Return the end of the rope if the row is out of bounds.
    ///
    /// ```
    /// use gpui_base::input::{Rope, RopeExt};
    /// let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
    /// assert_eq!(rope.line_end_offset(0), 5); // "Hello"
    /// assert_eq!(rope.line_end_offset(1), 11); // "World"
    /// ```
    fn line_end_offset(&self, row: usize) -> usize;

    /// Return a line slice at the given row (0-based) index, without its
    /// terminator: neither the `\n` nor the `\r` before it. A `\r` that no
    /// `\n` follows is text.
    ///
    /// ```
    /// use gpui_base::input::{Rope, RopeExt};
    /// let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
    /// assert_eq!(rope.slice_line(0).to_string(), "Hello");
    /// assert_eq!(rope.slice_line(1).to_string(), "World");
    /// assert_eq!(rope.slice_line(2).to_string(), "This is a test 中文");
    /// assert_eq!(rope.slice_line(6).to_string(), ""); // out of bounds
    /// ```
    fn slice_line(&self, row: usize) -> RopeSlice<'_>;

    /// Return a slice of rows in the given range (0-based, end exclusive).
    ///
    /// If the range is out of bounds, it will be clamped to the valid range.
    ///
    /// ```
    /// use gpui_base::input::{Rope, RopeExt};
    /// let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
    /// assert_eq!(rope.slice_lines(0..2).to_string(), "Hello\nWorld");
    /// assert_eq!(rope.slice_lines(1..3).to_string(), "World\r\nThis is a test 中文");
    /// assert_eq!(rope.slice_lines(2..5).to_string(), "This is a test 中文\nRope");
    /// assert_eq!(rope.slice_lines(3..10).to_string(), "Rope");
    /// assert_eq!(rope.slice_lines(5..10).to_string(), ""); // out of bounds
    /// ```
    fn slice_lines(&self, rows_range: Range<usize>) -> RopeSlice<'_>;

    /// Return an iterator over all lines in the rope.
    ///
    /// Each line slice is without its terminator, `\n` or `\r\n`.
    ///
    /// ```
    /// use gpui_base::input::{Rope, RopeExt};
    /// let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
    /// let lines: Vec<_> = rope.iter_lines().map(|r| r.to_string()).collect();
    /// assert_eq!(lines, vec!["Hello", "World", "This is a test 中文", "Rope"]);
    /// ```
    fn iter_lines(&self) -> RopeLines<'_>;

    /// Return the number of lines in the rope.
    ///
    /// ```
    /// use gpui_base::input::{Rope, RopeExt};
    /// let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
    /// assert_eq!(rope.lines_len(), 4);
    /// ```
    fn lines_len(&self) -> usize;

    /// Return the length of the row (0-based) in bytes, without its terminator.
    ///
    /// If the row is out of bounds, return 0.
    ///
    /// ```
    /// use gpui_base::input::{Rope, RopeExt};
    /// let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
    /// assert_eq!(rope.line_len(0), 5); // "Hello"
    /// assert_eq!(rope.line_len(1), 5); // "World"
    /// assert_eq!(rope.line_len(2), 21); // "This is a test 中文"
    /// assert_eq!(rope.line_len(4), 0); // out of bounds
    /// ```
    fn line_len(&self, row: usize) -> usize;

    /// Replace the text in the given byte range with new text.
    ///
    /// # Panics
    ///
    /// - If the range is not on char boundary.
    /// - If the range is out of bounds.
    ///
    /// ```
    /// use gpui_base::input::{Rope, RopeExt};
    /// let mut rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
    /// rope.replace(6..11, "Universe");
    /// assert_eq!(rope.to_string(), "Hello\nUniverse\r\nThis is a test 中文\nRope");
    /// ```
    fn replace(&mut self, range: Range<usize>, new_text: &str);

    /// Get char at the given offset (byte).
    ///
    /// - If the offset is in the middle of a multi-byte character, return None.
    /// - If the offset is out of bounds, return None.
    fn char_at(&self, offset: usize) -> Option<char>;

    /// Get the byte offset from the given line, column [`Position`] (0-based).
    ///
    /// The column is in characters.
    fn position_to_offset(&self, line_col: &Position) -> usize;

    /// Get the line, column [`Position`] (0-based) from the given byte offset.
    ///
    /// The column is in characters.
    fn offset_to_position(&self, offset: usize) -> Position;

    /// Get point (row, column) from the given byte offset.
    ///
    /// The column is in bytes.
    fn offset_to_point(&self, offset: usize) -> Point;

    /// Get byte offset from the given point (row, column).
    ///
    /// The column is 0-based in bytes.
    fn point_to_offset(&self, point: Point) -> usize;

    /// Get the word byte range at the given byte offset (0-based).
    fn word_range(&self, offset: usize) -> Option<Range<usize>>;

    /// Get word at the given byte offset (0-based).
    fn word_at(&self, offset: usize) -> String;

    /// Convert offset in UTF-16 to byte offset (0-based).
    ///
    /// Runs in O(log N) time.
    fn offset_utf16_to_offset(&self, offset_utf16: usize) -> usize;

    /// Convert byte offset (0-based) to offset in UTF-16.
    ///
    /// Runs in O(log N) time.
    fn offset_to_offset_utf16(&self, offset: usize) -> usize;

    /// Get a clipped offset (avoid in a char boundary).
    ///
    /// - If Bias::Left and inside the char boundary, return the ix - 1;
    /// - If Bias::Right and in inside char boundary, return the ix + 1;
    /// - Otherwise return the ix.
    ///
    /// ```
    /// use gpui_base::input::{Rope, RopeExt};
    /// use sum_tree::Bias;
    ///
    /// let rope = Rope::from("Hello 中文🎉 test\nRope");
    /// assert_eq!(rope.clip_offset(5, Bias::Left), 5);
    /// // Inside multi-byte character '中' (3 bytes)
    /// assert_eq!(rope.clip_offset(7, Bias::Left), 6);
    /// assert_eq!(rope.clip_offset(7, Bias::Right), 9);
    /// ```
    fn clip_offset(&self, offset: usize, bias: Bias) -> usize;

    /// The byte offset of the grapheme boundary before `offset`, or 0: one
    /// step back over what is drawn as one character, an emoji of several,
    /// a flag, a letter and the accents after it, a `\r\n`. An offset
    /// inside a character steps back from that character's end.
    ///
    /// Runs in O(log N) time, and the length of the grapheme.
    ///
    /// ```
    /// use gpui_base::input::{Rope, RopeExt};
    ///
    /// let rope = Rope::from("a👨‍👩‍👧🇳🇴e\u{301}\r\n");
    /// assert_eq!(rope.grapheme_before(rope.len()), rope.len() - 2);
    /// assert_eq!(rope.grapheme_before(rope.len() - 2), rope.len() - 5);
    /// assert_eq!(rope.grapheme_before("a👨‍👩‍👧🇳🇴".len()), "a👨‍👩‍👧".len());
    /// assert_eq!(rope.grapheme_before("a👨‍👩‍👧".len()), 1);
    /// assert_eq!(rope.grapheme_before(1), 0);
    /// assert_eq!(rope.grapheme_before(0), 0);
    /// ```
    fn grapheme_before(&self, offset: usize) -> usize;

    /// The byte offset of the grapheme boundary after `offset`, or the
    /// length: one step on over what is drawn as one character, as
    /// [`RopeExt::grapheme_before`] steps back. An offset inside a
    /// character steps on from that character's start.
    ///
    /// Runs in O(log N) time, and the length of the grapheme.
    ///
    /// ```
    /// use gpui_base::input::{Rope, RopeExt};
    ///
    /// let rope = Rope::from("a👨‍👩‍👧🇳🇴e\u{301}\r\n");
    /// assert_eq!(rope.grapheme_after(0), 1);
    /// assert_eq!(rope.grapheme_after(1), "a👨‍👩‍👧".len());
    /// assert_eq!(rope.grapheme_after("a👨‍👩‍👧".len()), "a👨‍👩‍👧🇳🇴".len());
    /// assert_eq!(rope.grapheme_after(rope.len() - 2), rope.len());
    /// assert_eq!(rope.grapheme_after(rope.len()), rope.len());
    /// ```
    fn grapheme_after(&self, offset: usize) -> usize;

    /// Convert offset in characters to byte offset (0-based).
    ///
    /// Run in O(n) time.
    ///
    /// # Example
    ///
    /// ```
    /// use gpui_base::input::{Rope, RopeExt};
    /// let rope = Rope::from("a 中文🎉 test\nRope");
    /// assert_eq!(rope.char_index_to_offset(0), 0);
    /// assert_eq!(rope.char_index_to_offset(1), 1);
    /// assert_eq!(rope.char_index_to_offset(3), "a 中".len());
    /// assert_eq!(rope.char_index_to_offset(5), "a 中文🎉".len());
    /// ```
    fn char_index_to_offset(&self, char_index: usize) -> usize;

    /// Convert byte offset (0-based) to offset in characters.
    ///
    /// Run in O(n) time.
    ///
    /// # Example
    ///
    /// ```
    /// use gpui_base::input::{Rope, RopeExt};
    /// let rope = Rope::from("a 中文🎉 test\nRope");
    /// assert_eq!(rope.offset_to_char_index(0), 0);
    /// assert_eq!(rope.offset_to_char_index(1), 1);
    /// assert_eq!(rope.offset_to_char_index(3), 3);
    /// assert_eq!(rope.offset_to_char_index(4), 3);
    /// ```
    fn offset_to_char_index(&self, offset: usize) -> usize;
}

impl RopeExt for Rope {
    fn slice_line(&self, row: usize) -> RopeSlice<'_> {
        let total_lines = self.lines_len();
        if row >= total_lines {
            return self.slice(0..0);
        }

        let line = self.line(row, LineType::LF);
        // The terminator is not part of the line: the `\n`, and the `\r`
        // before it in a CRLF file. Both are single bytes, so a byte
        // comparison needs no char boundary. The caret works in these
        // offsets, and an editor whose line ended after the `\r` put what
        // was typed at its end between the `\r` and the `\n`.
        let mut end = line.len();
        if end > 0 && line.byte(end - 1) == b'\n' {
            end -= 1;
            if end > 0 && line.byte(end - 1) == b'\r' {
                end -= 1;
            }
        }

        line.slice(..end)
    }

    fn slice_lines(&self, rows_range: Range<usize>) -> RopeSlice<'_> {
        let start = self.line_start_offset(rows_range.start);
        let end = self.line_end_offset(rows_range.end.saturating_sub(1));
        self.slice(start..end)
    }

    fn iter_lines(&self) -> RopeLines<'_> {
        RopeLines::new(&self)
    }

    fn line_len(&self, row: usize) -> usize {
        self.slice_line(row).len()
    }

    fn line_start_offset(&self, row: usize) -> usize {
        self.point_to_offset(Point::new(row, 0))
    }

    fn offset_to_point(&self, offset: usize) -> Point {
        let offset = self.clip_offset(offset, Bias::Left);
        let row = self.byte_to_line_idx(offset, LineType::LF);
        let line_start = self.line_to_byte_idx(row, LineType::LF);
        let column = offset.saturating_sub(line_start);
        Point::new(row, column)
    }

    fn point_to_offset(&self, point: Point) -> usize {
        if point.row >= self.lines_len() {
            return self.len();
        }

        let line_start = self.line_to_byte_idx(point.row, LineType::LF);
        line_start + point.column
    }

    fn position_to_offset(&self, pos: &Position) -> usize {
        let line = self.slice_line(pos.line as usize);
        // Clamp out-of-range columns, then use Ropey's index to avoid rescanning long lines.
        self.line_start_offset(pos.line as usize)
            + line.char_to_byte_idx((pos.character as usize).min(line.len_chars()))
    }

    fn offset_to_position(&self, offset: usize) -> Position {
        let point = self.offset_to_point(offset);
        let line = self.slice_line(point.row);
        // an offset inside the terminator is the end of the line
        let column = point.column.min(line.len());
        let offset = line.utf16_to_byte_idx(line.byte_to_utf16_idx(column));
        let character = line.slice(..offset).chars().count();
        Position::new(point.row as u32, character as u32)
    }

    fn line_end_offset(&self, row: usize) -> usize {
        if row > self.lines_len() {
            return self.len();
        }

        self.line_start_offset(row) + self.line_len(row)
    }

    fn lines_len(&self) -> usize {
        self.len_lines(LineType::LF)
    }

    fn char_at(&self, offset: usize) -> Option<char> {
        if offset > self.len() {
            return None;
        }

        self.get_char(offset).ok()
    }

    fn word_range(&self, offset: usize) -> Option<Range<usize>> {
        if offset >= self.len() {
            return None;
        }

        let mut left = String::new();
        let offset = self.clip_offset(offset, Bias::Left);
        for c in self.chars_at(offset).reversed() {
            if c.is_alphanumeric() || c == '_' {
                left.insert(0, c);
            } else {
                break;
            }
        }
        let start = offset.saturating_sub(left.len());

        let right = self
            .chars_at(offset)
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>();

        let end = offset + right.len();

        if start == end { None } else { Some(start..end) }
    }

    fn word_at(&self, offset: usize) -> String {
        if let Some(range) = self.word_range(offset) {
            self.slice(range).to_string()
        } else {
            String::new()
        }
    }

    #[inline]
    fn offset_utf16_to_offset(&self, offset_utf16: usize) -> usize {
        if offset_utf16 > self.len_utf16() {
            return self.len();
        }

        self.utf16_to_byte_idx(offset_utf16)
    }

    #[inline]
    fn offset_to_offset_utf16(&self, offset: usize) -> usize {
        if offset > self.len() {
            return self.len_utf16();
        }

        self.byte_to_utf16_idx(offset)
    }

    fn replace(&mut self, range: Range<usize>, new_text: &str) {
        let range =
            self.clip_offset(range.start, Bias::Left)..self.clip_offset(range.end, Bias::Right);
        self.remove(range.clone());
        self.insert(range.start, new_text);
    }

    fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        if offset > self.len() {
            return self.len();
        }

        if self.is_char_boundary(offset) {
            return offset;
        }

        if bias == Bias::Left {
            self.floor_char_boundary(offset)
        } else {
            self.ceil_char_boundary(offset)
        }
    }

    fn grapheme_before(&self, offset: usize) -> usize {
        let offset = self.clip_offset(offset, Bias::Right);
        if offset == 0 {
            return 0;
        }
        let mut cursor = GraphemeCursor::new(offset, self.len(), true);
        // the chunk the character before the offset is in
        let (mut chunk, mut chunk_start) = self.chunk(offset - 1);
        loop {
            match cursor.prev_boundary(chunk, chunk_start) {
                Ok(boundary) => return boundary.unwrap_or(0),
                Err(GraphemeIncomplete::PrevChunk) => (chunk, chunk_start) = self.chunk(chunk_start - 1),
                Err(GraphemeIncomplete::PreContext(end)) => {
                    let (context, start) = self.chunk(end - 1);
                    cursor.provide_context(&context[..end - start], start);
                }
                // the cursor asks for nothing else going back: a character back
                Err(_) => return self.clip_offset(offset - 1, Bias::Left),
            }
        }
    }

    fn grapheme_after(&self, offset: usize) -> usize {
        let len = self.len();
        let offset = self.clip_offset(offset, Bias::Left);
        if offset >= len {
            return len;
        }
        let mut cursor = GraphemeCursor::new(offset, len, true);
        let (mut chunk, mut chunk_start) = self.chunk(offset);
        loop {
            match cursor.next_boundary(chunk, chunk_start) {
                Ok(boundary) => return boundary.unwrap_or(len),
                Err(GraphemeIncomplete::NextChunk) => {
                    (chunk, chunk_start) = self.chunk(chunk_start + chunk.len())
                }
                Err(GraphemeIncomplete::PreContext(end)) => {
                    let (context, start) = self.chunk(end - 1);
                    cursor.provide_context(&context[..end - start], start);
                }
                // the cursor asks for nothing else going on: a character on
                Err(_) => return self.clip_offset(offset + 1, Bias::Right),
            }
        }
    }

    fn char_index_to_offset(&self, char_offset: usize) -> usize {
        self.chars().take(char_offset).map(|c| c.len_utf8()).sum()
    }

    fn offset_to_char_index(&self, offset: usize) -> usize {
        let offset = self.clip_offset(offset, Bias::Right);
        self.slice(..offset).chars().count()
    }
}

#[cfg(test)]
mod tests {
    use super::Point;
    use ropey::Rope;
    use sum_tree::Bias;

    use crate::input::{Position, RopeExt};

    /// Every grapheme boundary of a text many chunks long, where a family,
    /// flags, a run of flags longer than a chunk, accents, a conjunct and
    /// `\r\n` fall across the chunks' edges, is where `grapheme_before`
    /// and `grapheme_after` step to, as unicode-segmentation finds them in
    /// the whole string.
    #[test]
    fn test_grapheme_steps_across_chunks() {
        use unicode_segmentation::UnicodeSegmentation;

        let parts = [
            "a",
            "👨\u{200D}👩\u{200D}👧",
            "🇳🇴🇸🇪",
            "e\u{301}\u{302}",
            "\r\n",
            "\u{915}\u{94D}\u{937}",
            "中",
        ];
        let mut text: String = (0..700).map(|i| parts[i % parts.len()]).collect();
        text.push_str(&"🇳🇴".repeat(400));
        text.push('z');
        let rope = Rope::from(text.as_str());
        assert!(rope.chunks().count() > 8, "the text is many chunks");
        let boundaries: Vec<usize> = text
            .grapheme_indices(true)
            .map(|(ix, _)| ix)
            .chain([text.len()])
            .collect();
        for pair in boundaries.windows(2) {
            assert_eq!(rope.grapheme_after(pair[0]), pair[1], "after {}", pair[0]);
            assert_eq!(rope.grapheme_before(pair[1]), pair[0], "before {}", pair[1]);
        }
        assert_eq!(rope.grapheme_before(0), 0);
        assert_eq!(rope.grapheme_after(text.len()), text.len());
        assert_eq!(rope.grapheme_after(text.len() + 3), text.len());

        // an offset inside a character steps from that character's edge
        let rope = Rope::from("a中b");
        assert_eq!(rope.grapheme_before(2), 1);
        assert_eq!(rope.grapheme_after(2), 4);
        assert_eq!(Rope::from("").grapheme_before(0), 0);
        assert_eq!(Rope::from("").grapheme_after(0), 0);
    }

    #[test]
    fn test_slice_line() {
        let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
        assert_eq!(rope.slice_line(0).to_string(), "Hello");
        assert_eq!(rope.slice_line(1).to_string(), "World");
        assert_eq!(rope.slice_line(2).to_string(), "This is a test 中文");
        assert_eq!(rope.slice_line(3).to_string(), "Rope");

        // over bounds
        assert_eq!(rope.slice_line(6).to_string(), "");

        // only have \r end
        let rope = Rope::from("Hello\r");
        assert_eq!(rope.slice_line(0).to_string(), "Hello\r");
        assert_eq!(rope.slice_line(1).to_string(), "");

        // a \r before the \n is the terminator's, a \r anywhere else is text
        let rope = Rope::from("a\rb\r\n\r\n\r");
        assert_eq!(rope.slice_line(0).to_string(), "a\rb");
        assert_eq!(rope.slice_line(1).to_string(), "");
        assert_eq!(rope.slice_line(2).to_string(), "\r");
        assert_eq!(rope.line_len(0), 3);
        assert_eq!(rope.line_end_offset(0), 3);
        assert_eq!(rope.line_end_offset(1), 5);
    }

    #[test]
    fn test_lines_len() {
        let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
        assert_eq!(rope.lines_len(), 4);
        let rope = Rope::from("");
        assert_eq!(rope.lines_len(), 1);
        let rope = Rope::from("Single line");
        assert_eq!(rope.lines_len(), 1);

        // only have \r end
        let rope = Rope::from("Hello\r");
        assert_eq!(rope.lines_len(), 1);
    }

    #[test]
    fn test_lines() {
        let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope\r");
        let lines: Vec<_> = rope.iter_lines().map(|r| r.to_string()).collect();
        assert_eq!(
            lines,
            vec!["Hello", "World", "This is a test 中文", "Rope\r"]
        );
    }

    #[test]
    fn test_eq() {
        let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
        assert!(rope.eq(&Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope")));
        assert!(!rope.eq(&Rope::from("Hello\nWorld")));

        let rope1 = rope.clone();
        assert!(rope.eq(&rope1));
    }

    #[test]
    fn test_iter_lines() {
        let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
        let lines: Vec<_> = rope
            .iter_lines()
            .skip(1)
            .take(2)
            .map(|r| r.to_string())
            .collect();
        assert_eq!(lines, vec!["World", "This is a test 中文"]);
    }

    #[test]
    fn test_line_start_end_offset() {
        let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
        assert_eq!(rope.line_start_offset(0), 0);
        assert_eq!(rope.line_end_offset(0), 5);

        assert_eq!(rope.line_start_offset(1), 6);
        assert_eq!(rope.line_end_offset(1), 11);

        assert_eq!(rope.line_start_offset(2), 13);
        assert_eq!(rope.line_end_offset(2), 34);

        assert_eq!(rope.line_start_offset(3), 35);
        assert_eq!(rope.line_end_offset(3), 39);

        assert_eq!(rope.line_start_offset(4), 39);
        assert_eq!(rope.line_end_offset(4), 39);
    }

    #[test]
    fn test_line_column() {
        let rope = Rope::from("a 中文🎉 test\nRope");
        assert_eq!(rope.position_to_offset(&Position::new(0, 3)), "a 中".len());
        assert_eq!(
            rope.position_to_offset(&Position::new(0, 5)),
            "a 中文🎉".len()
        );
        assert_eq!(
            rope.position_to_offset(&Position::new(1, 1)),
            "a 中文🎉 test\nR".len()
        );
        assert_eq!(
            rope.position_to_offset(&Position::new(0, u32::MAX)),
            "a 中文🎉 test".len()
        );

        assert_eq!(
            rope.offset_to_position("a 中文🎉 test\nR".len()),
            Position::new(1, 1)
        );
        assert_eq!(
            rope.offset_to_position("a 中文🎉".len()),
            Position::new(0, 5)
        );
    }

    #[test]
    fn test_offset_inside_a_terminator_is_the_end_of_its_line() {
        let rope = Rope::from("ab\r\ncd");
        assert_eq!(rope.offset_to_position(2), Position::new(0, 2));
        assert_eq!(rope.offset_to_position(3), Position::new(0, 2), "between the \\r and the \\n");
        assert_eq!(rope.offset_to_position(4), Position::new(1, 0));
        assert_eq!(rope.offset_to_position(6), Position::new(1, 2));
    }

    #[test]
    fn test_offset_to_point() {
        let rope = Rope::from("a 中文🎉 test\nRope");
        assert_eq!(rope.offset_to_point(0), Point::new(0, 0));
        assert_eq!(rope.offset_to_point(1), Point::new(0, 1));
        assert_eq!(rope.offset_to_point("a 中".len()), Point::new(0, 5));
        assert_eq!(rope.offset_to_point("a 中文🎉".len()), Point::new(0, 12));
        assert_eq!(
            rope.offset_to_point("a 中文🎉 test\nR".len()),
            Point::new(1, 1)
        );
    }

    #[test]
    fn test_point_to_offset() {
        let rope = Rope::from("a 中文🎉 test\nRope");
        assert_eq!(rope.point_to_offset(Point::new(0, 0)), 0);
        assert_eq!(rope.point_to_offset(Point::new(0, 1)), 1);
        assert_eq!(rope.point_to_offset(Point::new(0, 5)), "a 中".len());
        assert_eq!(rope.point_to_offset(Point::new(0, 12)), "a 中文🎉".len());
        assert_eq!(
            rope.point_to_offset(Point::new(1, 1)),
            "a 中文🎉 test\nR".len()
        );
    }

    #[test]
    fn test_char_at() {
        let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文🎉\nRope");
        assert_eq!(rope.char_at(0), Some('H'));
        assert_eq!(rope.char_at(5), Some('\n'));
        assert_eq!(rope.char_at(13), Some('T'));
        assert_eq!(rope.char_at(28), Some('中'));
        assert_eq!(rope.char_at(34), Some('🎉'));
        assert_eq!(rope.char_at(38), Some('\n'));
        assert_eq!(rope.char_at(50), None);
        assert_eq!(rope.char_at(29), None, "inside a multi-byte character");
    }

    #[test]
    fn test_word_at() {
        let rope = Rope::from("Hello\nWorld\r\nThis is a test 中文 世界\nRope");
        assert_eq!(rope.word_at(0), "Hello");
        assert_eq!(rope.word_range(0), Some(0..5));
        assert_eq!(rope.word_at(8), "World");
        assert_eq!(rope.word_range(8), Some(6..11));
        assert_eq!(rope.word_at(12), "");
        assert_eq!(rope.word_range(12), None);
        assert_eq!(rope.word_at(13), "This");
        assert_eq!(rope.word_range(13), Some(13..17));
        assert_eq!(rope.word_at(31), "中文");
        assert_eq!(rope.word_range(31), Some(28..34));
        assert_eq!(rope.word_at(38), "世界");
        assert_eq!(rope.word_range(38), Some(35..41));
        assert_eq!(rope.word_at(44), "Rope");
        assert_eq!(rope.word_range(44), Some(42..46));
        assert_eq!(rope.word_at(45), "Rope");
    }

    #[test]
    fn test_offset_utf16_conversion() {
        let rope = Rope::from("hello 中文🎉 test\nRope");
        assert_eq!(rope.offset_to_offset_utf16("hello".len()), 5);
        assert_eq!(rope.offset_to_offset_utf16("hello 中".len()), 7);
        assert_eq!(rope.offset_to_offset_utf16("hello 中文".len()), 8);
        assert_eq!(rope.offset_to_offset_utf16("hello 中文🎉".len()), 10);
        assert_eq!(rope.offset_to_offset_utf16(100), 20);

        assert_eq!(rope.offset_utf16_to_offset(5), "hello".len());
        assert_eq!(rope.offset_utf16_to_offset(7), "hello 中".len());
        assert_eq!(rope.offset_utf16_to_offset(8), "hello 中文".len());
        assert_eq!(rope.offset_utf16_to_offset(10), "hello 中文🎉".len());
        assert_eq!(rope.offset_utf16_to_offset(100), rope.len());
    }

    #[test]
    fn test_replace() {
        let mut rope = Rope::from("Hello\nWorld\r\nThis is a test 中文\nRope");
        rope.replace(6..11, "Universe");
        assert_eq!(
            rope.to_string(),
            "Hello\nUniverse\r\nThis is a test 中文\nRope"
        );

        rope.replace(0..5, "Hi");
        assert_eq!(
            rope.to_string(),
            "Hi\nUniverse\r\nThis is a test 中文\nRope"
        );

        rope.replace(rope.len() - 4..rope.len(), "String");
        assert_eq!(
            rope.to_string(),
            "Hi\nUniverse\r\nThis is a test 中文\nString"
        );

        // Test for not on a char boundary
        let mut rope = Rope::from("中文");
        rope.replace(0..1, "New");
        // autocorrect-disable
        assert_eq!(rope.to_string(), "New文");
        let mut rope = Rope::from("中文");
        rope.replace(0..2, "New");
        assert_eq!(rope.to_string(), "New文");
        let mut rope = Rope::from("中文");
        rope.replace(0..3, "New");
        assert_eq!(rope.to_string(), "New文");
        // autocorrect-enable
        let mut rope = Rope::from("中文");
        rope.replace(1..4, "New");
        assert_eq!(rope.to_string(), "New");
    }

    #[test]
    fn test_clip_offset() {
        let rope = Rope::from("Hello 中文🎉 test\nRope");
        // Inside multi-byte character '中' (3 bytes)
        assert_eq!(rope.clip_offset(5, Bias::Left), 5);
        assert_eq!(rope.clip_offset(7, Bias::Left), 6);
        assert_eq!(rope.clip_offset(7, Bias::Right), 9);
        assert_eq!(rope.clip_offset(9, Bias::Left), 9);

        // Inside multi-byte character '🎉' (4 bytes)
        assert_eq!(rope.clip_offset(13, Bias::Left), 12);
        assert_eq!(rope.clip_offset(13, Bias::Right), 16);
        assert_eq!(rope.clip_offset(16, Bias::Left), 16);

        // At character boundary
        assert_eq!(rope.clip_offset(5, Bias::Left), 5);
        assert_eq!(rope.clip_offset(5, Bias::Right), 5);

        // Out of bounds
        assert_eq!(rope.clip_offset(26, Bias::Left), 26);
        assert_eq!(rope.clip_offset(100, Bias::Left), 26);
    }

    #[test]
    fn test_char_index_to_offset() {
        let rope = Rope::from("a 中文🎉 test\nRope");
        assert_eq!(rope.char_index_to_offset(0), 0);
        assert_eq!(rope.char_index_to_offset(1), 1);
        assert_eq!(rope.char_index_to_offset(3), "a 中".len());
        assert_eq!(rope.char_index_to_offset(5), "a 中文🎉".len());
        assert_eq!(rope.char_index_to_offset(6), "a 中文🎉 ".len());

        assert_eq!(rope.offset_to_char_index(0), 0);
        assert_eq!(rope.offset_to_char_index(1), 1);
        assert_eq!(rope.offset_to_char_index(3), 3);
        assert_eq!(rope.offset_to_char_index(4), 3);
        assert_eq!(rope.offset_to_char_index(5), 3);
        assert_eq!(rope.offset_to_char_index(6), 4);
        assert_eq!(rope.offset_to_char_index(10), 5);
    }
}
