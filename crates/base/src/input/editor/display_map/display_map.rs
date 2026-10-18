/// DisplayMap: Public facade for Editor/Input display mapping.
///
/// This combines WrapMap and FoldMap to provide a unified API:
/// - BufferPoint ↔ DisplayPoint conversion
/// - Fold management (candidates, toggle, query)
/// - Automatic projection updates on text/layout changes
use std::ops::Range;

use gpui::{App, Font, Pixels};
use ropey::Rope;
use sum_tree::SumTree;

use super::fold_map::FoldMap;
use super::folding::FoldRange;
pub use super::text_wrapper::WrappingIndent;
use super::text_wrapper::{LineItem, TextWrapper, WrapDisplayPoint};
use super::wrap_map::WrapMap;
use super::{BufferPoint, DisplayPoint};
use crate::input::Point as TreeSitterPoint;
use crate::input::display_map::WrapPoint;
use crate::input::rope_ext::RopeExt as _;

/// The lines of a whole text as a display map that does not wrap lays
/// them out, made with no [`App`], so on any thread, for
/// [`DisplayMap::set_prepared_text`].
pub(crate) struct PreparedLines(SumTree<LineItem>);

impl PreparedLines {
    pub(crate) fn of(text: &Rope) -> Self {
        Self(TextWrapper::unwrapped_lines(text))
    }
}

/// What a display map had before [`DisplayMap::set_prepared_text`], to be
/// freed off the UI thread: for a large text, dropping it takes a while.
pub(crate) struct Replaced(
    #[allow(dead_code)] Rope,
    #[allow(dead_code)] SumTree<LineItem>,
);

/// DisplayMap is the main interface for Editor/Input coordinate mapping.
///
/// It manages the two-layer projection:
/// 1. Buffer → Wrap (soft-wrapping)
/// 2. Wrap → Display (folding)
///
/// Editor/Input only needs to work with BufferPoint and DisplayPoint.
pub struct DisplayMap {
    wrap_map: WrapMap,
    fold_map: FoldMap,
}

impl DisplayMap {
    pub fn new(font: Font, font_size: Pixels, wrap_width: Option<Pixels>) -> Self {
        Self {
            wrap_map: WrapMap::new(font, font_size, wrap_width),
            fold_map: FoldMap::new(),
        }
    }

    // ==================== Core Coordinate Mapping ====================

    /// Convert buffer position to display position
    pub fn buffer_pos_to_display_pos(&self, pos: BufferPoint) -> DisplayPoint {
        // Buffer → Wrap
        let wrap_pos = self.wrap_map.buffer_pos_to_wrap_pos(pos);

        // Wrap → Display
        if let Some(display_row) = self.fold_map.wrap_row_to_display_row(wrap_pos.row) {
            DisplayPoint::new(display_row, wrap_pos.col)
        } else {
            // Cursor is in a folded region, find nearest visible row
            let display_row = self.fold_map.nearest_visible_display_row(wrap_pos.row);
            DisplayPoint::new(display_row, 0) // Column 0 at fold boundary
        }
    }

    /// Convert display position to buffer position
    pub fn display_pos_to_buffer_pos(&self, pos: DisplayPoint) -> BufferPoint {
        // Display → Wrap
        let wrap_row = self.fold_map.display_row_to_wrap_row(pos.row).unwrap_or(0);

        // Wrap → Buffer
        let wrap_pos = WrapPoint::new(wrap_row, pos.col);
        self.wrap_map.wrap_pos_to_buffer_pos(wrap_pos)
    }

    /// Get total number of visible display rows
    #[inline]
    pub fn display_row_count(&self) -> usize {
        self.fold_map.display_row_count()
    }

    /// Get the buffer line for a given display row
    pub fn display_row_to_buffer_line(&self, display_row: usize) -> usize {
        // Display → Wrap
        let wrap_row = self
            .fold_map
            .display_row_to_wrap_row(display_row)
            .unwrap_or(0);

        // Wrap → Buffer line
        self.wrap_map.wrap_row_to_buffer_line(wrap_row)
    }

    /// Get the display row range for a buffer line: [start, end)
    /// Returns None if the buffer line is completely hidden
    pub fn buffer_line_to_display_row_range(&self, line: usize) -> Option<Range<usize>> {
        // Buffer line → Wrap row range
        let wrap_row_range = self.wrap_map.buffer_line_to_wrap_row_range(line);

        // Find first and last visible display rows in this range
        let mut first_display_row = None;
        let mut last_display_row = None;

        for wrap_row in wrap_row_range {
            if let Some(display_row) = self.fold_map.wrap_row_to_display_row(wrap_row) {
                if first_display_row.is_none() {
                    first_display_row = Some(display_row);
                }
                last_display_row = Some(display_row);
            }
        }

        if let (Some(start), Some(end)) = (first_display_row, last_display_row) {
            Some(start..end + 1)
        } else {
            None // Completely folded
        }
    }

    /// Check if a buffer line is completely hidden
    #[inline]
    pub fn is_buffer_line_hidden(&self, line: usize) -> bool {
        self.buffer_line_to_display_row_range(line).is_none()
    }

    /// First display row of a buffer line. If the line is fully folded, returns the
    /// nearest visible display row.
    pub fn buffer_line_to_display_row(&self, line: usize) -> usize {
        match self.buffer_line_to_display_row_range(line) {
            Some(range) => range.start,
            None => {
                let wrap_row = self.wrap_map.buffer_line_to_first_wrap_row(line);
                self.fold_map.nearest_visible_display_row(wrap_row)
            }
        }
    }

    /// Set fold candidates (from tree-sitter/LSP)
    pub fn set_fold_candidates(&mut self, candidates: Vec<FoldRange>) {
        self.fold_map.set_candidates(candidates);
        self.rebuild_fold_projection();
    }

    /// Set a fold at the given start_line (must be in candidates)
    pub fn set_folded(&mut self, start_line: usize, folded: bool) {
        self.fold_map.set_folded(start_line, folded);
        self.rebuild_fold_projection();
    }

    /// Toggle fold at the given start_line
    pub fn toggle_fold(&mut self, start_line: usize) {
        self.fold_map.toggle_fold(start_line);
        self.rebuild_fold_projection();
    }

    /// Check if a line is currently folded
    #[inline]
    pub fn is_folded_at(&self, start_line: usize) -> bool {
        self.fold_map.is_folded_at(start_line)
    }

    /// Check if a line is a fold candidate
    #[inline]
    pub fn is_fold_candidate(&self, start_line: usize) -> bool {
        self.fold_map.is_fold_candidate(start_line)
    }

    /// Get all currently folded ranges
    #[inline]
    pub fn folded_ranges(&self) -> &[FoldRange] {
        self.fold_map.folded_ranges()
    }

    /// Clear all folds
    pub fn clear_folds(&mut self) {
        self.fold_map.clear_folds();
        self.rebuild_fold_projection();
    }

    // ==================== Text and Layout Updates ====================

    /// Adjust folds and candidates for a text edit before updating the wrap map.
    ///
    /// Must be called with the OLD text (before replacement) and the edit range/new_text
    /// so we can compute which old lines were affected.
    pub fn adjust_folds_for_edit(&mut self, old_text: &Rope, range: &Range<usize>, new_text: &str) {
        if self.fold_map.folded_ranges().is_empty() && self.fold_map.fold_candidates().is_empty() {
            return;
        }
        let range = range.start.min(old_text.len())..range.end.min(old_text.len());
        let put = new_text.bytes().filter(|b| *b == b'\n').count();
        // what follows the edit begins a line where what went in ends in
        // a line break, or where nothing went in at the start of a line
        let line_after = match new_text.as_bytes().last() {
            Some(last) => *last == b'\n',
            None => range.start == old_text.line_start_offset(old_text.offset_to_point(range.start).row),
        };
        let lines = edit_lines(old_text, &range, put, line_after);
        self.fold_map.adjust_folds_for_edits(&[lines]);
    }

    /// Adjust folds and candidates for edits made one after another, as a
    /// replace-all, a rename and their undo make them: `steps`, each a
    /// range of the text as the ones before it left it and how many bytes
    /// went there, turned `old_text` into `new_text`, and `range` of
    /// `old_text` is the one edit that covers them, `inserted` what is there
    /// now. Each step moves and keeps the folds as it would alone
    /// (`FoldMap::adjust_folds_for_edits`). Taken over the one edit,
    /// Replace All opened every fold between its first match and its last,
    /// and its undo did again (elundqvist/kvist#384). Steps in another
    /// order, that overlap (`apart`), are taken as the one edit.
    pub(crate) fn adjust_folds_for_steps(
        &mut self,
        old_text: &Rope,
        new_text: &Rope,
        range: &Range<usize>,
        inserted: &str,
        steps: &[(Range<usize>, usize)],
    ) {
        if self.fold_map.folded_ranges().is_empty() && self.fold_map.fold_candidates().is_empty() {
            return;
        }
        let edits = (steps.len() > 1)
            .then(|| crate::input::diagnostics::apart(steps))
            .flatten();
        let Some(edits) = edits else {
            return self.adjust_folds_for_edit(old_text, range, inserted);
        };
        // each edit's lines in the old text, and those of what it put in,
        // where that is in the new text: past what the edits before it grew
        // the text by
        let mut grown = 0isize;
        let mut reached = 0;
        let lines: Vec<(usize, usize, isize)> = edits
            .iter()
            .map(|(range, len)| {
                let range = range.start.min(old_text.len())..range.end.min(old_text.len());
                let at = ((range.start as isize + grown).max(0) as usize).min(new_text.len());
                grown += *len as isize - range.len() as isize;
                let end = (at + len).min(new_text.len());
                let put = new_text.offset_to_point(end).row - new_text.offset_to_point(at).row;
                let line_after = end == 0 || new_text.byte(end - 1) == b'\n';
                let (start, mut past, delta) = edit_lines(old_text, &range, put, line_after);
                // an edit that ends where the one before it left a line
                // the latter reaches: so that the lines reached never go back
                past = past.max(reached);
                reached = past;
                (start, past, delta)
            })
            .collect();
        self.fold_map.adjust_folds_for_edits(&lines);
    }

    /// Incrementally update fold candidates after a text edit.
    ///
    /// Extracts new fold candidates only within the edited byte range
    /// and merges them with existing (already adjusted) candidates.
    pub fn update_fold_candidates_for_edit(
        &mut self,
        extract_fold_ranges: impl FnOnce(Range<usize>, &Rope) -> Vec<FoldRange>,
        edit_byte_range: Range<usize>,
        new_text: &Rope,
    ) {
        let new_start_line = new_text.offset_to_point(edit_byte_range.start).row;
        let new_end_line = new_text
            .offset_to_point(edit_byte_range.end.min(new_text.len()))
            .row;

        let new_candidates = extract_fold_ranges(edit_byte_range, new_text);
        self.fold_map
            .merge_candidates_for_edit(new_start_line, new_end_line, new_candidates);
        // a fold kept through the edit takes the lines it covers now
        self.rebuild_fold_projection();
    }

    /// Update text (incremental or full)
    pub fn on_text_changed(
        &mut self,
        changed_text: &Rope,
        range: &Range<usize>,
        new_text: &Rope,
        cx: &mut App,
    ) {
        self.wrap_map
            .on_text_changed(changed_text, range, new_text, cx);
        self.rebuild_fold_projection();
    }

    /// Update layout parameters (wrap width or font)
    pub fn on_layout_changed(&mut self, wrap_width: Option<Pixels>, cx: &mut App) {
        self.wrap_map.on_layout_changed(wrap_width, cx);
        self.rebuild_fold_projection();
    }

    /// Set the wrapping indent for continuation lines.
    pub fn set_wrapping_indent(&mut self, wrapping_indent: WrappingIndent, cx: &mut App) {
        self.wrap_map.set_wrapping_indent(wrapping_indent, cx);
        self.rebuild_fold_projection();
    }

    /// Set font parameters
    pub fn set_font(&mut self, font: Font, font_size: Pixels, cx: &mut App) {
        self.wrap_map.set_font(font, font_size, cx);
        self.rebuild_fold_projection();
    }

    /// Ensure text is prepared (initializes wrapper if needed)
    pub fn ensure_text_prepared(&mut self, text: &Rope, cx: &mut App) {
        let did_initialize = self.wrap_map.ensure_text_prepared(text, cx);
        if did_initialize {
            self.rebuild_fold_projection();
        }
    }

    /// Take a whole new text with its lines, made off the UI thread
    /// ([`crate::input::PreparedValue`]): its folds and fold candidates go,
    /// as a replacement of the whole text takes them. Hands back the text
    /// and the lines it had, for the caller to free off the UI thread.
    pub(crate) fn set_prepared_text(
        &mut self,
        text: &Rope,
        lines: PreparedLines,
        cx: &mut App,
    ) -> Replaced {
        self.fold_map.clear_folds();
        self.fold_map.set_candidates(Vec::new());
        let (old_text, old_lines) = self.wrap_map.set_prepared(text, lines.0, cx);
        self.rebuild_fold_projection();
        Replaced(old_text, old_lines)
    }

    /// Initialize with text
    pub fn set_text(&mut self, text: &Rope, cx: &mut App) {
        self.wrap_map.set_text(text, cx);
        self.rebuild_fold_projection();
    }

    // ==================== Internal Helpers ====================

    /// Rebuild fold projection after wrap_map or fold state changes
    /// Only rebuilds if there are actually folded ranges
    fn rebuild_fold_projection(&mut self) {
        if !self.fold_map.folded_ranges().is_empty() {
            self.fold_map.rebuild(&self.wrap_map);
        } else {
            // No active folds: identity mapping (wrap_row == display_row).
            // Just update cached count so query methods work without Vec allocation.
            self.fold_map
                .mark_dirty_with_wrap_count(self.wrap_map.wrap_row_count());
        }
    }

    // ==================== Wrap Display Point Operations ====================

    /// Convert byte offset to wrap display point (with soft wrap info).
    #[inline]
    pub(crate) fn offset_to_wrap_display_point(&self, offset: usize) -> WrapDisplayPoint {
        self.wrap_map.wrapper().offset_to_display_point(offset)
    }

    /// Like [`Self::offset_to_wrap_display_point`], but honours the caret's line-end affinity so
    /// an offset on a soft wrap boundary resolves to the row the caret is drawn on.
    #[inline]
    pub(crate) fn offset_to_wrap_display_point_with_affinity(
        &self,
        offset: usize,
        line_end_affinity: bool,
    ) -> WrapDisplayPoint {
        self.wrap_map
            .wrapper()
            .offset_to_display_point_with_affinity(offset, line_end_affinity)
    }

    /// Convert wrap display point to byte offset.
    #[inline]
    pub(crate) fn wrap_display_point_to_offset(&self, point: WrapDisplayPoint) -> usize {
        self.wrap_map.wrapper().display_point_to_offset(point)
    }

    /// Convert wrap display point to TreeSitterPoint (buffer line/col).
    #[inline]
    pub(crate) fn wrap_display_point_to_point(&self, point: WrapDisplayPoint) -> TreeSitterPoint {
        self.wrap_map.wrapper().display_point_to_point(point)
    }

    /// Convert a wrap row to a display row (skipping folded rows).
    /// Returns None if the wrap row is folded.
    #[inline]
    pub fn wrap_row_to_display_row(&self, wrap_row: usize) -> Option<usize> {
        self.fold_map.wrap_row_to_display_row(wrap_row)
    }

    /// Find the nearest visible display row for a given wrap row.
    #[inline]
    pub fn nearest_visible_display_row(&self, wrap_row: usize) -> usize {
        self.fold_map.nearest_visible_display_row(wrap_row)
    }

    /// Convert a display row to a wrap row.
    #[inline]
    pub fn display_row_to_wrap_row(&self, display_row: usize) -> Option<usize> {
        self.fold_map.display_row_to_wrap_row(display_row)
    }

    /// Get the longest row index (by byte length).
    #[inline]
    pub(crate) fn longest_row(&self) -> usize {
        self.wrap_map.wrapper().longest_row()
    }

    // ==================== Access Methods ====================

    /// Get the line item by buffer row index.
    #[inline]
    pub(crate) fn line(&self, row: usize) -> Option<&LineItem> {
        self.wrap_map.line(row)
    }

    /// Get the rope text
    #[inline]
    pub fn text(&self) -> &Rope {
        self.wrap_map.text()
    }

    /// Calculate how many wrap rows of a buffer line are visible (not folded)
    #[inline]
    pub fn visible_wrap_row_count_for_buffer_line(&self, line: usize) -> usize {
        self.wrap_map
            .visible_wrap_row_count_for_line(line, &self.fold_map)
    }

    /// Get the wrap row count (before folding)
    #[inline]
    pub fn wrap_row_count(&self) -> usize {
        self.wrap_map.wrap_row_count()
    }

    /// Get the buffer line count (logical lines)
    #[inline]
    pub fn buffer_line_count(&self) -> usize {
        self.wrap_map.buffer_line_count()
    }
}

/// The lines of `old_text` an edit of `range` reaches, from the line it
/// begins on to the one before the second, and how many lines it put in
/// or took: `put` line breaks went in, and `line_after` says whether what
/// follows the edit begins a line in the text after it. One that ends at
/// the start of a line, where what follows it still begins a line, reaches
/// no further than the line before: deleting the line above a fold leaves
/// the fold's first line as it was, and so do lines put in before it, where
/// the fold was taken as edited and opened (elundqvist/kvist#404). One that
/// took nothing and put in whole lines reaches none (the two are the same),
/// and lies before the line it was put in at.
fn edit_lines(
    old_text: &Rope,
    range: &Range<usize>,
    put: usize,
    line_after: bool,
) -> (usize, usize, isize) {
    let start = old_text.offset_to_point(range.start).row;
    let end = old_text.offset_to_point(range.end).row;
    let delta = put as isize - (end - start) as isize;
    let at_line_start = range.end == old_text.line_start_offset(end);
    let nothing = range.is_empty() && put == 0;
    let past = if at_line_start && line_after && !nothing { end } else { end + 1 };
    (start, past, delta)
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use gpui::px;
    use ropey::Rope;

    use super::DisplayMap;
    use crate::input::RopeExt as _;
    use crate::input::display_map::FoldRange;

    /// `text` after `steps`, made one after another, each a range of the
    /// text the ones before it left and what goes there; with the one
    /// edit that covers them, as the editor hands it on, and the steps as
    /// the editor hands them to what follows the text.
    fn made(
        text: &Rope,
        steps: &[(Range<usize>, String)],
    ) -> (Rope, Range<usize>, String, Vec<(Range<usize>, usize)>) {
        let mut after = text.clone();
        let (mut head, mut tail) = (usize::MAX, usize::MAX);
        for (range, new) in steps {
            head = head.min(range.start);
            tail = tail.min(after.len() - range.end);
            after.replace(range.clone(), new);
        }
        let inserted = after.slice(head..after.len() - tail).to_string();
        let lens = steps.iter().map(|(range, new)| (range.clone(), new.len())).collect();
        (after, head..text.len() - tail, inserted, lens)
    }

    /// Folds and candidates as lines, to compare.
    fn lines(map: &DisplayMap) -> (Vec<(usize, usize)>, Vec<(usize, usize)>) {
        let lines = |ranges: &[FoldRange]| {
            ranges.iter().map(|f| (f.start_line, f.end_line)).collect::<Vec<_>>()
        };
        (lines(map.fold_map.folded_ranges()), lines(map.fold_map.fold_candidates()))
    }

    /// `map` after each of `steps` of `text` taken as an edit of its own.
    fn one_by_one(map: &mut DisplayMap, text: &Rope, steps: &[(Range<usize>, String)]) {
        let mut now = text.clone();
        for (range, new) in steps {
            map.adjust_folds_for_edit(&now, range, new);
            now.replace(range.clone(), new);
        }
    }

    /// Edits apart, the last first as a replace-all makes them and the
    /// first first as its undo does, take and move the folds as each made
    /// as an edit of its own does: a fold an edit's lines reach goes, and
    /// the rest move by the lines the edits before them put in or took.
    /// Edits that overlap are taken as the one edit over them all.
    #[test]
    fn test_folds_follow_steps_as_each_step_alone() {
        // sixty lines, a fold of three lines every four from line 1
        let text = Rope::from((0..60).map(|i| format!("line {i}\n")).collect::<String>());
        let folds: Vec<FoldRange> = (0..14).map(|k| FoldRange::new(1 + 4 * k, 3 + 4 * k)).collect();
        let fresh = || {
            let mut map = DisplayMap::new(gpui::font("Courier"), px(14.), None);
            map.fold_map.set_candidates(folds.clone());
            for fold in &folds {
                map.fold_map.set_folded(fold.start_line, true);
            }
            map
        };
        let at = |line: usize, column: usize| text.line_start_offset(line) + column;
        // a word on line 0 joined to line 1, lines 5 and 6 taken, two
        // lines put in at 12, the end of 24 joined to 25, two lines put in
        // on 40, two words on 50: the last first
        let steps: Vec<(Range<usize>, String)> = [
            (at(50, 4)..at(50, 6), "y"),
            (at(50, 0)..at(50, 2), "x"),
            (at(40, 2)..at(40, 4), "a\nb\nc"),
            (at(24, 7)..at(25, 0), " "),
            (at(12, 0)..at(12, 0), "x\ny\n"),
            (at(5, 0)..at(7, 0), ""),
            (at(0, 4)..at(1, 2), "-"),
        ]
        .into_iter()
        .map(|(range, new)| (range, new.to_string()))
        .collect();
        let (after, covering, inserted, lens) = made(&text, &steps);
        let mut each = fresh();
        one_by_one(&mut each, &text, &steps);
        let mut together = fresh();
        together.adjust_folds_for_steps(&text, &after, &covering, &inserted, &lens);
        assert_eq!(lines(&together), lines(&each), "the last first");
        // the folds on lines 1, 5 and 25 are taken, whose first lines the
        // edits break or join, the rest moved; the one on 49 hides the
        // line of the two words, which keeps it
        let kept = [(6, 8), (12, 14), (16, 18), (20, 22), (27, 29), (31, 33), (35, 37), (41, 43), (45, 47), (49, 51), (53, 55)];
        assert_eq!(lines(&together).0, kept);

        // their undo: each the other way, the first first, on the text the
        // ones before it left
        let mut now = text.clone();
        let mut undo = Vec::new();
        for (range, new) in &steps {
            undo.push((range.start..range.start + new.len(), now.slice(range.clone()).to_string()));
            now.replace(range.clone(), new);
        }
        undo.reverse();
        let (back, covering, inserted, lens) = made(&after, &undo);
        assert_eq!(back.to_string(), text.to_string());
        one_by_one(&mut each, &after, &undo);
        together.adjust_folds_for_steps(&after, &back, &covering, &inserted, &lens);
        assert_eq!(lines(&together), lines(&each), "the first first");

        // a rename in each of three folds, on a first line, a hidden line
        // and a last line, and the line above a fourth taken: every fold
        // stays, through the edits and their undo
        let rename: Vec<(Range<usize>, String)> = [
            (at(41, 0)..at(41, 4), "row"),
            (at(34, 0)..at(34, 4), "row"),
            (at(31, 0)..at(31, 4), "row"),
            (at(8, 0)..at(9, 0), ""),
        ]
        .into_iter()
        .map(|(range, new)| (range, new.to_string()))
        .collect();
        let (after, covering, inserted, lens) = made(&text, &rename);
        let mut together = fresh();
        together.adjust_folds_for_steps(&text, &after, &covering, &inserted, &lens);
        let mut moved: Vec<(usize, usize)> = folds.iter().map(|f| (f.start_line, f.end_line)).collect();
        for fold in &mut moved {
            if fold.0 > 8 {
                *fold = (fold.0 - 1, fold.1 - 1);
            }
        }
        assert_eq!(lines(&together).0, moved, "renamed in three, and the line above the fold on 9 taken");
        let mut each = fresh();
        one_by_one(&mut each, &text, &rename);
        assert_eq!(lines(&together), lines(&each), "renamed, each alone");

        // overlapping: taken as the one edit over them
        let tangled: Vec<(Range<usize>, String)> =
            vec![(at(20, 0)..at(20, 2), "ab\n".into()), (at(20, 1)..at(20, 3), "".into())];
        let (after, covering, inserted, lens) = made(&text, &tangled);
        let mut one = fresh();
        one.adjust_folds_for_edit(&text, &covering, &inserted);
        let mut together = fresh();
        together.adjust_folds_for_steps(&text, &after, &covering, &inserted, &lens);
        assert_eq!(lines(&together), lines(&one), "tangled");
    }

    /// What one edit does to a fold: lines put in or taken among those it
    /// hides grow or shrink it, an edit on its first or its last line that
    /// puts in no line keeps it, lines put in before its first line or
    /// taken above it move it, and an edit across an edge of it, or one that
    /// breaks its first line, takes it.
    #[test]
    fn test_an_edit_keeps_a_fold_it_lies_within() {
        let text = Rope::from((0..20).map(|i| format!("line {i}\n")).collect::<String>());
        let at = |line: usize, column: usize| text.line_start_offset(line) + column;
        let folded = |range: Range<usize>, new: &str| {
            let mut map = DisplayMap::new(gpui::font("Courier"), px(14.), None);
            map.fold_map.set_candidates(vec![FoldRange::new(5, 10)]);
            map.fold_map.set_folded(5, true);
            map.adjust_folds_for_edit(&text, &range, new);
            lines(&map).0
        };
        assert_eq!(folded(at(7, 0)..at(7, 4), "row"), [(5, 10)], "a word on a hidden line");
        assert_eq!(folded(at(7, 0)..at(9, 0), ""), [(5, 8)], "two hidden lines taken");
        assert_eq!(folded(at(6, 2)..at(6, 2), "a\nb\n"), [(5, 12)], "two lines put in on a hidden one");
        assert_eq!(folded(at(10, 0)..at(10, 0), "x\n"), [(5, 11)], "a line put in before its last");
        assert_eq!(folded(at(5, 0)..at(5, 4), "row"), [(5, 10)], "a word on its first line");
        assert_eq!(folded(at(10, 0)..at(10, 4), "row"), [(5, 10)], "a word on its last line");
        assert_eq!(folded(at(4, 0)..at(5, 0), ""), [(4, 9)], "the line above it taken");
        assert_eq!(folded(at(2, 0)..at(5, 0), "x\n"), [(3, 8)], "lines above it made one");
        assert_eq!(folded(at(5, 0)..at(5, 0), "x\ny\n"), [(7, 12)], "lines put in before its first");
        assert_eq!(folded(at(4, 6)..at(4, 6), "\n"), [(6, 11)], "a line broken above it");
        assert_eq!(folded(at(12, 0)..at(13, 0), ""), [(5, 10)], "a line taken below it");
        assert!(folded(at(5, 6)..at(5, 6), "\n").is_empty(), "its first line broken");
        assert!(folded(at(4, 6)..at(5, 0), "").is_empty(), "its first line joined to the one above");
        assert!(folded(at(4, 0)..at(6, 0), "").is_empty(), "its first line taken");
        assert!(folded(at(9, 2)..at(11, 0), "").is_empty(), "across its last line");
        assert!(folded(at(10, 6)..at(10, 6), "\n").is_empty(), "its last line broken");
    }
}
