use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CharacterKind {
    Word,
    Whitespace,
    Newline,
    Other,
}

impl From<char> for CharacterKind {
    /// A letter or a digit of any script is a word's, as VS Code has it,
    /// where Latin, Cyrillic and `_` alone were and a double-click in
    /// Hebrew, Arabic, Greek or Han took one letter (elundqvist/kvist#677).
    fn from(character: char) -> Self {
        if character == '_' || character.is_alphanumeric() {
            Self::Word
        } else if matches!(character, '\n' | '\r') {
            Self::Newline
        } else if character.is_whitespace() {
            Self::Whitespace
        } else {
            Self::Other
        }
    }
}

/// How many characters either side of a double-click are read for its word, about as many
/// as VS Code reads: a word longer than that either side of the click is taken in part.
const WINDOW: usize = 1024;

/// Whether `c` is read with the character before it as one grapheme, a mark, a joiner or
/// a selector: a window of the text cut before it cuts a grapheme in two.
fn extends(c: char) -> bool {
    let mut pair = String::from("a");
    pair.push(c);
    pair.graphemes(true).nth(1).is_none()
}

/// What a grapheme is to a double-click: what its first character is, so
/// a mark, a virama or a joiner is read with the letter it is set on, as
/// Thai's tone marks and Hebrew's points are, and a `\r\n` is one line
/// break.
fn kind_of(grapheme: &str) -> CharacterKind {
    grapheme
        .chars()
        .next()
        .map_or(CharacterKind::Other, CharacterKind::from)
}

/// The word of `text` a double-click at `at` takes: the run of graphemes
/// of words or of whitespace it is in, or the one grapheme of another
/// kind, an emoji or a punctuation mark, whole. A line break connects to
/// nothing.
fn word_range_in(text: &str, at: usize) -> Range<usize> {
    let graphemes: Vec<(usize, &str)> = text.grapheme_indices(true).collect();
    let Some(g) = graphemes.partition_point(|(start, _)| *start <= at).checked_sub(1) else {
        return at..at;
    };
    let end_of = |g: usize| graphemes[g].0 + graphemes[g].1.len();
    let kind = kind_of(graphemes[g].1);
    if !matches!(kind, CharacterKind::Word | CharacterKind::Whitespace) {
        return graphemes[g].0..end_of(g);
    }
    let first = graphemes[..g]
        .iter()
        .rposition(|(_, grapheme)| kind_of(grapheme) != kind)
        .map_or(0, |before| before + 1);
    let last = graphemes[g..]
        .iter()
        .position(|(_, grapheme)| kind_of(grapheme) != kind)
        .map_or(graphemes.len(), |after| g + after)
        - 1;
    graphemes[first].0..end_of(last)
}

/// The word a double-click on `character`, at `offset`, takes, with the
/// characters before it, nearest first, and after it: `word_range_in`
/// over the `WINDOW` either side, and as many marks and joiners past them
/// as are set on the graphemes at its ends, so the window cuts no
/// grapheme: cut in a letter with 200 marks on it, the pick lost the
/// letter. A `\r\n` is one character to the selection
/// as it is to the caret: a double-click on the end of a CRLF line takes
/// the pair, never the `\r` alone, out of which Right would step onto the
/// `\n`. A `\r` no `\n` follows is a line break of its own.
pub(crate) fn word_range_from_chars(
    offset: usize,
    character: char,
    previous: impl Iterator<Item = char>,
    following: impl Iterator<Item = char>,
) -> Range<usize> {
    let before = chars_before(previous, WINDOW);
    let mut text: String = before.iter().rev().collect();
    let at = text.len();
    text.push(character);
    let mut following = following.peekable();
    text.extend(following.by_ref().take(WINDOW));
    text.extend(
        std::iter::from_fn(|| following.next_if(|c| extends(*c))).take(WINDOW),
    );
    let range = word_range_in(&text, at);
    offset - at + range.start..offset - at + range.end
}

/// Up to `count` of the characters `previous` gives, nearest first, and the marks and
/// joiners before them back to the letter they are set on, up to `count` more.
fn chars_before(mut previous: impl Iterator<Item = char>, count: usize) -> Vec<char> {
    let mut before: Vec<char> = previous.by_ref().take(count).collect();
    while before.len() < 2 * count && before.last().is_some_and(|c| extends(*c)) {
        match previous.next() {
            Some(c) => before.push(c),
            None => break,
        }
    }
    before
}

/// What a double-click past the end of a line takes, `end` being where the
/// line ends, before its line break, and `previous` its characters back
/// from there: the word the line ends in, or the whitespace, as a
/// double-click on its last character takes them, and a grapheme that is
/// neither, an emoji or a symbol, whole, as there. A line that is empty or
/// ends in punctuation gives nothing, a caret at its end. Never the line
/// break: taken, the next key typed joined the line to the next
/// (elundqvist/kvist#416). VS Code takes the word a click past the end
/// touches, the whitespace after the last word, or nothing.
pub(crate) fn word_range_before_line_end(
    end: usize,
    previous: impl Iterator<Item = char>,
) -> Range<usize> {
    let mut text = chars_before(previous, WINDOW + 1);
    text.reverse();
    let text: String = text.into_iter().collect();
    let Some(last) = text.graphemes(true).next_back() else {
        return end..end;
    };
    let start = end - text.len();
    match kind_of(last) {
        CharacterKind::Newline => end..end,
        CharacterKind::Other if last.starts_with(|c: char| c.is_ascii_punctuation()) => end..end,
        _ => start + word_range_in(&text, text.len() - last.len()).start..end,
    }
}

pub(crate) fn word_range_at(text: &str, offset: usize) -> Option<Range<usize>> {
    let offset = clip_offset_left(text, offset);
    let character = text[offset..].chars().next()?;
    let end = offset + character.len_utf8();
    Some(word_range_from_chars(
        offset,
        character,
        text[..offset].chars().rev(),
        text[end..].chars(),
    ))
}

pub(crate) fn line_range_at(text: &str, offset: usize) -> Range<usize> {
    let offset = clip_offset_left(text, offset);
    let start = text[..offset].rfind('\n').map_or(0, |newline| newline + 1);
    let end = text[offset..]
        .find('\n')
        .map_or(text.len(), |newline| offset + newline);
    start..end
}

fn clip_offset_left(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_character_kind_from_char() {
        for c in ['a', 'Z', '0', '_', 'é', 'ä', 'ö', 'ü', 'д', 'ש', 'م', 'λ', '汉', 'ก', '٣'] {
            assert_eq!(CharacterKind::from(c), CharacterKind::Word, "{c:?}");
        }
        for c in ['.', ',', ';', '!', '?', '[', '{', '。', '🎉', '→', '\u{05BE}'] {
            assert_eq!(CharacterKind::from(c), CharacterKind::Other, "{c:?}");
        }
        for c in [' ', '\t', '\u{00A0}'] {
            assert_eq!(CharacterKind::from(c), CharacterKind::Whitespace, "{c:?}");
        }
        for c in ['\n', '\r'] {
            assert_eq!(CharacterKind::from(c), CharacterKind::Newline, "{c:?}");
        }
    }

    fn word(text: &str, at: &str) -> String {
        let offset = text.find(at).unwrap();
        text[word_range_at(text, offset).unwrap()].to_string()
    }

    /// A double-click takes a word of any script, as VS Code's does: in
    /// Hebrew, Arabic, Greek and the rest it took one letter, Latin,
    /// Cyrillic and `_` alone being a word's (elundqvist/kvist#677). A
    /// run of Han is one word, as VS Code and IntelliJ have it.
    #[test]
    fn a_double_click_takes_a_word_of_any_script() {
        let text = "hebrew: שלום עולם, arabic: مرحبا بالعالم. greek: καλημέρα κόσμε cjk: 汉字汉字。测试";
        assert_eq!(word(text, "ולם"), "עולם");
        assert_eq!(word(text, "שלום"), "שלום");
        assert_eq!(word(text, "عالم"), "بالعالم");
        assert_eq!(word(text, "ημέ"), "καλημέρα");
        assert_eq!(word(text, "字汉"), "汉字汉字");
        assert_eq!(word(text, "测"), "测试");
        assert_eq!(word(text, "。"), "。", "CJK punctuation stands alone");
        assert_eq!(word("x = 42", "42"), "42");
        assert_eq!(word("٣٤٥ عدد", "٤"), "٣٤٥", "Arabic-Indic digits");
    }

    /// A mark, a virama or a joiner is read with the letter it is set on:
    /// Hebrew's points, Arabic's vowels, Thai's tone marks, a Devanagari
    /// conjunct and a Persian word with a zero-width non-joiner in it are
    /// one word, and an emoji is taken whole, never the half of a
    /// grapheme the caret cannot stand in.
    #[test]
    fn a_mark_or_a_joiner_is_read_with_its_letter() {
        assert_eq!(word("אָב בְּרֵאשִׁית x", "רֵ"), "בְּרֵאשִׁית");
        assert_eq!(word("كَتَبَ الوَلَدُ", "تَ"), "كَتَبَ");
        assert_eq!(word("ภาษาไทย น้ำใจ ok", "ำ"), "น้ำใจ");
        assert_eq!(word("नमस्ते दुनिया", "ते"), "नमस्ते");
        assert_eq!(word("می‌خواهم بروم", "خوا"), "می‌خواهم");
        assert_eq!(word("e\u{301}te\u{301} x", "t"), "e\u{301}te\u{301}");
        let family = "a 👨\u{200D}👩\u{200D}👧 b";
        assert_eq!(word(family, "👩"), "👨\u{200D}👩\u{200D}👧", "an emoji whole");
        assert_eq!(word("❤\u{FE0F}abc", "abc"), "abc", "never the emoji's selector");
        assert_eq!(word("❤\u{FE0F}abc", "❤"), "❤\u{FE0F}");
        // a mark clicked is taken with its letter
        let text = "שָׁלוֹם";
        let mark = text.char_indices().nth(1).unwrap().0;
        assert_eq!(&text[word_range_at(text, mark).unwrap()], text);
    }

    /// Past a line's end, the word, the whitespace or the grapheme it
    /// ends in, as on its last character.
    #[test]
    fn past_a_lines_end_a_word_of_any_script() {
        let at_end = |line: &str| {
            let range = word_range_before_line_end(line.len(), line.chars().rev());
            line[range].to_string()
        };
        assert_eq!(at_end("x שלום עולם"), "עולם");
        assert_eq!(at_end("καλημέρα κόσμε"), "κόσμε");
        assert_eq!(at_end("汉字 测试"), "测试");
        assert_eq!(at_end("ok น้ำ"), "น้ำ");
        assert_eq!(at_end("a 👨\u{200D}👩"), "👨\u{200D}👩");
        assert_eq!(at_end("end."), "");
        assert_eq!(at_end(""), "");
        // a letter with more marks on it than are read either side
        let piled = format!("ok \u{0E20}{}", "\u{0E48}".repeat(1100));
        assert_eq!(at_end(&piled), &piled[3..]);
    }

    /// The window either side of a double-click cuts no grapheme, and takes
    /// a word of hundreds of letters whole: cut in a letter with 200 marks on
    /// it, 128 read either side, the pick lost the letter, and a word of 600
    /// letters was taken in part (elundqvist/kvist#677).
    #[test]
    fn a_long_word_or_a_pile_of_marks_is_taken_whole() {
        let piled = format!("\u{0E20}{}\u{0E29}\u{0E32} ok", "\u{0E48}".repeat(1100));
        let last_mark = piled.rfind('\u{0E48}').unwrap();
        let range = word_range_at(&piled, last_mark).unwrap();
        assert_eq!(&piled[range], piled.strip_suffix(" ok").unwrap());
        let long = format!("x {} y", "\u{05D0}".repeat(600));
        let range = word_range_at(&long, long.len() - 4).unwrap();
        assert_eq!(range, 2..long.len() - 2);
        assert!(extends('\u{0E48}') && extends('\u{200D}') && extends('\u{FE0F}'));
        assert!(!extends('a') && !extends(' ') && !extends('\u{05D0}'));
    }
}
