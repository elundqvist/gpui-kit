use std::{ops::Range, rc::Rc};

use gpui::{Bounds, Half, Pixels, ShapedLine, TextAlign, px};

use super::{WrappingIndent, display_map::LineLayout};

#[derive(Clone, Default)]
pub(crate) struct WhitespaceIndicators {
    pub(crate) space: ShapedLine,
    pub(crate) tab: ShapedLine,
}

#[derive(Clone)]
pub(super) struct LastLayout {
    pub(super) visible_range: Range<usize>,
    pub(super) visible_buffer_lines: Vec<usize>,
    pub(super) visible_line_byte_offsets: Vec<usize>,
    pub(super) visible_top: Pixels,
    pub(super) visible_range_offset: Range<usize>,
    pub(super) lines: Rc<Vec<LineLayout>>,
    pub(super) line_height: Pixels,
    pub(super) wrap_width: Option<Pixels>,
    pub(super) wrapping_indent: WrappingIndent,
    pub(super) line_number_width: Pixels,
    pub(super) cursor_bounds: Option<Bounds<Pixels>>,
    pub(super) text_align: TextAlign,
    pub(super) content_width: Pixels,
}

impl LastLayout {
    /// Whether `other` puts everything where this does: the same lines in
    /// sight, each as long and wrapped and as wide, the caret in the same
    /// place. What is drawn may differ (a colour, a glyph of the same
    /// width) where nothing that reads the layout after a paint would.
    pub(super) fn same_place(&self, other: &LastLayout) -> bool {
        self.visible_range == other.visible_range
            && self.visible_buffer_lines == other.visible_buffer_lines
            && self.visible_line_byte_offsets == other.visible_line_byte_offsets
            && self.visible_top == other.visible_top
            && self.visible_range_offset == other.visible_range_offset
            && self.line_height == other.line_height
            && self.wrap_width == other.wrap_width
            && self.wrapping_indent == other.wrapping_indent
            && self.line_number_width == other.line_number_width
            && self.cursor_bounds == other.cursor_bounds
            && self.text_align == other.text_align
            && self.content_width == other.content_width
            && self.lines.len() == other.lines.len()
            && self
                .lines
                .iter()
                .zip(other.lines.iter())
                .all(|(a, b)| a.same_place(b))
    }

    pub(crate) fn line(&self, row: usize) -> Option<&LineLayout> {
        let pos = self.visible_buffer_lines.binary_search(&row).ok()?;
        self.lines.get(pos)
    }

    pub(super) fn alignment_offset(&self, line_width: Pixels) -> Pixels {
        match self.text_align {
            TextAlign::Left => px(0.),
            TextAlign::Center => (self.content_width - line_width).half().max(px(0.)),
            TextAlign::Right => (self.content_width - line_width).max(px(0.)),
        }
    }
}
