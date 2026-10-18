use crate::input::InputModeKind;
use std::ops::Range;

use gpui::{Context, Window};
use ropey::Rope;
use sum_tree::Bias;

use super::{InputBaseState, RopeExt as _};
use crate::text_boundary::{word_range_before_line_end, word_range_from_chars};

impl<M: InputModeKind> InputBaseState<M> {
    /// Select the word at the given offset on double-click.
    ///
    /// The offset is the UTF-8 offset.
    pub(super) fn select_word(&mut self, offset: usize, _: &mut Window, cx: &mut Context<Self>) {
        // A masked value renders as one unbroken run of mask characters, so it
        // has no word boundaries to select by. Take all of it instead, rather
        // than let the selection highlight reveal where the words are.
        let range = if self.masked {
            0..self.text.len()
        } else {
            let Some(range) = TextSelector::word_range(&self.text, offset) else {
                return;
            };
            range
        };

        self.undo_manager.break_transaction_coalescing();
        self.selected_range = (range.start..range.end).into();
        self.selected_word_range = Some(self.selected_range);
        cx.notify()
    }

    /// Select the line at the given offset on triple-click.
    ///
    /// The offset is the UTF-8 offset.
    pub(super) fn select_line(&mut self, offset: usize, _: &mut Window, cx: &mut Context<Self>) {
        let range = TextSelector::line_range(&self.text, offset);
        self.undo_manager.break_transaction_coalescing();
        self.selected_range = (range.start..range.end).into();
        self.selected_word_range = None;
        cx.notify()
    }
}

struct TextSelector;
impl TextSelector {
    /// Select a line in the given text at the specified offset.
    ///
    /// The offset is the UTF-8 offset.
    ///
    /// Returns the start and end offsets of the selected line.
    pub(crate) fn line_range(text: &Rope, offset: usize) -> Range<usize> {
        let offset = text.clip_offset(offset, Bias::Left);
        let row = text.offset_to_point(offset).row;
        let start = text.line_start_offset(row);
        let end = text.line_end_offset(row);

        start..end
    }

    /// Select a word in the given text at the specified offset.
    ///
    /// The offset is the UTF-8 offset.
    ///
    /// Returns the start and end offsets of the selected word.
    /// A click past the end of a line lands on its line break, or past the
    /// end of the text on the last line, and takes the word the line ends
    /// in, or nothing (`word_range_before_line_end`), never the line break.
    pub(crate) fn word_range(text: &Rope, offset: usize) -> Option<Range<usize>> {
        let offset = text.clip_offset(offset, Bias::Left);
        let line_end = match text.char_at(offset) {
            None => Some(offset),
            // the caret never stands inside a `\r\n`, but a click may say so
            Some('\n') if offset > 0 && text.char_at(offset - 1) == Some('\r') => Some(offset - 1),
            Some('\n') => Some(offset),
            Some('\r') if text.char_at(offset + 1) == Some('\n') => Some(offset),
            _ => None,
        };
        if let Some(end) = line_end {
            return Some(word_range_before_line_end(end, text.chars_at(end).reversed()));
        }
        let Some(char) = text.char_at(offset) else {
            return None;
        };

        let end = offset + char.len_utf8();
        let prev_chars = text.chars_at(offset).reversed().take(128);
        let next_chars = text.chars_at(end).take(128);
        Some(word_range_from_chars(offset, char, prev_chars, next_chars))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    #[test]
    fn test_word_range() {
        use indoc::indoc;

        let rope = Rope::from(indoc! {
            r#"
            test text:
            abcde 中文🎉 test
            hello[()]
            test_connector ____
            Rope
            rök
            grande île
            "#
        });

        let tests = vec![
            (0, 0, Some("test")),
            (0, 4, Some(" ")),
            (1, 0, Some("abcde")),
            (1, 4, Some("abcde")),
            (1, 5, Some(" ")),
            (1, 6, Some("中")),
            (1, 9, Some("文")),
            (1, 13, Some("🎉")),
            (1, 20, Some("test")),
            (2, 5, Some("[")),
            (2, 6, Some("(")),
            (2, 7, Some(")")),
            (2, 8, Some("]")),
            (3, 5, Some("test_connector")),
            (3, 14, Some(" ")),
            (3, 16, Some("____")),
            (4, 0, Some("Rope")),
            (5, 0, Some("rök")),
            (6, 8, Some("île")),
        ];

        for (line, column, expected) in tests {
            let line_start_offset = rope.line_start_offset(line);
            let offset = line_start_offset + column;
            let range = TextSelector::word_range(&rope, offset);

            let actual = range.map(|r| rope.slice(r).to_string());
            let expect = expected.map(|s| s.to_string());
            assert_eq!(actual, expect, "line {}, column {}", line, column);
        }
    }

    #[test]
    fn test_line_range() {
        let rope = Rope::from("first line\nsecond line\nthird");
        let tests = vec![
            (0, 0, "first line"),
            (0, 5, "first line"),
            (1, 3, "second line"),
            (2, 1, "third"),
        ];

        for (line, column, expected) in tests {
            let line_start_offset = rope.line_start_offset(line);
            let offset = line_start_offset + column;
            let range = TextSelector::line_range(&rope, offset);

            let actual = rope.slice(range).to_string();
            assert_eq!(actual, expected, "line {}, column {}", line, column);
        }

        // a CRLF line ends before its \r, also to a click on the \r or the \n
        let rope = Rope::from("first\r\nsecond\r\nthird");
        for (offset, expected) in [(0, "first"), (5, "first"), (6, "first"), (10, "second"), (16, "third")] {
            let range = TextSelector::line_range(&rope, offset);
            assert_eq!(rope.slice(range).to_string(), expected, "offset {offset}");
        }
    }

    /// A double-click on the end of a CRLF line takes the word before it,
    /// never the pair nor half of it: the caret never stands between its
    /// bytes, so nor does a selection's edge.
    #[test]
    fn a_crlf_is_one_word_to_the_double_click() {
        let rope = Rope::from("ab\r\ncd\r\n\r\n");
        assert_eq!(TextSelector::word_range(&rope, 2), Some(0..2), "on the \\r");
        assert_eq!(TextSelector::word_range(&rope, 3), Some(0..2), "on the \\n");
        assert_eq!(TextSelector::word_range(&rope, 8), Some(8..8), "an empty line");
        assert_eq!(TextSelector::word_range(&rope, 1), Some(0..2), "the word before it");
        // a \r that no \n follows stands alone, as text; a \n is a line's end
        let rope = Rope::from("a\rb\n\rc");
        assert_eq!(TextSelector::word_range(&rope, 1), Some(1..2));
        assert_eq!(TextSelector::word_range(&rope, 3), Some(2..3));
        assert_eq!(TextSelector::word_range(&rope, 4), Some(4..5));
    }

    /// A double-click past the end of a line, which lands on its line
    /// break, takes the word the line ends in, or the whitespace, or
    /// nothing where it is empty or ends in punctuation; never the line
    /// break, which the next key typed then took, joining the line to the
    /// next (elundqvist/kvist#416). VS Code takes the word, the whitespace
    /// after the last word, or nothing.
    #[test]
    fn a_double_click_past_a_lines_end_takes_its_last_word_or_nothing() {
        let text = "let total = sum;\nfn main() {\n    call(x)\n\nhello world\ntrailing   \n  é_word\n中文\nlast";
        let rope = Rope::from(text);
        let end_of = |line: &str| text.find(&format!("{line}\n")).map_or(text.len(), |at| at + line.len());
        let taken = |line: &str| {
            let range = TextSelector::word_range(&rope, end_of(line)).expect("a range");
            assert!(!text[range.clone()].contains('\n'), "{line:?}: the line break taken");
            text[range].to_string()
        };
        assert_eq!(taken("hello world"), "world");
        assert_eq!(taken("trailing   "), "   ");
        assert_eq!(taken("  é_word"), "é_word");
        assert_eq!(taken("中文"), "文", "as a double-click on it takes it");
        // the end of the text, with no line break
        assert_eq!(taken("last"), "last");
        // punctuation, and an empty line: nothing, a caret at the end
        for line in ["let total = sum;", "fn main() {", "    call(x)"] {
            let end = end_of(line);
            assert_eq!(TextSelector::word_range(&rope, end), Some(end..end), "{line:?}");
        }
        let empty = text.find("\n\n").unwrap() + 1;
        assert_eq!(TextSelector::word_range(&rope, empty), Some(empty..empty));
        assert_eq!(TextSelector::word_range(&Rope::from(""), 0), Some(0..0));
        // a click on a word, or on the space between two, is as it was
        let at = text.find("hello").unwrap();
        assert_eq!(TextSelector::word_range(&rope, at + 2), Some(at..at + 5));
        assert_eq!(TextSelector::word_range(&rope, at + 5), Some(at + 5..at + 6));
    }
}
