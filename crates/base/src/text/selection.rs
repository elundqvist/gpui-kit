use std::ops::Range;

use crate::text_boundary::word_range_from_chars;

/// The word a double-click in a TextView takes, by the editor's own
/// boundaries (`text_boundary`), so a `\r\n` is one character here as it is
/// there: a double-click on the end of a CRLF line takes the pair, and a
/// copy of the selection never carries a lone `\r`.
pub(crate) fn word_range_at(text: &str, offset: usize) -> Option<Range<usize>> {
    if text.is_empty() {
        return None;
    }

    let offset = clip_offset(text, offset);
    let c = text[offset..].chars().next()?;
    Some(word_range_from_chars(
        offset,
        c,
        text[..offset].chars().rev(),
        text[offset + c.len_utf8()..].chars(),
    ))
}

fn clip_offset(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    if offset == text.len() {
        return text.char_indices().next_back().map_or(0, |(ix, _)| ix);
    }

    if text.is_char_boundary(offset) {
        offset
    } else {
        text.char_indices()
            .map(|(ix, _)| ix)
            .take_while(|ix| *ix < offset)
            .last()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_word_range_at() {
        let text =
            "test text\nabcde 中文🎉 test\nhello[()]\ntest_connector ____\nRope\nrök\ngrande île";
        let tests = [
            (0, Some("test")),
            (4, Some(" ")),
            (10, Some("abcde")),
            (15, Some(" ")),
            (16, Some("中")),
            (19, Some("文")),
            (22, Some("🎉")),
            (27, Some("test")),
            (37, Some("[")),
            (38, Some("(")),
            (39, Some(")")),
            (40, Some("]")),
            (42, Some("test_connector")),
            (56, Some(" ")),
            (57, Some("____")),
            (62, Some("Rope")),
            (67, Some("rök")),
            (79, Some("île")),
        ];

        for (offset, expected) in tests {
            let actual = word_range_at(text, offset).map(|range| text[range].to_string());
            assert_eq!(actual.as_deref(), expected, "offset {offset}");
        }
    }

    /// A `\r\n` is one character to the double-click, as it is to the
    /// editor's: on either byte it takes the pair, never the `\r` alone.
    #[test]
    fn a_crlf_is_one_word_to_the_double_click() {
        let text = "ab\r\ncd\r\n\r\nef";
        let at = |offset| word_range_at(text, offset);
        assert_eq!(at(2), Some(2..4), "on the \\r");
        assert_eq!(at(3), Some(2..4), "on the \\n");
        assert_eq!(at(8), Some(8..10), "an empty line");
        assert_eq!(at(1), Some(0..2), "the word before it");
        assert_eq!(at(4), Some(4..6), "the word after it");
        // a \r that no \n follows, and a \n that no \r precedes, stand alone
        let text = "a\rb\n\rc";
        assert_eq!(word_range_at(text, 1), Some(1..2));
        assert_eq!(word_range_at(text, 3), Some(3..4));
        assert_eq!(word_range_at(text, 4), Some(4..5));
    }
}
