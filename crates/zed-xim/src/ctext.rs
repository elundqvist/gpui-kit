// Added in elundqvist/gpui-kit to zed-xim 0.4.0-zed as published to crates.io: FORK.md at the
// crate's root lists the changes.

//! COMPOUND_TEXT, the encoding an input method hands a client its
//! preedit and what it commits in, decoded to UTF-8 whatever it holds.
//!
//! libX11, which ibus, fcitx 4 and uim encode with, writes each character
//! in the first charset of the locale's list that has it: ASCII, a letter
//! of Latin-1 as one byte in the right half, Greek, Cyrillic or Latin-4 as
//! a 96-character set designated to the right half, Japanese, Korean and
//! Chinese as JIS X 0208, KS C 5601 and GB 2312 in the left half, and
//! anything else in a UTF-8 segment. xim-ctext 0.3 decodes a whole
//! string of UTF-8 or of JIS X 0208 and gives an error for the rest,
//! which the client met with `expect`. This decodes ISO 2022 as compound
//! text has it, a side at a time, and never fails: a byte or a charset it
//! has no table for is U+FFFD, a sequence it does not know is passed over.

use alloc::string::String;
use encoding_rs::Encoding;

const ESC: u8 = 0x1B;
const CSI: u8 = 0x9B;
const STX: u8 = 0x02;
const REPLACEMENT: char = '\u{FFFD}';

/// The charset designated to a side: GL, the bytes 0x21 to 0x7E, or GR,
/// 0xA0 to 0xFF.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Set {
    /// ASCII, the left half of ISO 8859-1, where GL begins.
    Ascii,
    /// The left half of JIS X 0201: ASCII with a yen for the backslash and
    /// an overline for the tilde.
    JisRoman,
    /// The right half of JIS X 0201, the half-width katakana.
    Katakana,
    /// The right half of ISO 8859-1, where GR begins.
    Latin1,
    /// The right half of another 96-character set, by the encoding that
    /// has it in 0xA0 to 0xFF.
    Upper(&'static Encoding),
    /// A set of 94 by 94 characters, two bytes each, by the EUC encoding
    /// that has it in GR.
    Double(&'static Encoding),
    /// JIS X 0212, which EUC-JP has behind the byte 0x8F.
    Jis0212,
    /// A set there is no table for, of so many bytes a character.
    Unknown(usize),
}

/// The text of `bytes`, COMPOUND_TEXT, as UTF-8.
///
/// A string with no escape in it that is UTF-8 is taken as UTF-8, as
/// xim-ctext 0.3 took every string with no escape, for an input method
/// that sends UTF-8 as it is: ASCII reads the same either way, and the
/// Latin-1 that would also read as UTF-8 is letters no one types
/// together, `Ã©` and the like.
pub fn compound_text_to_utf8_lossy(bytes: &[u8]) -> String {
    if !bytes.contains(&ESC) {
        if let Ok(text) = core::str::from_utf8(bytes) {
            return String::from(text);
        }
    }
    let mut out = String::with_capacity(bytes.len());
    let (mut gl, mut gr) = (Set::Ascii, Set::Latin1);
    let mut at = 0;
    while at < bytes.len() {
        let b = bytes[at];
        at = match b {
            ESC => escape(bytes, at, &mut gl, &mut gr, &mut out),
            CSI => control_sequence_end(bytes, at + 1),
            b'\t' | b'\n' | b' ' => {
                out.push(char::from(b));
                at + 1
            }
            0x21..=0x7E => character(gl, bytes, at, &mut out),
            0xA0..=0xFF => character(gr, bytes, at, &mut out),
            // other controls, DEL and C1 have no place in compound text
            _ => at + 1,
        };
    }
    out
}

/// Decodes the character at `at`, of a side `set` is designated to, and
/// gives where the next begins.
fn character(set: Set, bytes: &[u8], at: usize, out: &mut String) -> usize {
    let b = bytes[at];
    let gr = b & 0x80 != 0;
    // the byte as it stands in GR, where the EUC encodings have their sets
    let high = b | 0x80;
    // 0xA0 and 0xFF have no character in a set of 94
    let in94 = (0xA1..=0xFE).contains(&high);
    match set {
        Set::Ascii | Set::JisRoman if !in94 => out.push(REPLACEMENT),
        Set::Ascii => out.push(char::from(b & 0x7F)),
        Set::JisRoman => out.push(match b & 0x7F {
            0x5C => '\u{A5}',
            0x7E => '\u{203E}',
            c => char::from(c),
        }),
        Set::Katakana => out.push(match high {
            // U+FF61 HALFWIDTH IDEOGRAPHIC FULL STOP to U+FF9F
            0xA1..=0xDF => char::from_u32(0xFF61 + u32::from(high - 0xA1)).unwrap_or(REPLACEMENT),
            _ => REPLACEMENT,
        }),
        Set::Latin1 if gr => out.push(char::from(b)),
        Set::Upper(encoding) if gr => push_decoded(out, encoding, &[b]),
        // a set of 96 has no place in GL
        Set::Latin1 | Set::Upper(_) => out.push(REPLACEMENT),
        Set::Double(_) | Set::Jis0212 => {
            let second = match bytes.get(at + 1).copied().filter(|&c| in94 && same_side(c, gr)) {
                Some(second) => second,
                None => {
                    out.push(REPLACEMENT);
                    return at + 1;
                }
            };
            if let Set::Double(encoding) = set {
                push_decoded(out, encoding, &[high, second | 0x80]);
            } else {
                push_decoded(out, encoding_rs::EUC_JP, &[0x8F, high, second | 0x80]);
            }
            return at + 2;
        }
        Set::Unknown(width) => {
            out.push(REPLACEMENT);
            let mut end = at + 1;
            while end < at + width && bytes.get(end).map_or(false, |&c| same_side(c, gr)) {
                end += 1;
            }
            return end;
        }
    }
    at + 1
}

/// Whether `b` is a graphic byte of the side, GL or GR, `gr` names, the
/// second byte of a character of two has to be.
fn same_side(b: u8, gr: bool) -> bool {
    if gr { (0xA1..=0xFE).contains(&b) } else { (0x21..=0x7E).contains(&b) }
}

fn push_decoded(out: &mut String, encoding: &'static Encoding, bytes: &[u8]) {
    let (text, _) = encoding.decode_without_bom_handling(bytes);
    out.push_str(&text);
}

/// Acts on the escape sequence at `at`, and gives where what follows it
/// begins: a designation sets a side's charset, a segment of UTF-8 or of
/// another encoding is decoded whole.
fn escape(bytes: &[u8], at: usize, gl: &mut Set, gr: &mut Set, out: &mut String) -> usize {
    let mut end = at + 1;
    while bytes.get(end).map_or(false, |b| (0x20..=0x2F).contains(b)) {
        end += 1;
    }
    let last = match bytes.get(end) {
        Some(&last) => last,
        None => return bytes.len(),
    };
    if !(0x30..=0x7E).contains(&last) {
        // no escape sequence: what came after the escape is read as text
        return end;
    }
    let next = end + 1;
    match (&bytes[at + 1..end], last) {
        (b"(", f) => *gl = set94(f),
        (b")", f) => *gr = set94(f),
        (b"-", f) => *gr = set96(f),
        (b"$(", f) | (b"$", f @ 0x40..=0x42) => *gl = set94n(f),
        (b"$)", f) => *gr = set94n(f),
        (b"%", b'G') => {
            let (text, after) = to_standard_return(bytes, next);
            out.push_str(&String::from_utf8_lossy(text));
            return after;
        }
        (b"%", b'@') => {}
        (b"%/", f @ 0x30..=0x34) => return extended_segment(bytes, next, f, out),
        (b"%", _) => {
            // another coding system: nothing to decode it with
            let (text, after) = to_standard_return(bytes, next);
            if !text.is_empty() {
                out.push(REPLACEMENT);
            }
            return after;
        }
        // the version of compound text, a direction, a charset of C0 or C1
        _ => {}
    }
    next
}

/// The bytes from `from` up to the `ESC % @` that returns from a coding
/// system, or to the end, and where what follows the return begins.
fn to_standard_return(bytes: &[u8], from: usize) -> (&[u8], usize) {
    let rest = bytes.get(from..).unwrap_or_default();
    match rest.windows(3).position(|w| w == [ESC, b'%', b'@']) {
        Some(end) => (&rest[..end], from + end + 3),
        None => (rest, bytes.len()),
    }
}

/// An extended segment, `ESC % / F M L`, then its length's worth of bytes:
/// the name of its encoding up to an STX, and the text. `width` is F, the
/// bytes of a character, `0` for a varying number.
fn extended_segment(bytes: &[u8], from: usize, width: u8, out: &mut String) -> usize {
    let (m, l) = match (bytes.get(from), bytes.get(from + 1)) {
        (Some(&m), Some(&l)) => (m, l),
        _ => return bytes.len(),
    };
    let len = usize::from(m & 0x7F) << 7 | usize::from(l & 0x7F);
    let start = from + 2;
    let end = (start + len).min(bytes.len());
    let segment = &bytes[start.min(end)..end];
    let (name, text) = match segment.iter().position(|&b| b == STX) {
        Some(stx) => (&segment[..stx], &segment[stx + 1..]),
        None => (segment, &[][..]),
    };
    match encoding_named(name) {
        Some(encoding) => push_decoded(out, encoding, text),
        None => {
            let width = usize::from(width - b'0');
            let characters = if width == 0 { usize::from(!text.is_empty()) } else { (text.len() + width - 1) / width };
            for _ in 0..characters {
                out.push(REPLACEMENT);
            }
        }
    }
    end
}

/// The encoding an extended segment names, by the X registry's names, as
/// libX11's locales write them.
fn encoding_named(name: &[u8]) -> Option<&'static Encoding> {
    let name = core::str::from_utf8(name).ok()?;
    let is = |n: &str| name.eq_ignore_ascii_case(n);
    let encoding = if is("BIG5-0") || is("BIG5-E0") || is("BIG5-E1") || is("BIG5HKSCS-0") || is("BIG5-HKSCS") {
        encoding_rs::BIG5
    } else if is("GBK-0") {
        encoding_rs::GBK
    } else if is("GB18030-0") || is("GB18030-1") {
        encoding_rs::GB18030
    } else if is("KOI8-R") {
        encoding_rs::KOI8_R
    } else if is("KOI8-U") {
        encoding_rs::KOI8_U
    } else if is("ISO10646-1") || is("UTF-8") {
        encoding_rs::UTF_8
    } else {
        return None;
    };
    Some(encoding)
}

/// The set of 94 characters a final byte designates.
fn set94(f: u8) -> Set {
    match f {
        b'B' => Set::Ascii,
        b'J' => Set::JisRoman,
        b'I' => Set::Katakana,
        _ => Set::Unknown(1),
    }
}

/// The set of 96 characters a final byte designates: the parts of ISO 8859
/// by their ISO-IR registration.
fn set96(f: u8) -> Set {
    let encoding = match f {
        b'A' => return Set::Latin1,
        b'B' => encoding_rs::ISO_8859_2,
        b'C' => encoding_rs::ISO_8859_3,
        b'D' => encoding_rs::ISO_8859_4,
        b'F' => encoding_rs::ISO_8859_7,
        b'G' => encoding_rs::ISO_8859_6,
        b'H' => encoding_rs::ISO_8859_8,
        b'L' => encoding_rs::ISO_8859_5,
        // ISO 8859-9, which windows-1254 is above 0x9F
        b'M' => encoding_rs::WINDOWS_1254,
        // TIS 620, ISO 8859-11, which windows-874 is above 0x9F
        b'T' => encoding_rs::WINDOWS_874,
        b'V' => encoding_rs::ISO_8859_10,
        b'Y' => encoding_rs::ISO_8859_13,
        b'_' => encoding_rs::ISO_8859_14,
        b'b' => encoding_rs::ISO_8859_15,
        b'f' => encoding_rs::ISO_8859_16,
        _ => return Set::Unknown(1),
    };
    Set::Upper(encoding)
}

/// The set of 94 by 94 characters a final byte designates.
fn set94n(f: u8) -> Set {
    match f {
        b'A' => Set::Double(encoding_rs::GBK),
        // JIS C 6226-1978, which JIS X 0208 took the place of, and JIS X 0208
        b'@' | b'B' => Set::Double(encoding_rs::EUC_JP),
        b'C' => Set::Double(encoding_rs::EUC_KR),
        b'D' => Set::Jis0212,
        _ => Set::Unknown(2),
    }
}

/// Where the control sequence whose CSI is just before `from` ends: its
/// parameters, intermediates and final byte. Compound text has them for
/// the direction of the text, which a preedit has no use for.
fn control_sequence_end(bytes: &[u8], from: usize) -> usize {
    let mut at = from;
    while bytes.get(at).map_or(false, |b| (0x20..=0x3F).contains(b)) {
        at += 1;
    }
    match bytes.get(at) {
        Some(0x40..=0x7E) => at + 1,
        _ => at,
    }
}

#[cfg(test)]
mod tests {
    use super::compound_text_to_utf8_lossy as decode;
    use alloc::vec::Vec;

    /// Each string as libX11 1.8 encodes it in en_US.UTF-8
    /// (`Xutf8TextListToTextProperty` with `XCompoundTextStyle`, as
    /// ibus-x11 encodes a preedit and a commit), and its text.
    #[test]
    fn what_libx11_writes() {
        let cases: &[(&[u8], &str)] = &[
            (b"abc", "abc"),
            // Latin-1 in GR, which xim-ctext 0.3 read as UTF-8 and failed
            (&[0xE9], "é"),
            (&[0x61, 0xE9], "aé"),
            // JIS X 0208 in GL
            (&[0x1B, 0x24, 0x28, 0x42, 0x46, 0x7C], "日"),
            (&[0x1B, 0x24, 0x28, 0x42, 0x46, 0x7C, 0x37, 0x6E], "日月"),
            (&[0x61, 0x1B, 0x24, 0x28, 0x42, 0x46, 0x7C], "a日"),
            (&[0x1B, 0x24, 0x28, 0x42, 0x43, 0x66, 0x4A, 0x38], "中文"),
            // KS C 5601 in GL, which xim-ctext 0.3 did not support
            (&[0x1B, 0x24, 0x28, 0x43, 0x47, 0x51, 0x31, 0x39], "한국"),
            // a UTF-8 segment
            (&[0x1B, 0x25, 0x47, 0xE0, 0xB9, 0x84, 0xE0, 0xB8, 0x97, 0xE0, 0xB8, 0xA2, 0x1B, 0x25, 0x40], "ไทย"),
            (&[0x1B, 0x25, 0x47, 0xF0, 0x9F, 0x98, 0x80, 0x1B, 0x25, 0x40], "😀"),
            // sets of 96 in GR
            (&[0x1B, 0x2D, 0x46, 0xD9], "Ω"),
            (&[0x1B, 0x2D, 0x4C, 0xBF, 0xE0, 0xD8, 0xD2, 0xD5, 0xE2], "Привет"),
            (&[0x1B, 0x2D, 0x44, 0xC0], "Ā"),
            // JIS X 0201's katakana in GR
            (&[0x1B, 0x29, 0x49, 0xB6], "ｶ"),
            // GL stays JIS X 0208 across a UTF-8 segment, and GR Latin-1
            (
                &[
                    0x1B, 0x24, 0x28, 0x42, 0x46, 0x7C, 0x1B, 0x25, 0x47, 0xF0, 0x9F, 0x98, 0x80, 0x1B, 0x25, 0x40, 0xE9,
                ],
                "日😀é",
            ),
        ];
        for (bytes, text) in cases {
            assert_eq!(decode(bytes), *text, "{bytes:02x?}");
        }
    }

    /// What ibus 1.5 sends with cangjie5, its preedit for `a` and for `ab`
    /// and the commit after it, GB 2312 in GL, on which zed-xim's client
    /// panicked ("Encoding Error: UnsupportedEncoding").
    #[test]
    fn what_ibus_sends_for_cangjie5() {
        assert_eq!(decode(&[0x1B, 0x24, 0x28, 0x41, 0x48, 0x55]), "日");
        assert_eq!(decode(&[0x1B, 0x24, 0x28, 0x41, 0x43, 0x77]), "明");
    }

    /// Each segment's charset is its own: GB 2312 between runs of ASCII,
    /// as an input method writes a date (the case xim-ctext 0.4.1 added),
    /// and Japanese then Chinese.
    #[test]
    fn segments_of_several_charsets() {
        #[rustfmt::skip]
        let date: &[u8] = &[
            b'2', b'0', b'2', b'6',
            0x1B, 0x24, 0x28, 0x41, 0x44, 0x6A,
            0x1B, 0x28, 0x42, b'0', b'7',
            0x1B, 0x24, 0x28, 0x41, 0x54, 0x42,
            0x1B, 0x28, 0x42, b'1', b'6',
            0x1B, 0x24, 0x28, 0x41, 0x48, 0x55,
        ];
        assert_eq!(decode(date), "2026年07月16日");
        let mixed: &[u8] = &[0x1B, 0x24, 0x28, 0x42, 0x5F, 0x5A, 0x53, 0x28, 0x1B, 0x24, 0x28, 0x41, 0x44, 0x63];
        assert_eq!(decode(mixed), "炸哦你");
        // the same sets designated to GR, as EUC has them
        assert_eq!(decode(&[0x1B, 0x24, 0x29, 0x41, 0xC4, 0xE3, 0x1B, 0x24, 0x29, 0x43, 0xC7, 0xD1]), "你한");
        // Latin-2 in GR beside ASCII in GL
        assert_eq!(decode(&[b'z', 0x1B, 0x2D, 0x42, 0xB1, b'a']), "ząa");
        // JIS X 0201's Roman in GL
        assert_eq!(decode(&[0x1B, 0x28, 0x4A, 0x5C, 0x31, 0x7E]), "¥1‾");
        // JIS X 0212, behind EUC-JP's 0x8F
        assert_eq!(decode(&[0x1B, 0x24, 0x28, 0x44, 0x30, 0x21]), "丂");
    }

    /// A segment of an encoding named in it, as libX11 writes Big5 and
    /// KOI8-R; an encoding it names that there is no table for is a
    /// replacement character a character.
    #[test]
    fn extended_segments() {
        let segment = |width: u8, name: &str, text: &[u8]| {
            let len = name.len() + 1 + text.len();
            let mut bytes = Vec::from([0x1B, b'%', b'/', width, 0x80 | (len >> 7) as u8, 0x80 | (len & 0x7F) as u8]);
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0x02);
            bytes.extend_from_slice(text);
            bytes
        };
        // 中文 in Big5, then ASCII after the segment
        let mut big5 = segment(b'2', "BIG5-0", &[0xA4, 0xA4, 0xA4, 0xE5]);
        big5.push(b'!');
        assert_eq!(decode(&big5), "中文!");
        assert_eq!(decode(&segment(b'1', "KOI8-R", &[0xF0, 0xD2])), "Пр");
        assert_eq!(decode(&segment(b'2', "NOSUCH-0", &[0xA4, 0xA4, 0xA4, 0xE5])), "\u{FFFD}\u{FFFD}");
        // a length longer than what is there ends the text
        let mut cut = segment(b'2', "BIG5-0", &[0xA4, 0xA4]);
        cut[5] += 9;
        assert_eq!(decode(&cut), "中");
    }

    /// What does not decode is a replacement character, or nothing, and
    /// never a failure: the cases that made zed-xim's client panic, and
    /// bytes of no meaning.
    #[test]
    fn what_does_not_decode() {
        let cases: &[(&[u8], &str)] = &[
            // a UTF-8 segment that is not UTF-8
            (&[0x1B, 0x25, 0x47, 0xFF, b'a', 0x1B, 0x25, 0x40], "\u{FFFD}a"),
            // one with no return, to the end
            (&[0x1B, 0x25, 0x47, b'a', b'b'], "ab"),
            // CNS 11643, which there is no table for: a replacement a character
            (&[0x1B, 0x24, 0x28, 0x47, 0x44, 0x21, 0x44, 0x22, 0x1B, 0x28, 0x42, b'x'], "\u{FFFD}\u{FFFD}x"),
            // half a character of two
            (&[0x1B, 0x24, 0x28, 0x43, 0x47], "\u{FFFD}"),
            (&[0x1B, 0x24, 0x28, 0x43, 0x47, 0xE9], "\u{FFFD}é"),
            // an escape cut short, and one of no sequence
            (&[b'a', 0x1B], "a"),
            (&[b'a', 0x1B, 0x24], "a"),
            (&[0x1B, 0x07, b'b'], "b"),
            // another coding system, up to its return
            (&[0x1B, 0x25, 0x42, 0x01, 0x02, 0x1B, 0x25, 0x40, b'c'], "\u{FFFD}c"),
            // controls compound text has no place for
            (&[b'a', 0x00, 0x07, 0x7F, 0x85, b'b'], "ab"),
            // the direction of the text is no character
            (&[0x9B, b'1', b']', b'a', 0x9B, b']'], "a"),
            // GB 2312 with a byte of GR in it, which xim-ctext 0.4.1 overflows on
            (&[0x1B, 0x24, 0x28, 0x41, 0x44, 0x63, 0xE9], "你é"),
        ];
        for (bytes, text) in cases {
            assert_eq!(decode(bytes), *text, "{bytes:02x?}");
        }
    }

    /// Text with no escape in it that is UTF-8 is read as UTF-8, as
    /// xim-ctext 0.3 read it, for an input method that sends it so; one
    /// that is not is Latin-1, as compound text begins.
    #[test]
    fn unescaped_text() {
        assert_eq!(decode("가나다".as_bytes()), "가나다");
        assert_eq!(decode(&[0x63, 0x61, 0x66, 0xE9]), "café");
        assert_eq!(decode(&[]), "");
    }
}
