// Changed in elundqvist/gpui-kit from gpui-pre 0.3.3 as published to crates.io: FORK.md at the
// crate's root lists the changes.
use crate::{FontId, GlyphId, Pixels, PlatformTextSystem, Point, SharedString, Size, point, px};
use collections::FxHashMap;
use parking_lot::{Mutex, RwLock, RwLockUpgradableReadGuard};
use smallvec::SmallVec;
use std::{
    borrow::Borrow,
    hash::{Hash, Hasher},
    ops::Range,
    sync::Arc,
};

use super::LineWrapper;

/// A laid out and styled line of text
#[derive(Default, Debug)]
pub struct LineLayout {
    /// The font size for this line
    pub font_size: Pixels,
    /// The width of the line
    pub width: Pixels,
    /// The ascent of the line
    pub ascent: Pixels,
    /// The descent of the line
    pub descent: Pixels,
    /// The shaped runs that make up this line
    pub runs: Vec<ShapedRun>,
    /// The length of the line in utf-8 bytes
    pub len: usize,
}

/// A run of text that has been shaped .
#[derive(Debug, Clone)]
pub struct ShapedRun {
    /// The font id for this run
    pub font_id: FontId,
    /// The glyphs that make up this run
    pub glyphs: Vec<ShapedGlyph>,
}

/// A single glyph, ready to paint.
#[derive(Clone, Debug)]
pub struct ShapedGlyph {
    /// The ID for this glyph, as determined by the text system.
    pub id: GlyphId,

    /// The position of this glyph in its containing line.
    pub position: Point<Pixels>,

    /// The index of this glyph in the original text.
    pub index: usize,

    /// Whether this glyph is an emoji
    pub is_emoji: bool,
}

impl LineLayout {
    /// The index for the character at the given x coordinate
    pub fn index_for_x(&self, x: Pixels) -> Option<usize> {
        if x >= self.width {
            None
        } else {
            for run in self.runs.iter().rev() {
                for glyph in run.glyphs.iter().rev() {
                    if glyph.position.x <= x {
                        return Some(glyph.index);
                    }
                }
            }
            Some(0)
        }
    }

    /// closest_index_for_x returns the character boundary closest to the given x coordinate
    /// (e.g. to handle aligning up/down arrow keys)
    ///
    /// The boundaries are where a cluster starts, and the line's end: the glyphs of one
    /// cluster share its index, and a caret between them would stand inside it. A grapheme
    /// the font draws as two glyphs, a family emoji it has no glyph of its own for, is one
    /// cluster, and a click on its right half, on the left of its second glyph, went to its
    /// start, which that glyph's start was taken for; the last cluster ends at the line's
    /// width, so a click on its left half goes before it, where it went to the line's end.
    pub fn closest_index_for_x(&self, x: Pixels) -> usize {
        // the cluster before x: where it starts, and its index
        let mut prev_index = 0;
        let mut prev_x = px(0.);
        let nearer = |prev_index: usize, prev_x: Pixels, index: usize, at: Pixels| {
            if at - x < x - prev_x {
                index
            } else {
                prev_index
            }
        };

        for run in self.runs.iter() {
            for glyph in run.glyphs.iter() {
                // a glyph of the cluster before it
                if glyph.index == prev_index {
                    continue;
                }
                if glyph.position.x >= x {
                    return nearer(prev_index, prev_x, glyph.index, glyph.position.x);
                }
                prev_index = glyph.index;
                prev_x = glyph.position.x;
            }
        }

        if x <= self.width {
            return nearer(prev_index, prev_x, self.len, self.width);
        }

        self.len
    }

    /// The x position of the character at the given index
    pub fn x_for_index(&self, index: usize) -> Pixels {
        for run in &self.runs {
            for glyph in &run.glyphs {
                if glyph.index >= index {
                    return glyph.position.x;
                }
            }
        }
        self.width
    }

    /// The corresponding Font at the given index
    pub fn font_id_for_index(&self, index: usize) -> Option<FontId> {
        for run in &self.runs {
            for glyph in &run.glyphs {
                if glyph.index >= index {
                    return Some(run.font_id);
                }
            }
        }
        None
    }

    /// Split this layout at a byte index, returning `(prefix, suffix)`.
    ///
    /// - `prefix` contains glyphs for bytes `[0, byte_index)` with original positions.
    ///   Its width equals the x-advance up to the split point.
    /// - `suffix` contains glyphs for bytes `[byte_index, len)` with positions
    ///   shifted left so the first glyph starts at x=0, and byte indices rebased to 0.
    /// - `font_size`, `ascent`, and `descent` are copied to both halves.
    pub fn split_at(&self, byte_index: usize) -> (LineLayout, LineLayout) {
        let x_offset = self.x_for_index(byte_index);

        // Partition glyph runs. A single run may contribute glyphs to both halves.
        let mut left_runs = Vec::new();
        let mut right_runs = Vec::new();

        for run in &self.runs {
            let split_pos = run.glyphs.partition_point(|g| g.index < byte_index);

            if split_pos > 0 {
                left_runs.push(ShapedRun {
                    font_id: run.font_id,
                    glyphs: run.glyphs[..split_pos].to_vec(),
                });
            }

            if split_pos < run.glyphs.len() {
                let right_glyphs = run.glyphs[split_pos..]
                    .iter()
                    .map(|g| ShapedGlyph {
                        id: g.id,
                        position: point(g.position.x - x_offset, g.position.y),
                        index: g.index - byte_index,
                        is_emoji: g.is_emoji,
                    })
                    .collect();
                right_runs.push(ShapedRun {
                    font_id: run.font_id,
                    glyphs: right_glyphs,
                });
            }
        }

        let left = LineLayout {
            font_size: self.font_size,
            width: x_offset,
            ascent: self.ascent,
            descent: self.descent,
            runs: left_runs,
            len: byte_index,
        };

        let right = LineLayout {
            font_size: self.font_size,
            width: self.width - x_offset,
            ascent: self.ascent,
            descent: self.descent,
            runs: right_runs,
            len: self.len - byte_index,
        };

        (left, right)
    }

    // Changed in elundqvist/gpui-kit (elundqvist/kvist#485): crate-visible, so that
    // `LineWrapper::truncate_wrapped_line` cuts text on the rows it is drawn in
    pub(crate) fn compute_wrap_boundaries(
        &self,
        text: &str,
        wrap_width: Pixels,
        max_lines: Option<usize>,
    ) -> SmallVec<[WrapBoundary; 1]> {
        let glyphs = self.runs.iter().enumerate().flat_map(move |(run_ix, run)| {
            run.glyphs.iter().enumerate().map(move |(glyph_ix, glyph)| {
                let character = text[glyph.index..].chars().next().unwrap();
                (
                    WrapBoundary { run_ix, glyph_ix },
                    character,
                    glyph.position.x,
                )
            })
        });
        wrap_points(glyphs, self.width, wrap_width, max_lines)
    }

    /// Where the rows of this line wrapped to `wrap_width` begin in its
    /// text, after the first, found in the text's order: each glyph as
    /// wide as it is drawn, its clusters in the order of their text
    /// (elundqvist/kvist#648). `compute_wrap_boundaries` breaks the line
    /// as it is drawn, so a line with a right-to-left part put the end of
    /// a word, drawn on the left, on the first row and its start on the
    /// second.
    pub(crate) fn text_order_row_starts(
        &self,
        text: &str,
        wrap_width: Pixels,
        max_lines: Option<usize>,
    ) -> SmallVec<[usize; 1]> {
        let (clusters, width) = self.text_order_clusters(text);
        wrap_points(clusters.into_iter(), width, wrap_width, max_lines)
    }

    /// This line's clusters in the order of its text, each its index, its
    /// character and where it begins were the line drawn in that order,
    /// and the line's width. A glyph is as wide as the room from where it
    /// is drawn to the glyph drawn next to its right: a line of
    /// right-to-left letters alone is laid out right to left, its glyphs
    /// in the order of their text and their x falling, and the room to the
    /// glyph after it in the line was the next one's width, the last one's
    /// the whole line's.
    pub(crate) fn text_order_clusters(&self, text: &str) -> (Vec<(usize, char, Pixels)>, Pixels) {
        let mut drawn: Vec<(usize, Pixels)> = self
            .runs
            .iter()
            .flat_map(|run| run.glyphs.iter())
            .map(|glyph| (glyph.index, glyph.position.x))
            .collect();
        drawn.sort_by_key(|(_, x)| *x);
        let mut widths: Vec<(usize, Pixels)> = drawn
            .iter()
            .enumerate()
            .map(|(k, (index, x))| {
                let right = drawn.get(k + 1).map_or(self.width, |(_, x)| *x);
                (*index, right - *x)
            })
            .collect();
        widths.sort_by_key(|(index, _)| *index);
        let mut x = px(0.);
        let mut clusters: Vec<(usize, char, Pixels)> = Vec::with_capacity(widths.len());
        for (index, width) in widths {
            match clusters.last() {
                Some((last, _, _)) if *last == index => {}
                _ => clusters.push((index, text[index..].chars().next().unwrap(), x)),
            }
            x += width;
        }
        (clusters, x)
    }
}

/// Where a line of `items`, each a place to break before, its character
/// and where it begins, `end` wide, breaks into rows `wrap_width` wide.
fn wrap_points<B: Copy + Ord>(
    items: impl Iterator<Item = (B, char, Pixels)>,
    end: Pixels,
    wrap_width: Pixels,
    max_lines: Option<usize>,
) -> SmallVec<[B; 1]> {
    let mut boundaries = SmallVec::new();
    let mut first_non_whitespace_ix = None;
    let mut last_candidate_ix = None;
    let mut last_candidate_x = px(0.);
    let mut glyphs = items.peekable();
    // a row breaks after its first glyph at the earliest
    let Some(&(mut last_boundary, _, _)) = glyphs.peek() else {
        return boundaries;
    };
    let mut last_boundary_x = px(0.);
    let mut prev_ch = '\0';

    while let Some((boundary, ch, x)) = glyphs.next() {
        if ch == '\n' {
            continue;
        }

        // Here is very similar to `LineWrapper::wrap_line` to determine text wrapping,
        // but there are some differences, so we have to duplicate the code here.
        if LineWrapper::is_word_char(ch) {
            // Changed in elundqvist/gpui-kit (elundqvist/kvist#497): a word may
            // begin a row after a `，` as after a space, and (elundqvist/kvist#522)
            // after a slash, as in `LineWrapper::wrap_line`
            if (prev_ch == ' '
                || LineWrapper::is_closing_mark(prev_ch)
                || LineWrapper::breaks_after_slash(prev_ch, ch))
                && ch != ' '
                && first_non_whitespace_ix.is_some()
            {
                last_candidate_ix = Some(boundary);
                last_candidate_x = x;
            }
        } else {
            // Changed in elundqvist/gpui-kit (elundqvist/kvist#497): no row
            // starts with a `，` or ends with a `「`, nor (elundqvist/kvist#522)
            // with a slash but after a space, as in `LineWrapper::wrap_line`
            if ch != ' '
                && !LineWrapper::holds_slash(prev_ch, ch)
                && first_non_whitespace_ix.is_some()
                && LineWrapper::may_break_between(prev_ch, ch)
            {
                last_candidate_ix = Some(boundary);
                last_candidate_x = x;
            }
        }

        if ch != ' ' && first_non_whitespace_ix.is_none() {
            first_non_whitespace_ix = Some(boundary);
        }

        let next_x = glyphs.peek().map_or(end, |(_, _, x)| *x);
        let width = next_x - last_boundary_x;

        if width > wrap_width && boundary > last_boundary {
            // When used line_clamp, we should limit the number of lines.
            if let Some(max_lines) = max_lines
                && boundaries.len() >= max_lines.saturating_sub(1)
            {
                break;
            }

            if let Some(last_candidate_ix) = last_candidate_ix.take() {
                last_boundary = last_candidate_ix;
                last_boundary_x = last_candidate_x;
            } else {
                last_boundary = boundary;
                last_boundary_x = x;
            }
            boundaries.push(last_boundary);
        }
        prev_ch = ch;
    }

    boundaries
}

/// Whether `text` has a right-to-left letter in it: its rows are then
/// laid out each on its own (`LineLayoutCache::rows_laid_out_apart`), as
/// what is drawn of a row depends on where it breaks.
pub(crate) fn has_right_to_left(text: &str) -> bool {
    use unicode_bidi::{BidiClass, bidi_class};
    text.chars()
        .any(|c| !c.is_ascii() && matches!(bidi_class(c), BidiClass::R | BidiClass::AL))
}

/// The part of `runs` over `range` of their text.
fn runs_within(runs: &[FontRun], range: Range<usize>) -> SmallVec<[FontRun; 1]> {
    let mut within = SmallVec::new();
    let mut start = 0;
    for run in runs {
        let end = start + run.len;
        let (from, to) = (start.max(range.start), end.min(range.end));
        if from < to {
            within.push(FontRun {
                len: to - from,
                font_id: run.font_id,
            });
        }
        start = end;
    }
    within
}

/// A line of text that has been wrapped to fit a given width
#[derive(Default, Debug)]
pub struct WrappedLineLayout {
    /// The line layout, pre-wrapping.
    pub unwrapped_layout: Arc<LineLayout>,

    /// The boundaries at which the line was wrapped
    pub wrap_boundaries: SmallVec<[WrapBoundary; 1]>,

    /// The width of the line, if it was wrapped
    pub wrap_width: Option<Pixels>,

    /// Where each row after the first begins in the text, for a line whose
    /// rows were each laid out on its own, one with right-to-left text in
    /// it (elundqvist/kvist#648): a row's first glyph drawn need not be its
    /// first in the text. Empty where each row's first glyph drawn is.
    pub(crate) row_starts: SmallVec<[usize; 1]>,
}

/// A boundary at which a line was wrapped
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WrapBoundary {
    /// The index in the run just before the line was wrapped
    pub run_ix: usize,
    /// The index of the glyph just before the line was wrapped
    pub glyph_ix: usize,
}

impl WrappedLineLayout {
    /// The length of the underlying text, in utf8 bytes.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.unwrapped_layout.len
    }

    /// The width of this line, in pixels, whether or not it was wrapped.
    pub fn width(&self) -> Pixels {
        self.wrap_width
            .unwrap_or(Pixels::MAX)
            .min(self.unwrapped_layout.width)
    }

    /// The size of the whole wrapped text, for the given line_height.
    /// can span multiple lines if there are multiple wrap boundaries.
    pub fn size(&self, line_height: Pixels) -> Size<Pixels> {
        Size {
            width: self.width(),
            height: line_height * (self.wrap_boundaries.len() + 1),
        }
    }

    /// The ascent of a line in this layout
    pub fn ascent(&self) -> Pixels {
        self.unwrapped_layout.ascent
    }

    /// The descent of a line in this layout
    pub fn descent(&self) -> Pixels {
        self.unwrapped_layout.descent
    }

    /// The wrap boundaries in this layout
    pub fn wrap_boundaries(&self) -> &[WrapBoundary] {
        &self.wrap_boundaries
    }

    /// The font size of this layout
    pub fn font_size(&self) -> Pixels {
        self.unwrapped_layout.font_size
    }

    /// The runs in this layout, sans wrapping
    pub fn runs(&self) -> &[ShapedRun] {
        &self.unwrapped_layout.runs
    }

    /// Where the row after the `ix`th wrap boundary begins in the text.
    pub fn row_start(&self, ix: usize) -> usize {
        match self.row_starts.get(ix) {
            Some(start) => *start,
            None => {
                let boundary = self.wrap_boundaries[ix];
                self.unwrapped_layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix].index
            }
        }
    }

    /// The index corresponding to a given position in this layout for the given line height.
    ///
    /// See also [`Self::closest_index_for_position`].
    pub fn index_for_position(
        &self,
        position: Point<Pixels>,
        line_height: Pixels,
    ) -> Result<usize, usize> {
        self._index_for_position(position, line_height, false)
    }

    /// The closest index to a given position in this layout for the given line height.
    ///
    /// Closest means the character boundary closest to the given position.
    ///
    /// See also [`LineLayout::closest_index_for_x`].
    pub fn closest_index_for_position(
        &self,
        position: Point<Pixels>,
        line_height: Pixels,
    ) -> Result<usize, usize> {
        self._index_for_position(position, line_height, true)
    }

    fn _index_for_position(
        &self,
        mut position: Point<Pixels>,
        line_height: Pixels,
        closest: bool,
    ) -> Result<usize, usize> {
        let wrapped_line_ix = (position.y / line_height) as usize;

        let wrapped_line_start_index;
        let wrapped_line_start_x;
        if wrapped_line_ix > 0 {
            let Some(line_start_boundary) = self.wrap_boundaries.get(wrapped_line_ix - 1) else {
                return Err(0);
            };
            let run = &self.unwrapped_layout.runs[line_start_boundary.run_ix];
            let glyph = &run.glyphs[line_start_boundary.glyph_ix];
            wrapped_line_start_index = self.row_start(wrapped_line_ix - 1);
            wrapped_line_start_x = glyph.position.x;
        } else {
            wrapped_line_start_index = 0;
            wrapped_line_start_x = Pixels::ZERO;
        };

        let wrapped_line_end_index;
        let wrapped_line_end_x;
        if wrapped_line_ix < self.wrap_boundaries.len() {
            let next_wrap_boundary_ix = wrapped_line_ix;
            let next_wrap_boundary = self.wrap_boundaries[next_wrap_boundary_ix];
            let run = &self.unwrapped_layout.runs[next_wrap_boundary.run_ix];
            let glyph = &run.glyphs[next_wrap_boundary.glyph_ix];
            wrapped_line_end_index = self.row_start(next_wrap_boundary_ix);
            wrapped_line_end_x = glyph.position.x;
        } else {
            wrapped_line_end_index = self.unwrapped_layout.len;
            wrapped_line_end_x = self.unwrapped_layout.width;
        };

        let mut position_in_unwrapped_line = position;
        position_in_unwrapped_line.x += wrapped_line_start_x;
        if position_in_unwrapped_line.x < wrapped_line_start_x {
            Err(wrapped_line_start_index)
        } else if position_in_unwrapped_line.x >= wrapped_line_end_x {
            Err(wrapped_line_end_index)
        } else {
            if closest {
                Ok(self
                    .unwrapped_layout
                    .closest_index_for_x(position_in_unwrapped_line.x))
            } else {
                Ok(self
                    .unwrapped_layout
                    .index_for_x(position_in_unwrapped_line.x)
                    .unwrap())
            }
        }
    }

    /// Returns the pixel position for the given byte index.
    pub fn position_for_index(&self, index: usize, line_height: Pixels) -> Option<Point<Pixels>> {
        let mut line_start_ix = 0;
        let mut line_end_indices = (0..self.wrap_boundaries.len())
            .map(|ix| self.row_start(ix))
            .chain([self.len()])
            .enumerate();
        for (ix, line_end_ix) in line_end_indices {
            let line_y = ix as f32 * line_height;
            if index < line_start_ix {
                break;
            } else if index > line_end_ix {
                line_start_ix = line_end_ix;
                continue;
            } else {
                let line_start_x = self.unwrapped_layout.x_for_index(line_start_ix);
                let x = self.unwrapped_layout.x_for_index(index) - line_start_x;
                return Some(point(x, line_y));
            }
        }

        None
    }
}

pub(crate) struct LineLayoutCache {
    previous_frame: Mutex<FrameCache>,
    current_frame: RwLock<FrameCache>,
    platform_text_system: Arc<dyn PlatformTextSystem>,
}

#[derive(Default)]
struct FrameCache {
    lines: FxHashMap<Arc<CacheKey>, Arc<LineLayout>>,
    wrapped_lines: FxHashMap<Arc<CacheKey>, Arc<WrappedLineLayout>>,
    used_lines: Vec<Arc<CacheKey>>,
    used_wrapped_lines: Vec<Arc<CacheKey>>,

    // Content-addressable caches keyed by caller-provided text hash + layout params.
    // These allow cache hits without materializing a contiguous `SharedString`.
    //
    // IMPORTANT: To support allocation-free lookups, we store these maps using a key type
    // (`HashedCacheKeyRef`) that can be computed without building a contiguous `&str`/`SharedString`.
    // On miss, we allocate once and store under an owned `HashedCacheKey`.
    lines_by_hash: FxHashMap<Arc<HashedCacheKey>, Arc<LineLayout>>,
    wrapped_lines_by_hash: FxHashMap<Arc<HashedCacheKey>, Arc<WrappedLineLayout>>,
    used_lines_by_hash: Vec<Arc<HashedCacheKey>>,
    used_wrapped_lines_by_hash: Vec<Arc<HashedCacheKey>>,
}

#[derive(Clone, Default)]
pub(crate) struct LineLayoutIndex {
    lines_index: usize,
    wrapped_lines_index: usize,
    lines_by_hash_index: usize,
    wrapped_lines_by_hash_index: usize,
}

impl LineLayoutIndex {
    /// This index, inside a range of a frame's layouts that began at
    /// `from`, where that range lands in a frame it is taken into at `to`
    /// (`LineLayoutCache::reuse_layouts` takes each layout of it, in order).
    pub(crate) fn moved(&self, from: &Self, to: &Self) -> Self {
        LineLayoutIndex {
            lines_index: self.lines_index - from.lines_index + to.lines_index,
            wrapped_lines_index: self.wrapped_lines_index - from.wrapped_lines_index + to.wrapped_lines_index,
            lines_by_hash_index: self.lines_by_hash_index - from.lines_by_hash_index + to.lines_by_hash_index,
            wrapped_lines_by_hash_index: self.wrapped_lines_by_hash_index - from.wrapped_lines_by_hash_index
                + to.wrapped_lines_by_hash_index,
        }
    }
}

impl LineLayoutCache {
    pub fn new(platform_text_system: Arc<dyn PlatformTextSystem>) -> Self {
        Self {
            previous_frame: Mutex::default(),
            current_frame: RwLock::default(),
            platform_text_system,
        }
    }

    pub fn layout_index(&self) -> LineLayoutIndex {
        let frame = self.current_frame.read();
        LineLayoutIndex {
            lines_index: frame.used_lines.len(),
            wrapped_lines_index: frame.used_wrapped_lines.len(),
            lines_by_hash_index: frame.used_lines_by_hash.len(),
            wrapped_lines_by_hash_index: frame.used_wrapped_lines_by_hash.len(),
        }
    }

    pub fn reuse_layouts(&self, range: Range<LineLayoutIndex>) {
        let mut previous_frame = &mut *self.previous_frame.lock();
        let mut current_frame = &mut *self.current_frame.write();

        for key in &previous_frame.used_lines[range.start.lines_index..range.end.lines_index] {
            if let Some((key, line)) = previous_frame.lines.remove_entry(key) {
                current_frame.lines.insert(key, line);
            }
            current_frame.used_lines.push(key.clone());
        }

        for key in &previous_frame.used_wrapped_lines
            [range.start.wrapped_lines_index..range.end.wrapped_lines_index]
        {
            if let Some((key, line)) = previous_frame.wrapped_lines.remove_entry(key) {
                current_frame.wrapped_lines.insert(key, line);
            }
            current_frame.used_wrapped_lines.push(key.clone());
        }

        for key in &previous_frame.used_lines_by_hash
            [range.start.lines_by_hash_index..range.end.lines_by_hash_index]
        {
            if let Some((key, line)) = previous_frame.lines_by_hash.remove_entry(key) {
                current_frame.lines_by_hash.insert(key, line);
            }
            current_frame.used_lines_by_hash.push(key.clone());
        }

        for key in &previous_frame.used_wrapped_lines_by_hash
            [range.start.wrapped_lines_by_hash_index..range.end.wrapped_lines_by_hash_index]
        {
            if let Some((key, line)) = previous_frame.wrapped_lines_by_hash.remove_entry(key) {
                current_frame.wrapped_lines_by_hash.insert(key, line);
            }
            current_frame.used_wrapped_lines_by_hash.push(key.clone());
        }
    }

    pub fn truncate_layouts(&self, index: LineLayoutIndex) {
        let mut current_frame = &mut *self.current_frame.write();
        current_frame.used_lines.truncate(index.lines_index);
        current_frame
            .used_wrapped_lines
            .truncate(index.wrapped_lines_index);
        current_frame
            .used_lines_by_hash
            .truncate(index.lines_by_hash_index);
        current_frame
            .used_wrapped_lines_by_hash
            .truncate(index.wrapped_lines_by_hash_index);
    }

    pub fn finish_frame(&self) {
        let mut prev_frame = self.previous_frame.lock();
        let mut curr_frame = self.current_frame.write();
        std::mem::swap(&mut *prev_frame, &mut *curr_frame);
        curr_frame.lines.clear();
        curr_frame.wrapped_lines.clear();
        curr_frame.used_lines.clear();
        curr_frame.used_wrapped_lines.clear();

        curr_frame.lines_by_hash.clear();
        curr_frame.wrapped_lines_by_hash.clear();
        curr_frame.used_lines_by_hash.clear();
        curr_frame.used_wrapped_lines_by_hash.clear();
    }

    pub fn layout_wrapped_line<Text>(
        &self,
        text: Text,
        font_size: Pixels,
        runs: &[FontRun],
        wrap_width: Option<Pixels>,
        max_lines: Option<usize>,
    ) -> Arc<WrappedLineLayout>
    where
        Text: AsRef<str>,
        SharedString: From<Text>,
    {
        let key = &CacheKeyRef {
            text: text.as_ref(),
            font_size,
            runs,
            wrap_width,
            force_width: None,
        } as &dyn AsCacheKeyRef;

        let current_frame = self.current_frame.upgradable_read();
        if let Some(layout) = current_frame.wrapped_lines.get(key) {
            return layout.clone();
        }

        let previous_frame_entry = self.previous_frame.lock().wrapped_lines.remove_entry(key);
        if let Some((key, layout)) = previous_frame_entry {
            let mut current_frame = RwLockUpgradableReadGuard::upgrade(current_frame);
            current_frame
                .wrapped_lines
                .insert(key.clone(), layout.clone());
            current_frame.used_wrapped_lines.push(key);
            layout
        } else {
            drop(current_frame);
            let text = SharedString::from(text);
            let unwrapped_layout = self.layout_line::<&SharedString>(&text, font_size, runs, None);
            let layout = Arc::new(match wrap_width {
                Some(wrap_width)
                    if unwrapped_layout.width > wrap_width && has_right_to_left(&text) =>
                {
                    let row_starts =
                        unwrapped_layout.text_order_row_starts(&text, wrap_width, max_lines);
                    self.rows_laid_out_apart(&text, font_size, runs, &row_starts, wrap_width)
                }
                _ => WrappedLineLayout {
                    wrap_boundaries: match wrap_width {
                        Some(wrap_width) => unwrapped_layout.compute_wrap_boundaries(
                            text.as_ref(),
                            wrap_width,
                            max_lines,
                        ),
                        None => SmallVec::new(),
                    },
                    unwrapped_layout,
                    wrap_width,
                    row_starts: SmallVec::new(),
                },
            });
            let key = Arc::new(CacheKey {
                text,
                font_size,
                runs: SmallVec::from(runs),
                wrap_width,
                force_width: None,
            });

            let mut current_frame = self.current_frame.write();
            current_frame
                .wrapped_lines
                .insert(key.clone(), layout.clone());
            current_frame.used_wrapped_lines.push(key);

            layout
        }
    }

    /// `text` wrapped at `row_starts`, each row laid out as a line of its
    /// own, as UAX #9 has a paragraph's lines reordered each on its own
    /// once it is broken in the text's order (elundqvist/kvist#648). The
    /// rows' glyphs make one line, each row's after the one before it and
    /// in the order they are drawn, so the line is drawn and read as any
    /// wrapped at its glyphs.
    fn rows_laid_out_apart(
        &self,
        text: &str,
        font_size: Pixels,
        runs: &[FontRun],
        row_starts: &[usize],
        wrap_width: Pixels,
    ) -> WrappedLineLayout {
        let ends = row_starts.iter().copied().chain([text.len()]);
        let mut line = LineLayout {
            font_size,
            ..Default::default()
        };
        let mut wrap_boundaries: SmallVec<[WrapBoundary; 1]> = SmallVec::new();
        let mut kept_starts = SmallVec::new();
        let mut start = 0;
        for end in ends {
            let row = self.platform_text_system.layout_line(
                &text[start..end],
                font_size,
                &runs_within(runs, start..end),
            );
            // the row's glyphs in the order they are drawn, left to right, as
            // a row is drawn from its first glyph at its left edge: a row of
            // right-to-left letters alone is laid out right to left, its
            // glyphs in the order of their text, the first at its right edge
            let mut glyphs: Vec<(FontId, ShapedGlyph)> = row
                .runs
                .into_iter()
                .flat_map(|run| {
                    let font_id = run.font_id;
                    run.glyphs.into_iter().map(move |glyph| (font_id, glyph))
                })
                .collect();
            if glyphs.is_empty() {
                // a row that drew no glyph is the end of the row before it
                start = end;
                continue;
            }
            glyphs.sort_by_key(|(_, glyph)| glyph.position.x);
            let first_run = line.runs.len();
            if first_run > 0 {
                wrap_boundaries.push(WrapBoundary {
                    run_ix: first_run,
                    glyph_ix: 0,
                });
                kept_starts.push(start);
            }
            let x = line.width;
            for (font_id, glyph) in glyphs {
                let glyph = ShapedGlyph {
                    position: point(glyph.position.x + x, glyph.position.y),
                    index: glyph.index + start,
                    ..glyph
                };
                let in_row = line.runs.len() > first_run;
                match line.runs.last_mut() {
                    Some(run) if in_row && run.font_id == font_id => run.glyphs.push(glyph),
                    _ => line.runs.push(ShapedRun {
                        font_id,
                        glyphs: vec![glyph],
                    }),
                }
            }
            line.width += row.width;
            line.ascent = line.ascent.max(row.ascent);
            line.descent = line.descent.max(row.descent);
            start = end;
        }
        line.len = text.len();
        WrappedLineLayout {
            unwrapped_layout: Arc::new(line),
            wrap_boundaries,
            wrap_width: Some(wrap_width),
            row_starts: kept_starts,
        }
    }

    pub fn layout_line<Text>(
        &self,
        text: Text,
        font_size: Pixels,
        runs: &[FontRun],
        force_width: Option<Pixels>,
    ) -> Arc<LineLayout>
    where
        Text: AsRef<str>,
        SharedString: From<Text>,
    {
        let key = &CacheKeyRef {
            text: text.as_ref(),
            font_size,
            runs,
            wrap_width: None,
            force_width,
        } as &dyn AsCacheKeyRef;

        let current_frame = self.current_frame.upgradable_read();
        if let Some(layout) = current_frame.lines.get(key) {
            return layout.clone();
        }

        let mut current_frame = RwLockUpgradableReadGuard::upgrade(current_frame);
        if let Some((key, layout)) = self.previous_frame.lock().lines.remove_entry(key) {
            current_frame.lines.insert(key.clone(), layout.clone());
            current_frame.used_lines.push(key);
            layout
        } else {
            let text = SharedString::from(text);
            let mut layout = self
                .platform_text_system
                .layout_line(&text, font_size, runs);

            if let Some(force_width) = force_width {
                apply_force_width_to_layout(&mut layout, force_width);
            }

            let key = Arc::new(CacheKey {
                text,
                font_size,
                runs: SmallVec::from(runs),
                wrap_width: None,
                force_width,
            });
            let layout = Arc::new(layout);
            current_frame.lines.insert(key.clone(), layout.clone());
            current_frame.used_lines.push(key);
            layout
        }
    }

    /// Try to retrieve a previously-shaped line layout using a caller-provided content hash.
    ///
    /// This is a *non-allocating* cache probe: it does not materialize any text. If the layout
    /// is not already cached in either the current frame or previous frame, returns `None`.
    ///
    /// Contract (caller enforced):
    /// - Same `text_hash` implies identical text content (collision risk accepted by caller).
    /// - `text_len` should be the UTF-8 byte length of the text (helps reduce accidental collisions).
    pub fn try_layout_line_by_hash(
        &self,
        text_hash: u64,
        text_len: usize,
        font_size: Pixels,
        runs: &[FontRun],
        force_width: Option<Pixels>,
    ) -> Option<Arc<LineLayout>> {
        let key_ref = HashedCacheKeyRef {
            text_hash,
            text_len,
            font_size,
            runs,
            wrap_width: None,
            force_width,
        };

        let current_frame = self.current_frame.read();
        if let Some((_, layout)) = current_frame.lines_by_hash.iter().find(|(key, _)| {
            HashedCacheKeyRef {
                text_hash: key.text_hash,
                text_len: key.text_len,
                font_size: key.font_size,
                runs: key.runs.as_slice(),
                wrap_width: key.wrap_width,
                force_width: key.force_width,
            } == key_ref
        }) {
            return Some(layout.clone());
        }

        let previous_frame = self.previous_frame.lock();
        if let Some((_, layout)) = previous_frame.lines_by_hash.iter().find(|(key, _)| {
            HashedCacheKeyRef {
                text_hash: key.text_hash,
                text_len: key.text_len,
                font_size: key.font_size,
                runs: key.runs.as_slice(),
                wrap_width: key.wrap_width,
                force_width: key.force_width,
            } == key_ref
        }) {
            return Some(layout.clone());
        }

        None
    }

    /// Layout a line of text using a caller-provided content hash as the cache key.
    ///
    /// This enables cache hits without materializing a contiguous `SharedString` for `text`.
    /// If the cache misses, `materialize_text` is invoked to produce the `SharedString` for shaping.
    ///
    /// Contract (caller enforced):
    /// - Same `text_hash` implies identical text content (collision risk accepted by caller).
    /// - `text_len` should be the UTF-8 byte length of the text (helps reduce accidental collisions).
    pub fn layout_line_by_hash(
        &self,
        text_hash: u64,
        text_len: usize,
        font_size: Pixels,
        runs: &[FontRun],
        force_width: Option<Pixels>,
        materialize_text: impl FnOnce() -> SharedString,
    ) -> Arc<LineLayout> {
        let key_ref = HashedCacheKeyRef {
            text_hash,
            text_len,
            font_size,
            runs,
            wrap_width: None,
            force_width,
        };

        // Fast path: already cached (no allocation).
        let current_frame = self.current_frame.upgradable_read();
        if let Some((_, layout)) = current_frame.lines_by_hash.iter().find(|(key, _)| {
            HashedCacheKeyRef {
                text_hash: key.text_hash,
                text_len: key.text_len,
                font_size: key.font_size,
                runs: key.runs.as_slice(),
                wrap_width: key.wrap_width,
                force_width: key.force_width,
            } == key_ref
        }) {
            return layout.clone();
        }

        let mut current_frame = RwLockUpgradableReadGuard::upgrade(current_frame);

        // Try to reuse from previous frame without allocating; do a linear scan to find a matching key.
        // (We avoid `drain()` here because it would eagerly move all entries.)
        let mut previous_frame = self.previous_frame.lock();
        if let Some(existing_key) = previous_frame
            .used_lines_by_hash
            .iter()
            .find(|key| {
                HashedCacheKeyRef {
                    text_hash: key.text_hash,
                    text_len: key.text_len,
                    font_size: key.font_size,
                    runs: key.runs.as_slice(),
                    wrap_width: key.wrap_width,
                    force_width: key.force_width,
                } == key_ref
            })
            .cloned()
        {
            if let Some((key, layout)) = previous_frame.lines_by_hash.remove_entry(&existing_key) {
                current_frame
                    .lines_by_hash
                    .insert(key.clone(), layout.clone());
                current_frame.used_lines_by_hash.push(key);
                return layout;
            }
        }

        let text = materialize_text();
        let mut layout = self
            .platform_text_system
            .layout_line(&text, font_size, runs);

        if let Some(force_width) = force_width {
            apply_force_width_to_layout(&mut layout, force_width);
        }

        let key = Arc::new(HashedCacheKey {
            text_hash,
            text_len,
            font_size,
            runs: SmallVec::from(runs),
            wrap_width: None,
            force_width,
        });
        let layout = Arc::new(layout);
        current_frame
            .lines_by_hash
            .insert(key.clone(), layout.clone());
        current_frame.used_lines_by_hash.push(key);
        layout
    }
}

// Combining marks (e.g. Thai vowel signs, Arabic diacritics) are shaped by
// HarfBuzz at the same x position as their base character. The force-width
// loop must not advance the cell counter for these zero-advance glyphs,
// otherwise they get displaced into the next cell. We detect them by checking
// whether shaped x has advanced by at least half a cell beyond the last base.
fn apply_force_width_to_layout(layout: &mut LineLayout, force_width: Pixels) {
    let mut glyph_pos: usize = 0;
    // NEG_INFINITY ensures the first glyph is always classified as a base.
    let mut last_base_shaped_x = px(f32::NEG_INFINITY);
    let mut last_base_actual_x = px(0.);

    for run in layout.runs.iter_mut() {
        for glyph in run.glyphs.iter_mut() {
            let shaped_x = glyph.position.x;

            if shaped_x > last_base_shaped_x + force_width * 0.5 {
                let forced_x = glyph_pos * force_width;
                if (shaped_x - forced_x).abs() > px(1.) {
                    glyph.position.x = forced_x;
                }
                last_base_shaped_x = shaped_x;
                last_base_actual_x = glyph.position.x;
                glyph_pos += 1;
            } else {
                glyph.position.x = last_base_actual_x + (shaped_x - last_base_shaped_x);
            }
        }
    }
}

/// A run of text with a single font.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[expect(missing_docs)]
pub struct FontRun {
    pub len: usize,
    pub font_id: FontId,
}

trait AsCacheKeyRef {
    fn as_cache_key_ref(&self) -> CacheKeyRef<'_>;
}

#[derive(Clone, Debug, Eq)]
struct CacheKey {
    text: SharedString,
    font_size: Pixels,
    runs: SmallVec<[FontRun; 1]>,
    wrap_width: Option<Pixels>,
    force_width: Option<Pixels>,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
struct CacheKeyRef<'a> {
    text: &'a str,
    font_size: Pixels,
    runs: &'a [FontRun],
    wrap_width: Option<Pixels>,
    force_width: Option<Pixels>,
}

#[derive(Clone, Debug)]
struct HashedCacheKey {
    text_hash: u64,
    text_len: usize,
    font_size: Pixels,
    runs: SmallVec<[FontRun; 1]>,
    wrap_width: Option<Pixels>,
    force_width: Option<Pixels>,
}

#[derive(Copy, Clone)]
struct HashedCacheKeyRef<'a> {
    text_hash: u64,
    text_len: usize,
    font_size: Pixels,
    runs: &'a [FontRun],
    wrap_width: Option<Pixels>,
    force_width: Option<Pixels>,
}

impl PartialEq for dyn AsCacheKeyRef + '_ {
    fn eq(&self, other: &dyn AsCacheKeyRef) -> bool {
        self.as_cache_key_ref() == other.as_cache_key_ref()
    }
}

impl PartialEq for HashedCacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.text_hash == other.text_hash
            && self.text_len == other.text_len
            && self.font_size == other.font_size
            && self.runs.as_slice() == other.runs.as_slice()
            && self.wrap_width == other.wrap_width
            && self.force_width == other.force_width
    }
}

impl Eq for HashedCacheKey {}

impl Hash for HashedCacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.text_hash.hash(state);
        self.text_len.hash(state);
        self.font_size.hash(state);
        self.runs.as_slice().hash(state);
        self.wrap_width.hash(state);
        self.force_width.hash(state);
    }
}

impl PartialEq for HashedCacheKeyRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.text_hash == other.text_hash
            && self.text_len == other.text_len
            && self.font_size == other.font_size
            && self.runs == other.runs
            && self.wrap_width == other.wrap_width
            && self.force_width == other.force_width
    }
}

impl Eq for HashedCacheKeyRef<'_> {}

impl Hash for HashedCacheKeyRef<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.text_hash.hash(state);
        self.text_len.hash(state);
        self.font_size.hash(state);
        self.runs.hash(state);
        self.wrap_width.hash(state);
        self.force_width.hash(state);
    }
}

impl Eq for dyn AsCacheKeyRef + '_ {}

impl Hash for dyn AsCacheKeyRef + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_cache_key_ref().hash(state)
    }
}

impl AsCacheKeyRef for CacheKey {
    fn as_cache_key_ref(&self) -> CacheKeyRef<'_> {
        CacheKeyRef {
            text: &self.text,
            font_size: self.font_size,
            runs: self.runs.as_slice(),
            wrap_width: self.wrap_width,
            force_width: self.force_width,
        }
    }
}

impl PartialEq for CacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.as_cache_key_ref().eq(&other.as_cache_key_ref())
    }
}

impl Hash for CacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_cache_key_ref().hash(state);
    }
}

impl<'a> Borrow<dyn AsCacheKeyRef + 'a> for Arc<CacheKey> {
    fn borrow(&self) -> &(dyn AsCacheKeyRef + 'a) {
        self.as_ref() as &dyn AsCacheKeyRef
    }
}

impl AsCacheKeyRef for CacheKeyRef<'_> {
    fn as_cache_key_ref(&self) -> CacheKeyRef<'_> {
        *self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GlyphId;

    fn glyph_at(x: f32, index: usize) -> ShapedGlyph {
        ShapedGlyph {
            id: GlyphId(0),
            position: point(px(x), px(0.)),
            index,
            is_emoji: false,
        }
    }

    fn make_layout(glyphs: Vec<ShapedGlyph>) -> LineLayout {
        LineLayout {
            font_size: px(16.),
            width: px(100.),
            ascent: px(12.),
            descent: px(4.),
            runs: vec![ShapedRun {
                font_id: FontId(0),
                glyphs,
            }],
            len: 0,
        }
    }

    fn glyph_x_positions(layout: &LineLayout) -> Vec<f32> {
        layout.runs[0]
            .glyphs
            .iter()
            .map(|g| f32::from(g.position.x))
            .collect()
    }

    /// The rows a shaped line is drawn in break after a slash in a word,
    /// as `LineWrapper::wrap_line` does, where they broke before it and left
    /// `Merge feature` alone on a row (elundqvist/kvist#522).
    #[test]
    fn test_drawn_rows_break_after_a_slash() {
        let text = "Merge feature/topic-name";
        let mut layout = make_layout((0..text.len()).map(|i| glyph_at(i as f32 * 10., i)).collect());
        layout.width = px(text.len() as f32 * 10.);
        layout.len = text.len();
        let rows: Vec<usize> = layout
            .compute_wrap_boundaries(text, px(100.), None)
            .iter()
            .map(|b| b.glyph_ix)
            .collect();
        assert_eq!(rows, [6, 14]);
    }

    #[test]
    fn test_force_width_latin_unchanged() {
        let cell_width = px(8.);
        let mut layout = make_layout(vec![glyph_at(0., 0), glyph_at(8., 1), glyph_at(16., 2)]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0., 8., 16.]);
    }

    #[test]
    fn test_force_width_combining_marks_not_advanced() {
        let cell_width = px(8.);
        // Simulates Thai "กี" — base consonant at x=0, combining vowel also at x=0
        let mut layout = make_layout(vec![
            glyph_at(0., 0), // ก (base)
            glyph_at(0., 3), // ี (combining mark, same x)
        ]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0., 0.]);
    }

    #[test]
    fn test_force_width_base_after_combining_mark() {
        let cell_width = px(8.);
        let mut layout = make_layout(vec![glyph_at(0., 0), glyph_at(0., 3), glyph_at(8., 6)]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0., 0., 8.]);
    }

    #[test]
    fn test_force_width_multiple_combining_marks() {
        let cell_width = px(8.);
        // Simulates "ก้" — base + vowel + tone mark (two combining marks stacked)
        let mut layout = make_layout(vec![
            glyph_at(0., 0), // ก (base)
            glyph_at(0., 3), // vowel (combining)
            glyph_at(0., 6), // tone mark (combining)
            glyph_at(8., 9), // next base
        ]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0., 0., 0., 8.]);
    }

    #[test]
    fn test_force_width_corrects_drifted_base_positions() {
        let cell_width = px(8.);
        // Font metrics don't perfectly match cell grid — glyphs drift >1px from cell boundary
        let mut layout = make_layout(vec![
            glyph_at(0.5, 0),  // within 1px tolerance, kept as-is
            glyph_at(10.2, 1), // >1px off from 8.0, corrected
            glyph_at(19.8, 2), // >1px off from 16.0, corrected
        ]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0.5, 8., 16.]);
    }

    #[test]
    fn test_force_width_combining_mark_after_within_tolerance_base() {
        let cell_width = px(8.);
        // Base glyph is within 1px of grid so it keeps its shaped position.
        // The combining mark must align to the base's actual position, not the grid slot.
        let mut layout = make_layout(vec![glyph_at(0.5, 0), glyph_at(0.5, 3)]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0.5, 0.5]);
    }

    /// `x👧‍👦a` as cosmic-text shapes it with Noto Color Emoji, which has no glyph for a
    /// girl and a boy: the x, the two glyphs of the family, both of the cluster at byte 1
    /// where the joined emoji starts, and the a after it at byte 12, the line 13 long. Each
    /// is 16 px but the x, which is 8.
    fn family() -> LineLayout {
        LineLayout {
            len: "x👧‍👦a".len(),
            width: px(56.),
            ..make_layout(vec![
                glyph_at(0., 0),
                glyph_at(8., 1),
                glyph_at(24., 1),
                glyph_at(40., 12),
            ])
        }
    }

    /// A click on a grapheme of two glyphs goes to its nearer edge: its start for its
    /// left half, the end for its right half, the left of its second glyph among it, where
    /// that went to its start (elundqvist/kvist#474).
    #[test]
    fn a_click_on_a_cluster_of_two_glyphs_goes_to_its_nearer_edge() {
        let layout = family();
        // the family runs from 8 to 40, its middle at 24
        for (x, at) in [
            (9., 1),
            (20., 1),
            (23.9, 1),
            (24.1, 12),
            (26., 12),
            (30., 12),
            (39., 12),
        ] {
            assert_eq!(layout.closest_index_for_x(px(x)), at, "a click at {x}");
        }
        // the x and the a, either side of it
        for (x, at) in [
            (-3., 0),
            (0., 0),
            (3., 0),
            (5., 1),
            (41., 12),
            (47.9, 12),
            (48.1, 13),
            (56., 13),
            (90., 13),
        ] {
            assert_eq!(layout.closest_index_for_x(px(x)), at, "a click at {x}");
        }
    }

    /// The last cluster ends at the line's width: a click on its left half goes before
    /// it, as on any other, where any click past its start went to the line's end.
    #[test]
    fn a_click_on_the_left_half_of_the_last_cluster_goes_before_it() {
        let ab = LineLayout {
            len: 2,
            width: px(16.),
            ..make_layout(vec![glyph_at(0., 0), glyph_at(8., 1)])
        };
        assert_eq!(ab.closest_index_for_x(px(9.)), 1);
        assert_eq!(ab.closest_index_for_x(px(11.9)), 1);
        assert_eq!(ab.closest_index_for_x(px(12.1)), 2);
        assert_eq!(ab.closest_index_for_x(px(30.)), 2);
        // a line of one cluster, as before
        let a = LineLayout {
            len: 1,
            width: px(8.),
            ..make_layout(vec![glyph_at(0., 0)])
        };
        assert_eq!(a.closest_index_for_x(px(3.)), 0);
        assert_eq!(a.closest_index_for_x(px(5.)), 1);
    }

    /// A text system that lays out each character 10 px wide, as a bidi
    /// shaper would: a line with a Latin letter left to right, each run of
    /// Hebrew in it right to left, a line of Hebrew alone right to left,
    /// its glyphs in the order of their text, as cosmic-text has it.
    struct BidiTextSystem;

    impl crate::PlatformTextSystem for BidiTextSystem {
        fn add_fonts(&self, fonts: Vec<std::borrow::Cow<'static, [u8]>>) -> anyhow::Result<()> {
            crate::NoopTextSystem.add_fonts(fonts)
        }
        fn all_font_names(&self) -> Vec<String> {
            Vec::new()
        }
        fn font_id(&self, descriptor: &crate::Font) -> anyhow::Result<FontId> {
            crate::NoopTextSystem.font_id(descriptor)
        }
        fn font_metrics(&self, font_id: FontId) -> crate::FontMetrics {
            crate::NoopTextSystem.font_metrics(font_id)
        }
        fn typographic_bounds(
            &self,
            font_id: FontId,
            glyph_id: GlyphId,
        ) -> anyhow::Result<crate::Bounds<f32>> {
            crate::NoopTextSystem.typographic_bounds(font_id, glyph_id)
        }
        fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> anyhow::Result<Size<f32>> {
            crate::NoopTextSystem.advance(font_id, glyph_id)
        }
        fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
            crate::NoopTextSystem.glyph_for_char(font_id, ch)
        }
        fn glyph_raster_bounds(
            &self,
            params: &crate::RenderGlyphParams,
        ) -> anyhow::Result<crate::Bounds<crate::DevicePixels>> {
            crate::NoopTextSystem.glyph_raster_bounds(params)
        }
        fn rasterize_glyph(
            &self,
            params: &crate::RenderGlyphParams,
            raster_bounds: crate::Bounds<crate::DevicePixels>,
        ) -> anyhow::Result<(Size<crate::DevicePixels>, Vec<u8>)> {
            crate::NoopTextSystem.rasterize_glyph(params, raster_bounds)
        }
        fn layout_line(&self, text: &str, font_size: Pixels, _runs: &[FontRun]) -> LineLayout {
            let hebrew = |c: char| ('\u{05D0}'..='\u{05EA}').contains(&c);
            let mut order: Vec<(usize, char)> = text.char_indices().collect();
            if order.iter().any(|(_, c)| c.is_ascii_alphabetic()) {
                // each run of Hebrew, its spaces between its words in it
                let mut i = 0;
                while i < order.len() {
                    if !hebrew(order[i].1) {
                        i += 1;
                        continue;
                    }
                    let mut end = i;
                    for j in i..order.len() {
                        if hebrew(order[j].1) {
                            end = j + 1;
                        } else if order[j].1 != ' ' {
                            break;
                        }
                    }
                    order[i..end].reverse();
                    i = end;
                }
            }
            // a line of Hebrew alone is laid out right to left, as
            // cosmic-text lays it out: its glyphs in the order of their
            // text, the first at its right edge
            let right_to_left = !text.chars().any(|c| c.is_ascii_alphabetic());
            let n = order.len();
            let glyphs = order
                .iter()
                .enumerate()
                .map(|(at, (index, c))| ShapedGlyph {
                    id: GlyphId(*c as u32),
                    position: point(
                        px(if right_to_left { n - 1 - at } else { at } as f32 * 10.),
                        px(0.),
                    ),
                    index: *index,
                    is_emoji: false,
                })
                .collect();
            LineLayout {
                font_size,
                width: px(order.len() as f32 * 10.),
                ascent: px(10.),
                descent: px(-3.),
                runs: vec![ShapedRun {
                    font_id: FontId(0),
                    glyphs,
                }],
                len: text.len(),
            }
        }
        fn recommended_rendering_mode(
            &self,
            font_id: FontId,
            font_size: Pixels,
        ) -> crate::TextRenderingMode {
            crate::NoopTextSystem.recommended_rendering_mode(font_id, font_size)
        }
    }

    /// A wrapped line with a right-to-left part breaks in the order of its
    /// text, each row laid out on its own: the Hebrew's first word, drawn
    /// rightmost, is on the first row, where breaking the line as it is
    /// drawn put its last word there and its first on the next
    /// (elundqvist/kvist#648).
    #[test]
    fn a_wrapped_line_with_hebrew_in_it_breaks_in_its_texts_order() {
        let cache = LineLayoutCache::new(Arc::new(BidiTextSystem));
        let text =
            "ab \u{05D0}\u{05D1}\u{05D2} \u{05D3}\u{05D4}\u{05D5} \u{05D6}\u{05D7}\u{05D8} cd";
        let runs = [FontRun {
            len: text.len(),
            font_id: FontId(0),
        }];
        let wrapped = cache.layout_wrapped_line(text, px(16.), &runs, Some(px(80.)), None);

        let at = |c: char| text.find(c).unwrap();
        let rows: Vec<usize> = (0..wrapped.wrap_boundaries().len())
            .map(|ix| wrapped.row_start(ix))
            .collect();
        assert_eq!(rows, [at('\u{05D3}'), at('c')]);
        // each row drawn of its own glyphs, the first word of the Hebrew
        // rightmost on the first row
        let row_of = |c: char| {
            let p = wrapped.position_for_index(at(c), px(20.)).unwrap();
            (p.y / px(20.)) as usize
        };
        assert_eq!(row_of('\u{05D0}'), 0);
        assert_eq!(row_of('\u{05D6}'), 1);
        assert_eq!(row_of('d'), 2);
        let first_row: Vec<usize> = wrapped.runs()[..wrapped.wrap_boundaries()[0].run_ix]
            .iter()
            .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.index))
            .collect();
        assert_eq!(
            first_row,
            [
                0,
                1,
                2,
                at('\u{05D2}'),
                at('\u{05D1}'),
                at('\u{05D0}'),
                at('\u{05D2}') + 2
            ]
        );

        // a line of Latin alone is broken where it is drawn, as before
        let latin = cache.layout_wrapped_line(
            "ab cd ef gh",
            px(16.),
            &[FontRun {
                len: 11,
                font_id: FontId(0),
            }],
            Some(px(50.)),
            None,
        );
        assert!(latin.row_starts.is_empty());
        assert_eq!(latin.row_start(0), 3);
    }

    /// A line of Latin and Hebrew clamped to two rows with an ellipsis is
    /// cut on the rows it is drawn in, broken in the order of its text: the
    /// cut broke it as it is drawn, into two rows where its text's order
    /// has three, so it was left whole and drawn in two rows, the second
    /// wider than the width (elundqvist/kvist#648).
    #[test]
    fn a_line_with_hebrew_in_it_clamped_with_an_ellipsis_is_cut_on_its_rows() {
        use crate::{LineWrapper, TextRun, TextSystem, TruncateFrom, font};

        let platform = Arc::new(BidiTextSystem);
        let text_system = Arc::new(TextSystem::new(platform.clone()));
        let font_id = text_system.resolve_font(&font("Dummy"));
        let mut wrapper = LineWrapper::new(font_id, px(16.), text_system);
        // 160 px: two rows as it is drawn, three in its text's order
        let text = "ab \u{05D0} \u{05D1} \u{05D2} \u{05D3}\u{05D4}\u{05D5}\u{05D6} cd";
        let wrap_width = px(80.);
        let runs = [TextRun {
            len: text.len(),
            font: font("Dummy"),
            ..Default::default()
        }];
        let (truncated, _) = wrapper.truncate_wrapped_line(
            text.into(),
            wrap_width,
            2,
            "\u{2026}",
            &runs,
            TruncateFrom::End,
        );
        assert!(truncated.ends_with('\u{2026}'), "'{truncated}' is cut");

        // and drawn in two rows, each within the width
        let cache = LineLayoutCache::new(platform);
        let font_runs = [FontRun {
            len: truncated.len(),
            font_id,
        }];
        let drawn =
            cache.layout_wrapped_line(&truncated, px(16.), &font_runs, Some(wrap_width), Some(2));
        assert_eq!(
            drawn.wrap_boundaries().len(),
            1,
            "'{truncated}' in two rows"
        );
        let boundary = drawn.wrap_boundaries()[0];
        let second_x = drawn.runs()[boundary.run_ix].glyphs[boundary.glyph_ix]
            .position
            .x;
        assert!(second_x <= wrap_width, "its first row {second_x:?} wide");
        let second_width = drawn.unwrapped_layout.width - second_x;
        assert!(
            second_width <= wrap_width,
            "its second row {second_width:?} wide"
        );
    }

    /// A wrapped line of Hebrew alone, laid out right to left, its glyphs
    /// in the order of their text, breaks between its words, each its
    /// width, and each row is drawn from its first glyph at its left edge
    /// rightwards within the width: the room to the next glyph in the line
    /// was taken for a glyph's width, the last one's the whole line's, and
    /// a row's first glyph in its text, drawn at its right edge, was taken
    /// for its left edge, so each row after the first hung to the left of
    /// the text (elundqvist/kvist#648).
    #[test]
    fn a_wrapped_line_of_hebrew_alone_is_drawn_row_by_row_within_its_width() {
        let cache = LineLayoutCache::new(Arc::new(BidiTextSystem));
        let word = "\u{05D0}\u{05D1}\u{05D2}";
        let text = [word; 6].join(" ");
        let runs = [FontRun {
            len: text.len(),
            font_id: FontId(0),
        }];
        let wrap_width = px(80.);
        let wrapped = cache.layout_wrapped_line(&text, px(16.), &runs, Some(wrap_width), None);

        // two words a row, each row's start a word's
        let word_at = |k: usize| k * (word.len() + 1);
        let rows: Vec<usize> = (0..wrapped.wrap_boundaries().len())
            .map(|ix| wrapped.row_start(ix))
            .collect();
        assert_eq!(rows, [word_at(2), word_at(4)]);

        let glyphs: Vec<&ShapedGlyph> = wrapped
            .runs()
            .iter()
            .flat_map(|run| run.glyphs.iter())
            .collect();
        let row_ends = wrapped
            .wrap_boundaries()
            .iter()
            .map(|boundary| {
                wrapped.runs()[..boundary.run_ix]
                    .iter()
                    .map(|run| run.glyphs.len())
                    .sum::<usize>()
                    + boundary.glyph_ix
            })
            .chain([glyphs.len()]);
        let mut row_start = 0;
        for (row, row_end) in row_ends.enumerate() {
            let left = glyphs[row_start].position.x;
            let mut x = left;
            for glyph in &glyphs[row_start..row_end] {
                assert!(glyph.position.x >= x, "row {row} drawn right to left");
                x = glyph.position.x;
                assert!(x - left < wrap_width, "row {row} wider than its width");
            }
            // the row's first word in its text drawn rightmost
            let first = glyphs[row_end - 1].index;
            assert!(
                (word_at(row * 2)..word_at(row * 2) + word.len()).contains(&first),
                "row {row} ends drawn with {first}"
            );
            row_start = row_end;
        }
    }
}
