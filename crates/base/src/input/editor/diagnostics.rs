use std::{
    cmp::Ordering,
    ops::{Deref, Range},
};

use gpui::SharedString;
use ropey::Rope;
use sum_tree::{Bias, SeekTarget, SumTree};

use crate::input::{Position, RopeExt as _};

pub type DiagnosticRelatedInformation = lsp_types::DiagnosticRelatedInformation;
pub(super) type CodeDescription = lsp_types::CodeDescription;
pub type RelatedInformation = lsp_types::DiagnosticRelatedInformation;
pub type DiagnosticTag = lsp_types::DiagnosticTag;

#[derive(Debug, Eq, PartialEq, Clone, Default)]
pub struct Diagnostic {
    /// The range [`Position`] at which the message applies.
    ///
    /// This is the column, character range within a single line.
    pub range: Range<Position>,

    /// The diagnostic's severity. Can be omitted. If omitted it is up to the
    /// client to interpret diagnostics as error, warning, info or hint.
    pub severity: DiagnosticSeverity,

    /// The diagnostic's code. Can be omitted.
    pub code: Option<SharedString>,

    pub code_description: Option<CodeDescription>,

    /// A human-readable string describing the source of this
    /// diagnostic, e.g. 'typescript' or 'super lint'.
    pub source: Option<SharedString>,

    /// The diagnostic's message.
    pub message: SharedString,

    /// An array of related diagnostic information, e.g. when symbol-names within
    /// a scope collide all definitions can be marked via this property.
    pub related_information: Option<Vec<DiagnosticRelatedInformation>>,

    /// Additional metadata about the diagnostic.
    pub tags: Option<Vec<DiagnosticTag>>,

    /// A data entry field that is preserved between a `textDocument/publishDiagnostics`
    /// notification and `textDocument/codeAction` request.
    ///
    /// @since 3.16.0
    pub data: Option<serde_json::Value>,
}

impl From<lsp_types::Diagnostic> for Diagnostic {
    fn from(value: lsp_types::Diagnostic) -> Self {
        Self {
            range: value.range.start..value.range.end,
            severity: value
                .severity
                .map(Into::into)
                .unwrap_or(DiagnosticSeverity::Info),
            code: value.code.map(|c| match c {
                lsp_types::NumberOrString::Number(n) => SharedString::from(n.to_string()),
                lsp_types::NumberOrString::String(s) => SharedString::from(s),
            }),
            code_description: value.code_description,
            source: value.source.map(|s| s.into()),
            message: value.message.into(),
            related_information: value.related_information,
            tags: value.tags,
            data: value.data,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiagnosticSeverity {
    #[default]
    Hint,
    Error,
    Warning,
    Info,
}

impl From<lsp_types::DiagnosticSeverity> for DiagnosticSeverity {
    fn from(value: lsp_types::DiagnosticSeverity) -> Self {
        match value {
            lsp_types::DiagnosticSeverity::ERROR => Self::Error,
            lsp_types::DiagnosticSeverity::WARNING => Self::Warning,
            lsp_types::DiagnosticSeverity::INFORMATION => Self::Info,
            lsp_types::DiagnosticSeverity::HINT => Self::Hint,
            _ => Self::Info, // Default to Info if unknown
        }
    }
}

impl Diagnostic {
    pub fn new(range: Range<impl Into<Position>>, message: impl Into<SharedString>) -> Self {
        Self {
            range: range.start.into()..range.end.into(),
            message: message.into(),
            ..Default::default()
        }
    }

    pub fn with_severity(mut self, severity: impl Into<DiagnosticSeverity>) -> Self {
        self.severity = severity.into();
        self
    }

    pub fn with_code(mut self, code: impl Into<SharedString>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn with_source(mut self, source: impl Into<SharedString>) -> Self {
        self.source = Some(source.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiagnosticEntry {
    /// The byte range of the diagnostic in the rope.
    pub range: Range<usize>,
    pub diagnostic: Diagnostic,
}

impl Deref for DiagnosticEntry {
    type Target = Diagnostic;

    fn deref(&self) -> &Self::Target {
        &self.diagnostic
    }
}

#[derive(Debug, Default, Clone)]
pub struct DiagnosticSummary {
    count: usize,
    start: usize,
    end: usize,
}

impl sum_tree::Item for DiagnosticEntry {
    type Summary = DiagnosticSummary;
    fn summary(&self, _cx: &()) -> Self::Summary {
        DiagnosticSummary {
            count: 1,
            start: self.range.start,
            end: self.range.end,
        }
    }
}

impl sum_tree::Summary for DiagnosticSummary {
    type Context<'a> = &'a ();
    fn zero(_: Self::Context<'_>) -> Self {
        DiagnosticSummary {
            count: 0,
            start: usize::MIN,
            end: usize::MIN,
        }
    }

    fn add_summary(&mut self, other: &Self, _: Self::Context<'_>) {
        self.start = other.start;
        self.end = other.end;
        self.count += other.count;
    }
}

/// For seeking by byte range.
impl SeekTarget<'_, DiagnosticSummary, DiagnosticSummary> for usize {
    fn cmp(&self, other: &DiagnosticSummary, _: &()) -> Ordering {
        if *self < other.start {
            Ordering::Less
        } else if *self > other.end {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    }
}

/// A count of marks, to slice the set's tree by.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Count(usize);

impl<'a> sum_tree::Dimension<'a, DiagnosticSummary> for Count {
    fn zero(_: &()) -> Self {
        Count(0)
    }

    fn add_summary(&mut self, summary: &'a DiagnosticSummary, _: &()) {
        self.0 += summary.count;
    }
}

#[derive(Debug, Clone)]
pub struct DiagnosticSet {
    text: Rope,
    diagnostics: SumTree<DiagnosticEntry>,
}

impl DiagnosticSet {
    pub fn new(text: &Rope) -> Self {
        Self {
            text: text.clone(),
            diagnostics: SumTree::new(&()),
        }
    }

    pub fn reset(&mut self, text: &Rope) {
        self.text = text.clone();
        self.clear();
    }

    /// Move the marks with an edit of the text: `range` of the text the
    /// set was of is `new_len` bytes now, and `text` is the text after it
    /// (`edit_steps`, of one step).
    pub fn edit(&mut self, range: &Range<usize>, new_len: usize, text: &Rope) {
        self.edit_steps(&[(range.clone(), new_len)], text)
    }

    /// Move the marks with edits made one after another, `steps`, each a
    /// range of the text as the ones before it left it and how many bytes
    /// went there, as an undo or a replace-all makes them; `text` is the
    /// text after the last. A mark before them stays, one after them
    /// moves with its text, and one between two of them stays on its
    /// text; one an edit reached keeps what is left of it, an end in the
    /// edit put at its edge, and one an edit took whole is dropped, as it
    /// has nothing left to underline. None grows by what is typed at its
    /// edges, as VS Code's markers do not. Dropped at every edit, a
    /// thousand marks were handed back at every key and cost 1.8 ms of it
    /// (elundqvist/kvist#370); moved by one edit from the first step to
    /// the last, as the highlighter is handed an undo, a mark between two
    /// of them went with the text that edit took. Whoever hands them is
    /// to hand them again once it knows where the edits left them. The
    /// marks that end before the first edit are kept as they are, in the
    /// part of the tree that holds them: every mark was copied and the
    /// tree built anew, 0.7 ms a key with a thousand marks in a debug
    /// build (elundqvist/kvist#383).
    pub fn edit_steps(&mut self, steps: &[(Range<usize>, usize)], text: &Rope) {
        let old = std::mem::replace(&mut self.text, text.clone());
        if self.diagnostics.is_empty() || steps.is_empty() {
            return;
        }
        let grown_by = |edits: &[(Range<usize>, usize)]| {
            let mut grown = vec![0isize; edits.len() + 1];
            for (k, (range, len)) in edits.iter().enumerate() {
                grown[k + 1] = grown[k] + *len as isize - range.len() as isize;
            }
            grown
        };
        // the edits as edits of the text before them, where the steps are
        // in order, as an undo's and a replace-all's are: an offset is
        // looked up among them; in another order, it goes through each
        let edits = apart(steps);
        let grown = edits.as_deref().map(grown_by).unwrap_or_default();
        let map = |at: usize, start: bool| match &edits {
            Some(edits) => moved(at, start, edits, &grown),
            None => steps.iter().fold(at, |at, step| {
                moved(at, start, std::slice::from_ref(step), &grown_by(std::slice::from_ref(step)))
            }),
        };
        // before the first edit a mark keeps its place as it was said;
        // past the line the last ends on it keeps its column and moves by
        // the lines the edits put in or took; between, its place is read
        // again from the text
        let first = match &edits {
            Some(edits) => edits[0].0.start,
            None => steps.iter().map(|(range, _)| range.start).min().unwrap_or(0),
        };
        let (past, rows) = match edits.as_deref().and_then(|edits| edits.last().map(|last| (edits.len() - 1, last))) {
            Some((k, (range, len))) => {
                let end_row = old.offset_to_point(range.end.min(old.len())).row;
                let past = if end_row + 1 < old.lines_len() {
                    old.line_start_offset(end_row + 1)
                } else {
                    usize::MAX
                };
                let new_end = (range.start as isize + grown[k]).max(0) as usize + len;
                (past, text.offset_to_point(new_end.min(text.len())).row as i64 - end_row as i64)
            }
            None => (usize::MAX, 0),
        };
        let position = |was: usize, now: usize, said: Position| {
            if was < first {
                said
            } else if was >= past {
                Position::new((said.line as i64 + rows).max(0) as u32, said.character)
            } else {
                text.offset_to_position(now)
            }
        };
        // the marks before the first edit, which none of it moves: those
        // that end before it, up to the first that does not
        let untouched = self
            .diagnostics
            .iter()
            .take_while(|entry| entry.range.end < first)
            .count();
        let mut cursor = self.diagnostics.cursor::<Count>(&());
        let mut kept = cursor.slice(&Count(untouched), Bias::Right);
        let rest = std::iter::from_fn(|| {
            let entry = cursor.item()?;
            cursor.next();
            Some(entry)
        });
        let mut entries: Vec<DiagnosticEntry> = rest
            .filter_map(|entry| {
                let (from, to) = if entry.range.is_empty() {
                    // a point stays a point, out of what is typed at it
                    let at = map(entry.range.start, false);
                    (at, at)
                } else {
                    (map(entry.range.start, true), map(entry.range.end, false))
                };
                if from >= to && !entry.range.is_empty() {
                    return None;
                }
                let mut diagnostic = entry.diagnostic.clone();
                diagnostic.range = position(entry.range.start, from, diagnostic.range.start)
                    ..position(entry.range.end, to, diagnostic.range.end);
                Some(DiagnosticEntry {
                    range: from..to,
                    diagnostic,
                })
            })
            .collect();
        drop(cursor);
        // after those kept, which begin before the first edit, where none
        // of these is moved to
        entries.sort_by_key(|entry| (entry.range.start, entry.range.end));
        kept.extend(entries, &());
        self.diagnostics = kept;
    }

    pub fn push(&mut self, diagnostic: impl Into<Diagnostic>) {
        let diagnostic = diagnostic.into();
        let start = self.text.position_to_offset(&diagnostic.range.start);
        let end = self.text.position_to_offset(&diagnostic.range.end);

        self.diagnostics.push(
            DiagnosticEntry {
                range: start..end,
                diagnostic,
            },
            &(),
        );
    }

    pub fn extend<D, I>(&mut self, diagnostics: D)
    where
        D: IntoIterator<Item = I>,
        I: Into<Diagnostic>,
    {
        for diagnostic in diagnostics {
            self.push(diagnostic.into());
        }
    }

    pub fn len(&self) -> usize {
        self.diagnostics.summary().count
    }

    pub fn clear(&mut self) {
        self.diagnostics = SumTree::new(&());
    }

    pub fn is_empty(&self) -> bool {
        self.diagnostics.is_empty()
    }

    pub fn range(&self, range: Range<usize>) -> impl Iterator<Item = &DiagnosticEntry> {
        let mut cursor = self.diagnostics.cursor::<DiagnosticSummary>(&());
        cursor.seek(&range.start, Bias::Left);
        std::iter::from_fn(move || {
            if let Some(entry) = cursor.item() {
                if entry.range.start < range.end {
                    cursor.next();
                    return Some(entry);
                }
            }
            None
        })
    }

    pub fn for_offset(&self, offset: usize) -> Option<&DiagnosticEntry> {
        self.range(offset..offset + 1).next()
    }

    #[allow(unused)]
    pub fn iter(&self) -> impl Iterator<Item = &DiagnosticEntry> {
        self.diagnostics.iter()
    }
}

/// `steps`, edits made one after another, each a range of the text as the
/// ones before it left it and how many bytes went there, as edits of the
/// text before the first of them, in order and apart, where the steps
/// allow it: each beginning at or past where the one before it put its
/// text, as a redo's do, or ending at or before where the one before it
/// began, as an undo's and a replace-all's do. None for steps in another
/// order, which overlap.
pub(crate) fn apart(steps: &[(Range<usize>, usize)]) -> Option<Vec<(Range<usize>, usize)>> {
    if steps.windows(2).all(|w| w[1].0.end <= w[0].0.start) {
        return Some(steps.iter().rev().cloned().collect());
    }
    let (mut grown, mut past) = (0isize, 0usize);
    let mut edits = Vec::with_capacity(steps.len());
    for (range, len) in steps {
        if range.start < past {
            return None;
        }
        let back = |at: usize| (at as isize - grown).max(0) as usize;
        edits.push((back(range.start)..back(range.end), *len));
        grown += *len as isize - range.len() as isize;
        past = range.start + len;
    }
    Some(edits)
}

/// Where `at`, an offset of the text before `edits` (as `apart` gives
/// them), is after them, `grown[k]` being what the first `k` grew the text
/// by: a start in an edit goes past what went there, an end in one to
/// where it begins, and one at an edge stays out of it, so nothing grows
/// by what is typed at its edges.
fn moved(at: usize, start: bool, edits: &[(Range<usize>, usize)], grown: &[isize]) -> usize {
    let k = edits.partition_point(|(range, _)| if start { range.end <= at } else { range.end < at });
    let shift = |x: usize| (x as isize + grown[k]).max(0) as usize;
    match edits.get(k) {
        Some((range, len)) if range.start < at || (start && range.start == at) => {
            if start {
                shift(range.start) + len
            } else {
                shift(range.start)
            }
        }
        _ => shift(at),
    }
}

#[cfg(test)]
mod tests {
    use crate::input::Position;

    #[test]
    fn test_diagnostic() {
        use ropey::Rope;

        use super::{Diagnostic, DiagnosticSet, DiagnosticSeverity};

        let text = Rope::from("Hello, 你好warld!\nThis is a test.\nGoodbye, world!");
        let mut diagnostics = DiagnosticSet::new(&text);

        diagnostics.push(
            Diagnostic::new(
                Position::new(0, 7)..Position::new(0, 17),
                "Spelling mistake",
            )
            .with_severity(DiagnosticSeverity::Warning),
        );
        diagnostics.push(
            Diagnostic::new(Position::new(2, 9)..Position::new(2, 14), "Syntax error")
                .with_severity(DiagnosticSeverity::Error),
        );

        assert_eq!(diagnostics.len(), 2);
        let items = diagnostics.iter().collect::<Vec<_>>();

        assert_eq!(items[0].message.as_str(), "Spelling mistake");
        assert_eq!(items[0].range, 7..19);

        assert_eq!(items[1].message.as_str(), "Syntax error");
        assert_eq!(items[1].range, 45..50);

        let items = diagnostics.range(6..48).collect::<Vec<_>>();
        assert_eq!(items.len(), 2);

        let item = diagnostics.for_offset(10).unwrap();
        assert_eq!(item.message.as_str(), "Spelling mistake");

        let item = diagnostics.for_offset(30);
        assert!(item.is_none());

        let item = diagnostics.for_offset(46).unwrap();
        assert_eq!(item.message.as_str(), "Syntax error");

        diagnostics.push(
            Diagnostic::new(Position::new(1, 5)..Position::new(1, 7), "Info message")
                .with_severity(DiagnosticSeverity::Info),
        );
        assert_eq!(diagnostics.len(), 3);

        diagnostics.clear();
        assert_eq!(diagnostics.len(), 0);
    }

    /// Marks moved with an edit are what marks put where their text went
    /// are, on the text after it: one above the edit stays, one after it
    /// on its line moves along the line, one below moves with its line,
    /// one the edit reached keeps what is left of it, and one it took
    /// whole is dropped. They were dropped at every edit, and handed back
    /// at every key (elundqvist/kvist#370).
    #[test]
    fn test_marks_move_with_an_edit() {
        use ropey::Rope;

        use super::{Diagnostic, DiagnosticSet};
        use crate::input::RopeExt as _;

        fn set(text: &str, marks: &[((u32, u32), (u32, u32))]) -> DiagnosticSet {
            let mut set = DiagnosticSet::new(&Rope::from(text));
            for (i, (from, to)) in marks.iter().enumerate() {
                set.push(Diagnostic::new(
                    Position::new(from.0, from.1)..Position::new(to.0, to.1),
                    format!("mark {i}"),
                ));
            }
            set
        }
        fn edit(set: &mut DiagnosticSet, text: &str, from: (u32, u32), to: (u32, u32), new: &str) -> String {
            let rope = Rope::from(text);
            let range = rope.position_to_offset(&Position::new(from.0, from.1))
                ..rope.position_to_offset(&Position::new(to.0, to.1));
            let edited = format!("{}{new}{}", &text[..range.start], &text[range.end..]);
            set.edit(&range, new.len(), &Rope::from(edited.as_str()));
            edited
        }

        let text = "fn a() {}\nlet x = 1;\nlet y = 2;\nlet z = 3;\n";
        let mut moved = set(
            text,
            &[((0, 3), (0, 4)), ((1, 4), (1, 5)), ((2, 0), (2, 5)), ((2, 8), (2, 9)), ((3, 4), (3, 5))],
        );
        // two lines put in on the third, over `y = `
        let text = edit(&mut moved, text, (2, 4), (2, 8), "w = 0;\nlet v = 1;\nlet u = ");
        let marks = [((0, 3), (0, 4)), ((1, 4), (1, 5)), ((2, 0), (2, 4)), ((4, 8), (4, 9)), ((5, 4), (5, 5))];
        assert_eq!(
            moved.iter().map(|e| (e.range.clone(), e.diagnostic.range.clone())).collect::<Vec<_>>(),
            set(&text, &marks).iter().map(|e| (e.range.clone(), e.diagnostic.range.clone())).collect::<Vec<_>>(),
            "an edit above some and below others"
        );
        // the `z` below typed over, and a line taken above it
        let text = edit(&mut moved, &text, (5, 4), (5, 5), "zz");
        let text = edit(&mut moved, &text, (1, 0), (2, 0), "");
        let marks = [((0, 3), (0, 4)), ((1, 0), (1, 4)), ((3, 8), (3, 9))];
        assert_eq!(
            moved.iter().map(|e| (e.range.clone(), e.diagnostic.range.clone())).collect::<Vec<_>>(),
            set(&text, &marks).iter().map(|e| (e.range.clone(), e.diagnostic.range.clone())).collect::<Vec<_>>(),
            "a mark typed over and a line taken"
        );
        let said: Vec<&str> = moved.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(said, ["mark 0", "mark 2", "mark 3"], "the `x` and the `z` taken whole");
    }

    /// The marks that end before an edit keep their places, in the tree
    /// as it was; one that begins before it and ends past it keeps its
    /// start and moves its end, and those after move: in order, so that
    /// each is found where it is now (elundqvist/kvist#383).
    #[test]
    fn test_marks_before_an_edit_are_kept_as_they_are() {
        use ropey::Rope;

        use super::{Diagnostic, DiagnosticSet};
        use crate::input::RopeExt as _;

        // a hundred lines of 8 bytes, a mark on each of the first fifty, and
        // one from line 10 to line 60
        let text: String = (0..100).map(|i| format!("line {i:02}\n")).collect();
        let mut set = DiagnosticSet::new(&Rope::from(text.as_str()));
        for i in 0..50 {
            set.push(Diagnostic::new(Position::new(i, 0)..Position::new(i, 4), format!("mark {i}")));
            if i == 10 {
                set.push(Diagnostic::new(Position::new(10, 2)..Position::new(60, 2), "long"));
            }
        }
        let before: Vec<_> = set.iter().take(30).map(|e| (e.range.clone(), e.diagnostic.clone())).collect();
        // three lines put in at the start of line 30
        let edited = format!("{}a\nb\nc\n{}", &text[..240], &text[240..]);
        let rope = Rope::from(edited.as_str());
        set.edit(&(240..240), 6, &rope);
        let after: Vec<_> = set.iter().map(|e| (e.range.clone(), e.diagnostic.clone())).collect();
        assert_eq!(after.len(), 51);
        assert!(after.windows(2).all(|w| (w[0].0.start, w[0].0.end) <= (w[1].0.start, w[1].0.end)), "in order");
        // the first thirty, the long one among them, as they were but its end
        for (k, ((range, d), (was_range, was))) in after.iter().zip(&before).enumerate() {
            if d.message.as_str() == "long" {
                assert_eq!(range.start, was_range.start);
                assert_eq!(range.end, was_range.end + 6);
                assert_eq!(d.range.end, Position::new(63, 2));
            } else {
                assert_eq!((range, d), (was_range, was), "mark {k}");
            }
        }
        // and each is found where its text is
        for e in set.iter() {
            let found = set.for_offset(e.range.start).map(|f| f.message.clone());
            assert!(found.is_some(), "{} at {:?}", e.message, e.range);
            assert_eq!(rope.offset_to_position(e.range.start), e.diagnostic.range.start, "{}", e.message);
        }
    }

    /// The marks moved by the edits of an undo or a replace-all, made one
    /// after another, are where each edit in turn puts them: one between
    /// two of them stays on its text. Moved by one edit from the first to
    /// the last, a problem's underline between two matches of a replace,
    /// or two changes of an undo, went with the text that edit took, until
    /// the problems were handed again (elundqvist/kvist#370).
    #[test]
    fn test_marks_move_with_edits_in_turn() {
        use ropey::Rope;

        use super::{apart, Diagnostic, DiagnosticSet};

        // `lines` lines of 8 bytes, a mark over the second to fifth bytes
        // of each other one
        let lines = 40;
        let text: String = (0..lines).map(|i| format!("line {i:02}\n")).collect();
        let fresh = |text: &str| {
            let mut set = DiagnosticSet::new(&Rope::from(text));
            for i in (0..lines as u32).step_by(2) {
                set.push(Diagnostic::new(Position::new(i, 1)..Position::new(i, 5), format!("mark {i}")));
            }
            set
        };
        let marks = |set: &DiagnosticSet| -> Vec<(String, std::ops::Range<usize>, std::ops::Range<Position>)> {
            set.iter().map(|e| (e.message.to_string(), e.range.clone(), e.diagnostic.range.clone())).collect()
        };
        // each step in the text the ones before it left, and the text after
        // each, from the last line up, from the first down, and in no order
        let falling: Vec<(std::ops::Range<usize>, &str)> = (0..lines).rev().filter(|i| i % 4 == 1).map(|i| (i * 8 + 5..i * 8 + 7, "\nab\n")).collect();
        let mut rising: Vec<(std::ops::Range<usize>, &str)> = Vec::new();
        let mut grown = 0isize;
        for i in (0..lines).filter(|i| i % 4 == 3) {
            let at = (i as isize * 8 + grown) as usize;
            rising.push((at..at + 4, "x"));
            grown += 1 - 4;
        }
        let tangled: Vec<(std::ops::Range<usize>, &str)> = vec![(100..104, "zz\nzz"), (20..30, ""), (95..96, "q")];
        for (what, steps) in [("falling", falling), ("rising", rising), ("tangled", tangled)] {
            // each step in turn, with the text it leaves
            let mut one_by_one = fresh(&text);
            let mut now = text.clone();
            for (range, new) in &steps {
                now = format!("{}{new}{}", &now[..range.start], &now[range.end..]);
                one_by_one.edit(range, new.len(), &Rope::from(now.as_str()));
            }
            let lengths: Vec<(std::ops::Range<usize>, usize)> = steps.iter().map(|(r, n)| (r.clone(), n.len())).collect();
            assert_eq!(apart(&lengths).is_some(), what != "tangled", "{what}");
            let mut at_once = fresh(&text);
            at_once.edit_steps(&lengths, &Rope::from(now.as_str()));
            assert_eq!(marks(&at_once), marks(&one_by_one), "{what}");
            assert!(at_once.len() > lines / 4, "{what}: those between the edits kept");
        }

        // and a mark between the first change and the last keeps its range,
        // where one edit over both took it: `{` replaced with `{ ` on the
        // lines above and below it, the last match first
        let text = "fn a() {\n    let x: i32 = \"no\";\n    if x {\n    }\n}\n";
        let replaced = text.replace('{', "{ ");
        let braces: Vec<usize> = text.match_indices('{').map(|(at, _)| at).collect();
        let mut set = DiagnosticSet::new(&Rope::from(text));
        set.push(Diagnostic::new(Position::new(1, 17)..Position::new(1, 21), "expected i32"));
        let mut covered = set.clone();
        let steps: Vec<(std::ops::Range<usize>, usize)> = braces.iter().rev().map(|&at| (at..at + 1, 2)).collect();
        set.edit_steps(&steps, &Rope::from(replaced.as_str()));
        let kept: Vec<&str> = set.iter().map(|e| &replaced[e.range.clone()]).collect();
        assert_eq!(kept, ["\"no\""]);
        assert_eq!(set.iter().next().unwrap().diagnostic.range, Position::new(1, 17)..Position::new(1, 21));
        // one edit from the first to the last took it
        covered.edit(&(braces[0]..braces[1] + 1), braces[1] + 3 - braces[0], &Rope::from(replaced.as_str()));
        assert_eq!(covered.len(), 0);
    }
}
