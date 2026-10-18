/// FoldMap: Folding projection layer (Wrap rows → Display rows).
///
/// This module manages code folding by:
/// - Filtering out wrap rows that belong to folded regions
/// - Maintaining bidirectional mapping: wrap_row ↔ display_row
/// - Handling fold state changes and rebuilding the projection
use super::folding::FoldRange;
use super::wrap_map::WrapMap;

/// FoldMap projects wrap rows to display rows by hiding folded regions.
pub(super) struct FoldMap {
    /// Mapping: display_row → wrap_row
    /// index = display_row, value = actual wrap_row
    visible_wrap_rows: Vec<usize>,

    /// Reverse mapping: wrap_row → display_row
    /// index = wrap_row, value = Some(display_row) if visible, None if folded
    wrap_row_to_display_row: Vec<Option<usize>>,

    /// Candidate fold ranges (from tree-sitter/LSP)
    /// Sorted by start_line, unique start_line
    candidates: Vec<FoldRange>,

    /// Currently folded ranges
    /// Subset of candidates, sorted by start_line
    folded: Vec<FoldRange>,

    /// Flag indicating if the fold projection needs rebuilding
    /// Used for lazy evaluation to avoid expensive rebuilds on every text change
    needs_rebuild: bool,

    /// Cached wrap_row_count from last rebuild
    /// Used to detect if WrapMap changed and rebuild is needed
    cached_wrap_row_count: usize,
}

impl FoldMap {
    pub(super) fn new() -> Self {
        Self {
            visible_wrap_rows: Vec::new(),
            wrap_row_to_display_row: Vec::new(),
            candidates: Vec::new(),
            folded: Vec::new(),
            needs_rebuild: true,
            cached_wrap_row_count: 0,
        }
    }

    /// Update cached wrap_row_count without full rebuild.
    /// Used when no folds are active (identity mapping assumed).
    pub(super) fn mark_dirty_with_wrap_count(&mut self, wrap_row_count: usize) {
        self.needs_rebuild = true;
        self.cached_wrap_row_count = wrap_row_count;
    }

    /// Get total number of visible display rows
    pub(super) fn display_row_count(&self) -> usize {
        if self.folded.is_empty() {
            return self.cached_wrap_row_count;
        }
        self.visible_wrap_rows.len()
    }

    /// Convert wrap_row to display_row
    /// Returns None if the wrap_row is hidden by folding
    pub(super) fn wrap_row_to_display_row(&self, wrap_row: usize) -> Option<usize> {
        if self.folded.is_empty() {
            return if wrap_row < self.cached_wrap_row_count {
                Some(wrap_row)
            } else {
                None
            };
        }
        self.wrap_row_to_display_row
            .get(wrap_row)
            .copied()
            .flatten()
    }

    /// Convert display_row to wrap_row
    pub(super) fn display_row_to_wrap_row(&self, display_row: usize) -> Option<usize> {
        if self.folded.is_empty() {
            return if display_row < self.cached_wrap_row_count {
                Some(display_row)
            } else {
                None
            };
        }
        self.visible_wrap_rows.get(display_row).copied()
    }

    /// Find the nearest visible display_row for a given wrap_row
    pub(super) fn nearest_visible_display_row(&self, wrap_row: usize) -> usize {
        if self.folded.is_empty() {
            return wrap_row.min(self.cached_wrap_row_count.saturating_sub(1));
        }

        if let Some(dr) = self.wrap_row_to_display_row(wrap_row) {
            return dr;
        }

        match self.visible_wrap_rows.binary_search(&wrap_row) {
            Ok(idx) => idx,
            Err(insert_pos) => insert_pos.saturating_sub(1),
        }
    }

    /// Set fold candidates (from tree-sitter/LSP), full replacement.
    pub(super) fn set_candidates(&mut self, mut candidates: Vec<FoldRange>) {
        // Sort and deduplicate by start_line
        candidates.sort_by_key(|r| r.start_line);
        candidates.dedup_by_key(|r| r.start_line);
        self.candidates = candidates;
        self.fold_as_candidates();
    }

    /// Merge new candidates extracted from an edited region into existing candidates.
    ///
    /// Replaces candidates within [edit_start_line, edit_end_line] with `new_candidates`,
    /// keeping candidates outside the edit range intact. A new candidate
    /// takes the place of one that begins on its line, as the extraction
    /// finds the folds around the edit too, which a fold kept through the
    /// edit grew or shrank with it, and a fold takes the lines of the
    /// candidate on its first line, or goes with it.
    pub(super) fn merge_candidates_for_edit(
        &mut self,
        edit_start_line: usize,
        edit_end_line: usize,
        mut new_candidates: Vec<FoldRange>,
    ) {
        self.candidates
            .retain(|c| c.start_line < edit_start_line || c.start_line > edit_end_line);
        // the new first, so that the sort keeps them first on their line
        // and the dedup keeps them
        new_candidates.append(&mut self.candidates);
        new_candidates.sort_by_key(|r| r.start_line);
        new_candidates.dedup_by_key(|r| r.start_line);
        self.candidates = new_candidates;
        self.fold_as_candidates();
    }

    /// Each fold covers what the candidate on its first line covers, and
    /// one with no candidate there goes: a fold an edit kept found again
    /// shorter or longer, or no longer found.
    fn fold_as_candidates(&mut self) {
        let candidates = &self.candidates;
        let mut changed = false;
        self.folded.retain_mut(|fold| {
            match candidates.binary_search_by_key(&fold.start_line, |c| c.start_line) {
                Ok(i) => {
                    changed |= fold.end_line != candidates[i].end_line;
                    fold.end_line = candidates[i].end_line;
                    true
                }
                Err(_) => {
                    changed = true;
                    false
                }
            }
        });
        if changed {
            self.needs_rebuild = true;
        }
    }

    /// Set a fold at the given start_line (must be in candidates)
    pub(super) fn set_folded(&mut self, start_line: usize, folded: bool) {
        if folded {
            // Find the candidate range for this start_line
            if let Some(candidate) = self.candidates.iter().find(|c| c.start_line == start_line) {
                // Add to folded if not already present
                if !self.folded.iter().any(|f| f.start_line == start_line) {
                    self.folded.push(*candidate);
                    self.folded.sort_by_key(|r| r.start_line);
                    self.needs_rebuild = true;
                }
            }
        } else {
            // Remove from folded
            self.folded.retain(|f| f.start_line != start_line);
            self.needs_rebuild = true;
        }
    }

    /// Toggle fold at the given start_line
    pub(super) fn toggle_fold(&mut self, start_line: usize) {
        let is_folded = self.is_folded_at(start_line);
        self.set_folded(start_line, !is_folded);
    }

    /// Check if a line is currently folded
    pub(super) fn is_folded_at(&self, start_line: usize) -> bool {
        self.folded.iter().any(|f| f.start_line == start_line)
    }

    /// Check if a line is a fold candidate
    pub(super) fn is_fold_candidate(&self, start_line: usize) -> bool {
        self.candidates.iter().any(|c| c.start_line == start_line)
    }

    /// Get all fold candidates
    #[inline]
    pub(super) fn fold_candidates(&self) -> &[FoldRange] {
        &self.candidates
    }

    /// Get all currently folded ranges
    #[inline]
    pub(super) fn folded_ranges(&self) -> &[FoldRange] {
        &self.folded
    }

    /// Clear all folds
    #[inline]
    pub(super) fn clear_folds(&mut self) {
        self.folded.clear();
    }

    /// Adjust folds and candidates after edits of the text, `edits`, each
    /// the lines it reached in the text before them all, from the first to
    /// the one before the second (none where the two are the same: whole
    /// lines put in before the first), and how many lines it put in or
    /// took, in order (one may end on the line the next begins on).
    ///
    /// A fold keeps its first and last line in view and hides those
    /// between. One stays, and moves by the lines the edits before it put
    /// in or took, where every edit that reaches it lies within the lines
    /// it hides, which then grow or shrink by that edit's lines, or keeps
    /// to its first line or its last and puts in or takes no line: a
    /// rename, a replace or a line taken in a fold leaves it shut, as VS
    /// Code and IntelliJ leave it; it was taken at any edit that reached
    /// its lines, and opened (elundqvist/kvist#404). An edit across its
    /// edges, or one that breaks or joins its first line or its last,
    /// takes it. What it covers is found again where the edit was
    /// (`merge_candidates_for_edit`). Each fold is looked up among the
    /// edits, so a replace of thousands of matches in a file of thousands
    /// of folds is not their product.
    pub(super) fn adjust_folds_for_edits(&mut self, edits: &[(usize, usize, isize)]) {
        if edits.is_empty() {
            return;
        }
        let mut grown = Vec::with_capacity(edits.len() + 1);
        grown.push(0isize);
        for (_, _, delta) in edits {
            grown.push(grown[grown.len() - 1] + delta);
        }
        let adjust = |ranges: &mut Vec<FoldRange>| {
            ranges.retain_mut(|fold| {
                let (first, last) = (fold.start_line, fold.end_line);
                // the edits before it reach no further than the line
                // before its first
                let k = edits.partition_point(|&(_, past, _)| past <= first);
                let mut inside = 0isize;
                for &(start, past, delta) in edits[k..].iter().take_while(|&&(start, _, _)| start <= last) {
                    let hidden = start > first && past <= last;
                    let on_its_own = delta == 0 && past == start + 1 && (start == first || start == last);
                    if hidden {
                        inside += delta;
                    } else if !on_its_own {
                        return false;
                    }
                }
                let before = grown[k];
                fold.start_line = (first as isize + before).max(0) as usize;
                fold.end_line = (last as isize + before + inside).max(0) as usize;
                fold.end_line > fold.start_line
            });
        };
        adjust(&mut self.folded);
        adjust(&mut self.candidates);
        self.needs_rebuild = true;
    }

    /// Rebuild the fold mapping after wrap_map or fold state changes
    ///
    /// This is the core algorithm that projects wrap rows to display rows.
    pub(super) fn rebuild(&mut self, wrap_map: &WrapMap) {
        let wrap_row_count = wrap_map.wrap_row_count();

        // Performance optimization: skip rebuild if nothing changed
        if !self.needs_rebuild && wrap_row_count == self.cached_wrap_row_count {
            return;
        }

        self.cached_wrap_row_count = wrap_row_count;

        self.visible_wrap_rows.clear();
        self.wrap_row_to_display_row = vec![None; wrap_row_count];

        if self.folded.is_empty() {
            // Fast path: no folds, all wrap rows are visible
            self.visible_wrap_rows = (0..wrap_row_count).collect();
            for (display_row, &wrap_row) in self.visible_wrap_rows.iter().enumerate() {
                self.wrap_row_to_display_row[wrap_row] = Some(display_row);
            }
            self.needs_rebuild = false;
            return;
        }

        // Build set of hidden wrap_row ranges from folded buffer lines
        let mut hidden_ranges = Vec::new();
        for fold in &self.folded {
            // Hide wrap rows from (start_line + 1) to (end_line - 1) (inclusive)
            // Both the first line and last line of the fold remain visible
            let hide_start_line = fold.start_line + 1;
            let hide_end_line = fold.end_line.saturating_sub(1);

            if hide_start_line > hide_end_line {
                continue; // No middle lines to hide (0 or 1 lines between start and end)
            }

            // Get wrap_row ranges for the hidden buffer lines
            let start_wrap_row = wrap_map.buffer_line_to_first_wrap_row(hide_start_line);
            let end_wrap_row = if hide_end_line + 1 < wrap_map.buffer_line_count() {
                wrap_map.buffer_line_to_first_wrap_row(hide_end_line + 1)
            } else {
                wrap_row_count
            };

            if start_wrap_row < end_wrap_row {
                hidden_ranges.push(start_wrap_row..end_wrap_row);
            }
        }

        // Merge overlapping hidden ranges
        hidden_ranges.sort_by_key(|r| r.start);
        let mut merged_hidden = Vec::new();
        for range in hidden_ranges {
            if let Some(last) = merged_hidden.last_mut() {
                if range.start <= *last {
                    // Overlapping or adjacent, merge
                    *last = (*last).max(range.end);
                } else {
                    merged_hidden.push(range.start);
                    merged_hidden.push(range.end);
                }
            } else {
                merged_hidden.push(range.start);
                merged_hidden.push(range.end);
            }
        }

        // Scan all wrap rows and filter out hidden ones
        let mut display_row = 0;
        let mut hidden_iter = merged_hidden.chunks_exact(2);
        let mut current_hidden = hidden_iter.next();

        for wrap_row in 0..wrap_row_count {
            // Check if wrap_row is in current hidden range
            let is_hidden = if let Some(&[start, end]) = current_hidden {
                if wrap_row >= end {
                    current_hidden = hidden_iter.next();
                    if let Some(&[new_start, new_end]) = current_hidden {
                        wrap_row >= new_start && wrap_row < new_end
                    } else {
                        false
                    }
                } else {
                    wrap_row >= start && wrap_row < end
                }
            } else {
                false
            };

            if !is_hidden {
                self.visible_wrap_rows.push(wrap_row);
                self.wrap_row_to_display_row[wrap_row] = Some(display_row);
                display_row += 1;
            }
        }

        self.needs_rebuild = false;
    }
}
