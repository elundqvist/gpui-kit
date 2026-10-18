use crate::input::InputModeKind;
use std::rc::Rc;
use std::{cell::RefCell, ops::Range};

use gpui::{Context, SharedString, Window};
use ropey::Rope;

use super::DisplayMap;
use crate::input::{
    DiagnosticSet, InputEdit, InputHighlighter, InputHighlighterFactory, Point, RopeExt as _,
    TabSize,
};

/// What changed, handed to the syntax highlighter.
pub(crate) struct HighlighterUpdate<'a> {
    pub(super) selected_range: &'a Range<usize>,
    pub(super) old_text: &'a Rope,
    pub(super) new_text: &'a Rope,
    pub(super) change_text: &'a str,
    /// The changes made one after another that `selected_range` and
    /// `change_text` cover, each a range of the text as the ones before it
    /// left it and how many bytes went there; empty for one edit.
    pub(super) steps: &'a [(Range<usize>, usize)],
    pub(super) force: bool,
}

/// How the input lays its text out: rows, growth, and the code-editor extras.
///
/// This does not say what *kind* of input this is — that is fixed at the type
/// level by [`crate::input::InputModeKind`], and asking it here is what used to
/// let the two disagree.
#[derive(Clone)]
pub(crate) enum LayoutMode {
    /// A plain text input mode.
    PlainText { tab: TabSize, rows: usize },
    /// An auto grow input mode.
    AutoGrow {
        rows: usize,
        min_rows: usize,
        max_rows: usize,
    },
    /// A code editor input mode.
    CodeEditor {
        tab: TabSize,
        rows: usize,
        /// Show line number
        line_number: bool,
        language: SharedString,
        indent_guides: bool,
        folding: bool,
        highlighter: Rc<RefCell<Option<Box<dyn InputHighlighter>>>>,
        highlighter_factory: Option<InputHighlighterFactory>,
        diagnostics: DiagnosticSet,
    },
}

impl Default for LayoutMode {
    fn default() -> Self {
        LayoutMode::plain_text()
    }
}

#[allow(unused)]
impl LayoutMode {
    /// Create a plain input mode with default settings.
    pub(super) fn plain_text() -> Self {
        LayoutMode::PlainText {
            tab: TabSize::default(),
            rows: 1,
        }
    }

    /// Create a code editor input mode with default settings.
    ///
    /// Starts with no language; the state sets one through its own builder.
    pub(super) fn code_editor() -> Self {
        LayoutMode::CodeEditor {
            rows: 2,
            tab: TabSize::default(),
            language: SharedString::default(),
            highlighter: Rc::new(RefCell::new(None)),
            highlighter_factory: None,
            line_number: true,
            indent_guides: true,
            folding: true,
            diagnostics: DiagnosticSet::new(&Rope::new()),
        }
    }

    /// Create an auto grow input mode with given min and max rows.
    pub(super) fn auto_grow(min_rows: usize, max_rows: usize) -> Self {
        LayoutMode::AutoGrow {
            rows: min_rows,
            min_rows,
            max_rows,
        }
    }

    /// Return true if this layout is a code editor with folding enabled.
    #[inline]
    pub(crate) fn is_folding(&self) -> bool {
        if cfg!(target_family = "wasm") {
            return false;
        }

        matches!(self, LayoutMode::CodeEditor { folding: true, .. })
    }

    #[inline]
    pub(super) fn is_auto_grow(&self) -> bool {
        matches!(self, LayoutMode::AutoGrow { .. })
    }

    pub(super) fn set_rows(&mut self, new_rows: usize) {
        match self {
            LayoutMode::PlainText { rows, .. } => {
                *rows = new_rows;
            }
            LayoutMode::CodeEditor { rows, .. } => {
                *rows = new_rows;
            }
            LayoutMode::AutoGrow {
                rows,
                min_rows,
                max_rows,
            } => {
                *rows = new_rows.clamp(*min_rows, *max_rows);
            }
        }
    }

    /// Grow the row count to fit the content.
    ///
    /// Callers gate this on the input being multi-line; a single-line field
    /// keeps its one row.
    pub(super) fn update_auto_grow(&mut self, display_map: &DisplayMap) {
        let wrapped_lines = display_map.wrap_row_count();
        self.set_rows(wrapped_lines);
    }

    /// At least 1 row be return.
    pub(super) fn rows(&self) -> usize {
        match self {
            LayoutMode::PlainText { rows, .. } => *rows,
            LayoutMode::CodeEditor { rows, .. } => *rows,
            LayoutMode::AutoGrow { rows, .. } => *rows,
        }
        .max(1)
    }

    /// At least 1 row be return.
    #[allow(unused)]
    pub(super) fn min_rows(&self) -> usize {
        match self {
            LayoutMode::AutoGrow { min_rows, .. } => *min_rows,
            _ => 1,
        }
        .max(1)
    }

    #[allow(unused)]
    pub(super) fn max_rows(&self) -> usize {
        match self {
            LayoutMode::AutoGrow { max_rows, .. } => *max_rows,
            _ => usize::MAX,
        }
    }

    /// Return false if the mode is not [`LayoutMode::CodeEditor`].
    #[inline]
    pub(super) fn line_number(&self) -> bool {
        match self {
            LayoutMode::CodeEditor { line_number, .. } => *line_number,
            _ => false,
        }
    }

    /// Update the syntax highlighter with new text.
    ///
    pub(crate) fn update_highlighter<M: InputModeKind>(
        &mut self,
        update: HighlighterUpdate<'_>,
        window: &mut Window,
        cx: &mut Context<crate::input::InputBaseState<M>>,
    ) {
        match &self {
            LayoutMode::CodeEditor {
                language,
                highlighter,
                highlighter_factory,
                folding,
                ..
            } => {
                if !update.force && highlighter.borrow().is_some() {
                    return;
                }

                let mut highlighter_ref = highlighter.borrow_mut();
                if highlighter_ref.is_none() {
                    let Some(factory) = highlighter_factory else {
                        return;
                    };
                    *highlighter_ref = factory(language);
                }

                if highlighter_ref.is_none() {
                    return;
                }
                drop(highlighter_ref);

                // A text made anew, a paste over all of it or the undo of
                // one, is new to the highlighter too, and parsed afresh:
                // handed as an edit of all of it, its trees were edited
                // node by node to be thrown away, 33 ms of a paste of
                // 1.4 MB over as much (elundqvist/kvist#384).
                let anew = update.old_text.len() > 0
                    && update.selected_range.start == 0
                    && update.selected_range.end >= update.old_text.len();
                let edit = (!anew).then(|| {
                    replacement_input_edit(
                        update.old_text,
                        update.new_text,
                        update.selected_range,
                        update.change_text,
                    )
                });
                let steps = if anew {
                    Vec::new()
                } else {
                    step_input_edits(update.old_text, update.new_text, update.steps)
                };
                M::drive_highlighter(highlighter, edit, &steps, update.new_text, *folding, window, cx);
            }
            _ => {}
        }
    }

    #[allow(unused)]
    pub(super) fn diagnostics(&self) -> Option<&DiagnosticSet> {
        match self {
            LayoutMode::CodeEditor { diagnostics, .. } => Some(diagnostics),
            _ => None,
        }
    }

    pub(super) fn diagnostics_mut(&mut self) -> Option<&mut DiagnosticSet> {
        match self {
            LayoutMode::CodeEditor { diagnostics, .. } => Some(diagnostics),
            _ => None,
        }
    }

    /// Get a reference to the highlighter (if available)
    pub(super) fn highlighter(&self) -> Option<&Rc<RefCell<Option<Box<dyn InputHighlighter>>>>> {
        match self {
            LayoutMode::CodeEditor { highlighter, .. } => Some(highlighter),
            _ => None,
        }
    }

    pub(super) fn set_highlighter_factory(&mut self, factory: InputHighlighterFactory) {
        if let LayoutMode::CodeEditor {
            highlighter_factory,
            highlighter,
            ..
        } = self
        {
            *highlighter_factory = Some(factory);
            *highlighter.borrow_mut() = None;
        }
    }

    pub(super) fn ensure_highlighter_factory(&mut self, factory: InputHighlighterFactory) {
        if let LayoutMode::CodeEditor {
            highlighter_factory,
            ..
        } = self
        {
            if highlighter_factory.is_none() {
                *highlighter_factory = Some(factory);
            }
        }
    }
}

/// Builds the tree-sitter edit for a text replacement.
///
/// Byte offsets and positions for `start`/`old_end` come from `old_text`;
/// `new_end` byte/position come from the post-edit `text`.
fn replacement_input_edit(
    old_text: &Rope,
    new_text: &Rope,
    selected_range: &Range<usize>,
    change_text: &str,
) -> InputEdit {
    let start_byte = selected_range.start.min(old_text.len());
    let old_end_byte = selected_range.end.min(old_text.len()).max(start_byte);
    let new_end_byte = (start_byte + change_text.len()).min(new_text.len());

    InputEdit {
        start_byte,
        old_end_byte,
        new_end_byte,
        start_position: old_text.offset_to_point(start_byte),
        old_end_position: old_text.offset_to_point(old_end_byte),
        new_end_position: new_text.offset_to_point(new_end_byte),
    }
}

/// `steps`, changes made one after another that turned `old_text` into
/// `new_text`, each a range of the text as the ones before it left it and
/// how many bytes went there, as the edits a highlighter's tree is moved by
/// one after another: the last of the text first, so that each is where it
/// is in `old_text` and the ones made before it lie past it. None for one
/// change, or for changes that overlap (`apart`), which the one edit that
/// covers them stands for.
fn step_input_edits(old_text: &Rope, new_text: &Rope, steps: &[(Range<usize>, usize)]) -> Vec<InputEdit> {
    if steps.len() < 2 {
        return Vec::new();
    }
    let Some(edits) = crate::input::diagnostics::apart(steps) else {
        return Vec::new();
    };
    let mut grown = 0isize;
    let mut moved: Vec<InputEdit> = edits
        .iter()
        .map(|(range, len)| {
            let start = range.start.min(old_text.len());
            let old_end = range.end.min(old_text.len()).max(start);
            // what went there, where it is in the new text
            let at = ((start as isize + grown).max(0) as usize).min(new_text.len());
            let end = (at + len).min(new_text.len());
            grown += *len as isize - (old_end - start) as isize;
            let start_position = old_text.offset_to_point(start);
            let (from, to) = (new_text.offset_to_point(at), new_text.offset_to_point(end));
            let new_end_position = if to.row == from.row {
                Point::new(start_position.row, start_position.column + to.column - from.column)
            } else {
                Point::new(start_position.row + to.row - from.row, to.column)
            };
            InputEdit {
                start_byte: start,
                old_end_byte: old_end,
                new_end_byte: start + (end - at),
                start_position,
                old_end_position: old_text.offset_to_point(old_end),
                new_end_position,
            }
        })
        .collect();
    moved.reverse();
    moved
}

#[cfg(test)]
mod tests {
    use ropey::Rope;

    use super::{replacement_input_edit, step_input_edits};
    use crate::input::{DiagnosticSet, Point, RopeExt as _, TabSize, mode::LayoutMode};

    /// Changes made one after another, as a rename, a replace of every
    /// match and an undo or a redo of them make them, are handed to the
    /// highlighter as edits its tree is moved by one after another: each
    /// where it is in the text the ones applied before it left, its points
    /// those of that text, so that the tree ends as an edit of the new text
    /// (elundqvist/kvist#404). A change that puts lines in or takes them,
    /// and one on a line another changed too, among them.
    #[test]
    fn test_changes_are_edits_one_after_another() {
        let old = "fn sum(sum: u32) {\n    sum\n}\n\nfn other() {\n    let sum = 1;\n    sum + sum\n}\n";
        let changes: Vec<(std::ops::Range<usize>, &str)> = old
            .match_indices("sum")
            .enumerate()
            .map(|(i, (at, _))| (at..at + 3, ["total", "x", "a\nb", ""][i % 4]))
            .collect();
        // made the last first, as a replace of every match makes them, and
        // the first first, as a redo makes them, each a range of the text the
        // ones before it left
        let last_first: Vec<(std::ops::Range<usize>, usize)> =
            changes.iter().rev().map(|(range, with)| (range.clone(), with.len())).collect();
        let mut grown = 0isize;
        let first_first: Vec<(std::ops::Range<usize>, usize)> = changes
            .iter()
            .map(|(range, with)| {
                let at = |x: usize| (x as isize + grown) as usize;
                let step = (at(range.start)..at(range.end), with.len());
                grown += with.len() as isize - range.len() as isize;
                step
            })
            .collect();
        let mut new = old.to_string();
        for (range, with) in changes.iter().rev() {
            new.replace_range(range.clone(), with);
        }
        let (old_rope, new_rope) = (Rope::from_str(old), Rope::from_str(&new));
        for steps in [&last_first, &first_first] {
            let edits = step_input_edits(&old_rope, &new_rope, steps);
            assert_eq!(edits.len(), changes.len());
            // applied one after another, the last of the text first
            let mut text = old.to_string();
            for (edit, (_, with)) in edits.iter().zip(changes.iter().rev()) {
                let before = Rope::from_str(&text);
                assert_eq!(edit.start_position, before.offset_to_point(edit.start_byte));
                assert_eq!(edit.old_end_position, before.offset_to_point(edit.old_end_byte));
                assert_eq!(edit.new_end_byte - edit.start_byte, with.len());
                text.replace_range(edit.start_byte..edit.old_end_byte, with);
                let after = Rope::from_str(&text);
                assert_eq!(edit.new_end_position, after.offset_to_point(edit.new_end_byte));
            }
            assert_eq!(text, new);
        }
        // one change, and changes that overlap, are the one edit
        assert!(step_input_edits(&old_rope, &new_rope, &last_first[..1]).is_empty());
        assert!(step_input_edits(&old_rope, &new_rope, &[(2..5, 1), (0..3, 1)]).is_empty());
    }

    #[test]
    fn test_replacement_input_edit_backspace_at_end_uses_old_range() {
        let old_text = Rope::from_str("-=");
        let text = Rope::from_str("-");
        let edit = replacement_input_edit(&old_text, &text, &(1..2), "");

        assert_eq!(edit.start_byte, 1);
        assert_eq!(edit.old_end_byte, 2);
        assert_eq!(edit.new_end_byte, 1);
        assert_eq!(edit.start_position, Point::new(0, 1));
        assert_eq!(edit.old_end_position, Point::new(0, 2));
        assert_eq!(edit.new_end_position, Point::new(0, 1));
    }

    #[test]
    fn test_code_editor() {
        let mode = LayoutMode::code_editor();
        assert_eq!(mode.line_number(), true);
        assert_eq!(mode.has_indent_guides(), true);
        assert_eq!(mode.max_rows(), usize::MAX);
        assert_eq!(mode.min_rows(), 1);
        assert_eq!(mode.is_folding(), true);

        let mode = LayoutMode::CodeEditor {
            line_number: false,
            indent_guides: false,
            folding: false,
            rows: 0,
            tab: Default::default(),
            language: "rust".into(),
            highlighter: Default::default(),
            highlighter_factory: None,
            diagnostics: DiagnosticSet::new(&Rope::new()),
        };
        assert_eq!(mode.line_number(), false);
        assert_eq!(mode.has_indent_guides(), false);
        assert_eq!(mode.min_rows(), 1);
        assert_eq!(mode.is_folding(), false);
    }

    #[test]
    fn test_plain() {
        let mode = LayoutMode::PlainText {
            tab: TabSize::default(),
            rows: 5,
        };
        assert_eq!(mode.line_number(), false);
        assert_eq!(mode.rows(), 5);
        assert_eq!(mode.max_rows(), usize::MAX);
        assert_eq!(mode.min_rows(), 1);

        let mode = LayoutMode::plain_text();
        assert_eq!(mode.line_number(), false);
        assert_eq!(mode.rows(), 1);
        assert_eq!(mode.min_rows(), 1);
    }

    #[test]
    fn test_auto_grow() {
        let mut mode = LayoutMode::auto_grow(2, 5);
        assert_eq!(mode.line_number(), false);
        assert_eq!(mode.rows(), 2);
        assert_eq!(mode.max_rows(), 5);
        assert_eq!(mode.min_rows(), 2);

        mode.set_rows(4);
        assert_eq!(mode.rows(), 4);

        mode.set_rows(1);
        assert_eq!(mode.rows(), 2);

        mode.set_rows(10);
        assert_eq!(mode.rows(), 5);
    }
}
