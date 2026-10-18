use crate::input::EditorMode;
use std::ops::Range;

use gpui::{App, Context, HighlightStyle, WeakEntity};
use ropey::Rope;
use sum_tree::Bias;

use super::{InputBaseState, RopeExt as _};

/// A presentation style applied to a UTF-8 byte range in an input.
///
/// This is the GPUI [`HighlightStyle`] counterpart of Monaco's
/// [`IModelDeltaDecoration`](https://microsoft.github.io/monaco-editor/typedoc/interfaces/editor_editor_api.editor.IModelDeltaDecoration.html).
#[derive(Clone, Debug, PartialEq)]
pub struct TextDecoration {
    pub range: Range<usize>,
    pub style: HighlightStyle,
}

impl TextDecoration {
    /// Create a text decoration from a UTF-8 byte range and a GPUI style.
    pub fn new(range: Range<usize>, style: HighlightStyle) -> Self {
        Self { range, style }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct TextDecorationCollectionId(usize);

/// An independently managed collection of [`TextDecoration`]s.
///
/// This is the GPUI Component counterpart of Monaco's
/// [`IEditorDecorationsCollection`](https://microsoft.github.io/monaco-editor/typedoc/interfaces/editor_editor_api.editor.IEditorDecorationsCollection.html).
#[derive(Clone, Debug)]
pub struct TextDecorationCollection {
    state: WeakEntity<InputBaseState<EditorMode>>,
    id: TextDecorationCollectionId,
}

impl TextDecorationCollection {
    /// Replace all decorations in this collection.
    ///
    /// This corresponds to Monaco's
    /// [`IEditorDecorationsCollection.set`](https://microsoft.github.io/monaco-editor/typedoc/interfaces/editor_editor_api.editor.IEditorDecorationsCollection.html#set).
    pub fn set(&self, decorations: Vec<TextDecoration>, cx: &mut App) {
        let _ = self.state.update(cx, |state, cx| {
            let decorations = normalize(&state.text, decorations);
            if state.extras.decorations.set(self.id, decorations) {
                cx.notify();
            }
        });
    }

    /// Add decorations to this collection.
    ///
    /// This corresponds to Monaco's
    /// [`IEditorDecorationsCollection.append`](https://microsoft.github.io/monaco-editor/typedoc/interfaces/editor_editor_api.editor.IEditorDecorationsCollection.html#append).
    pub fn append(&self, decorations: Vec<TextDecoration>, cx: &mut App) {
        let _ = self.state.update(cx, |state, cx| {
            let decorations = normalize(&state.text, decorations);
            if state.extras.decorations.append(self.id, decorations) {
                cx.notify();
            }
        });
    }

    /// Remove all decorations from this collection.
    ///
    /// This corresponds to Monaco's
    /// [`IEditorDecorationsCollection.clear`](https://microsoft.github.io/monaco-editor/typedoc/interfaces/editor_editor_api.editor.IEditorDecorationsCollection.html#clear).
    pub fn clear(&self, cx: &mut App) {
        self.set(Vec::new(), cx);
    }

    /// Return the UTF-8 byte ranges in this collection.
    ///
    /// This corresponds to Monaco's
    /// [`IEditorDecorationsCollection.getRanges`](https://microsoft.github.io/monaco-editor/typedoc/interfaces/editor_editor_api.editor.IEditorDecorationsCollection.html#getRanges).
    pub fn get_ranges(&self, cx: &App) -> Vec<Range<usize>> {
        self.state
            .read_with(cx, |state, _| {
                state
                    .extras
                    .decorations
                    .get(self.id)
                    .unwrap_or_default()
                    .iter()
                    .map(|decoration| decoration.range.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Default)]
pub(crate) struct DecorationCollections {
    entries: Vec<(TextDecorationCollectionId, Vec<TextDecoration>)>,
}

impl DecorationCollections {
    fn create(&mut self, decorations: Vec<TextDecoration>) -> TextDecorationCollectionId {
        let id = TextDecorationCollectionId(self.entries.len());
        self.entries.push((id, decorations));
        id
    }

    fn set(&mut self, id: TextDecorationCollectionId, decorations: Vec<TextDecoration>) -> bool {
        let Some((_, current)) = self
            .entries
            .iter_mut()
            .find(|(entry_id, _)| *entry_id == id)
        else {
            return false;
        };
        *current = decorations;
        true
    }

    fn append(&mut self, id: TextDecorationCollectionId, decorations: Vec<TextDecoration>) -> bool {
        let Some((_, current)) = self
            .entries
            .iter_mut()
            .find(|(entry_id, _)| *entry_id == id)
        else {
            return false;
        };
        current.extend(decorations);
        true
    }

    fn get(&self, id: TextDecorationCollectionId) -> Option<&[TextDecoration]> {
        self.entries
            .iter()
            .find(|(entry_id, _)| *entry_id == id)
            .map(|(_, decorations)| decorations.as_slice())
    }

    pub(super) fn adjust_for_edit(&mut self, edited_range: &Range<usize>, inserted_len: usize) {
        for (_, decorations) in &mut self.entries {
            decorations.retain_mut(|decoration| {
                decoration.range =
                    adjust_range_for_edit(&decoration.range, edited_range, inserted_len);
                !decoration.range.is_empty()
            });
        }
    }

    /// Move the decorations with edits made one after another, `steps`,
    /// each a range of the text as the ones before it left it and how
    /// many bytes went there, as an undo, a replace-all or an
    /// application's changes (`replace_steps`) make them: each where the
    /// steps made one by one leave it (`adjust_for_edit`). They were moved
    /// by the one edit from the first step to the last, so a breakpoint
    /// between two places of a rename went to the line of the first, and
    /// stayed there through the undo (elundqvist/kvist#404). Steps in
    /// order, as `apart` gives them, are looked up for each end, so
    /// thousands of matches replaced are not multiplied by the
    /// decorations; in another order each is made in turn.
    pub(super) fn adjust_for_steps(&mut self, steps: &[(Range<usize>, usize)]) {
        if self.entries.iter().all(|(_, decorations)| decorations.is_empty()) {
            return;
        }
        let edits = (steps.len() > 1).then(|| super::diagnostics::apart(steps)).flatten();
        let Some(edits) = edits else {
            for (range, len) in steps {
                self.adjust_for_edit(range, *len);
            }
            return;
        };
        let mut grown = vec![0isize; edits.len() + 1];
        for (k, (range, len)) in edits.iter().enumerate() {
            grown[k + 1] = grown[k] + *len as isize - range.len() as isize;
        }
        for (_, decorations) in &mut self.entries {
            decorations.retain_mut(|decoration| {
                decoration.range = moved_through(decoration.range.start, true, &edits, &grown)
                    ..moved_through(decoration.range.end, false, &edits, &grown);
                !decoration.range.is_empty()
            });
        }
    }

    pub(super) fn clear(&mut self) {
        for (_, decorations) in &mut self.entries {
            decorations.clear();
        }
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &[TextDecoration]> {
        self.entries
            .iter()
            .map(|(_, decorations)| decorations.as_slice())
    }
}

fn adjust_range_for_edit(
    range: &Range<usize>,
    edited_range: &Range<usize>,
    inserted_len: usize,
) -> Range<usize> {
    adjust_offset_for_edit(range.start, true, edited_range, inserted_len)
        ..adjust_offset_for_edit(range.end, false, edited_range, inserted_len)
}

/// Where a decoration's start (`start`) or end at `offset` is after
/// `edited_range` is replaced by `inserted_len` bytes: one at an edge stays
/// out of what is put in, so nothing grows by what is typed at its edges,
/// and one inside what is replaced goes to the edge of what went there, so
/// a decoration over the edit covers what is put in.
fn adjust_offset_for_edit(
    offset: usize,
    start: bool,
    edited_range: &Range<usize>,
    inserted_len: usize,
) -> usize {
    let removed_len = edited_range.end.saturating_sub(edited_range.start);
    let shift = |offset: usize| {
        if inserted_len >= removed_len {
            offset.saturating_add(inserted_len - removed_len)
        } else {
            offset.saturating_sub(removed_len - inserted_len)
        }
    };
    if edited_range.is_empty() {
        let before = if start {
            offset < edited_range.start
        } else {
            offset <= edited_range.start
        };
        return if before { offset } else { shift(offset) };
    }
    if offset <= edited_range.start {
        offset
    } else if offset >= edited_range.end {
        shift(offset)
    } else if start {
        edited_range.start
    } else {
        edited_range.start + inserted_len
    }
}

/// Where `offset`, a decoration's start (`start`) or end in the text
/// before `edits`, is once they are made one after another, as
/// `adjust_offset_for_edit` moves it through each: `edits` are apart and in
/// order in that text (`apart`), and `grown[k]` is what the first `k` grew
/// it by. Those that end before it move it by what they grew the text by;
/// of the rest, only the few at its place can move it otherwise.
fn moved_through(
    offset: usize,
    start: bool,
    edits: &[(Range<usize>, usize)],
    grown: &[isize],
) -> usize {
    let shift = |at: usize, by: isize| (at as isize + by).max(0) as usize;
    let k = edits.partition_point(|(range, _)| range.end < offset);
    let mut offset = shift(offset, grown[k]);
    for (j, (range, len)) in edits.iter().enumerate().skip(k) {
        // the edit where the ones before it left it
        let range = shift(range.start, grown[j])..shift(range.end, grown[j]);
        if range.start > offset {
            break;
        }
        offset = adjust_offset_for_edit(offset, start, &range, *len);
    }
    offset
}

fn normalize(text: &Rope, decorations: Vec<TextDecoration>) -> Vec<TextDecoration> {
    decorations
        .into_iter()
        .filter_map(|decoration| {
            let range = text.clip_offset(decoration.range.start, Bias::Left)
                ..text.clip_offset(decoration.range.end, Bias::Right);
            (!range.is_empty()).then_some(TextDecoration {
                range,
                style: decoration.style,
            })
        })
        .collect()
}

impl InputBaseState<EditorMode> {
    /// Create an independently managed collection of text decorations.
    ///
    /// This follows Monaco's
    /// [`createDecorationsCollection`](https://microsoft.github.io/monaco-editor/typedoc/interfaces/editor_editor_api.editor.ICodeEditor.html#createDecorationsCollection)
    /// ownership model. Ranges use UTF-8 byte offsets into [`Self::value`].
    ///
    /// Decoration ranges follow text edits and do not need to be set again
    /// after each change. Insertions at a range boundary do not expand the
    /// range, matching Monaco's
    /// [`NeverGrowsWhenTypingAtEdges`](https://microsoft.github.io/monaco-editor/typedoc/enums/editor_editor_api.editor.TrackedRangeStickiness.html#NeverGrowsWhenTypingAtEdges)
    /// behavior. Decorations are not rendered while the input is masked.
    /// Collections live until their [`InputBaseState`] is dropped.
    ///
    /// Collections are layered in insertion order; the first collection wins
    /// when overlapping decorations set the same [`HighlightStyle`] property.
    /// Callers should avoid conflicting overlaps within one collection.
    pub fn create_decorations_collection(
        &mut self,
        decorations: Vec<TextDecoration>,
        cx: &mut Context<Self>,
    ) -> TextDecorationCollection {
        let decorations = normalize(&self.text, decorations);
        let id = self.extras.decorations.create(decorations);
        cx.notify();
        TextDecorationCollection {
            state: cx.entity().downgrade(),
            id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collections_are_independent_and_ranges_are_clipped() {
        let text = Rope::from("héllo");
        let first_style = HighlightStyle {
            font_weight: Some(gpui::FontWeight::BOLD),
            ..Default::default()
        };
        let second_style = HighlightStyle {
            background_color: Some(gpui::red()),
            ..Default::default()
        };
        let mut collections = DecorationCollections::default();

        let first = collections.create(normalize(
            &text,
            vec![TextDecoration::new(2..4, first_style)],
        ));
        let second = collections.create(normalize(
            &text,
            vec![TextDecoration::new(5..100, second_style)],
        ));

        assert_ne!(first, second);
        assert_eq!(
            collections.get(first),
            Some(&[TextDecoration::new(1..4, first_style)][..])
        );
        assert_eq!(
            collections.get(second),
            Some(&[TextDecoration::new(5..6, second_style)][..])
        );

        assert!(collections.append(first, vec![TextDecoration::new(4..5, second_style)]));
        assert_eq!(
            collections.get(first),
            Some(
                &[
                    TextDecoration::new(1..4, first_style),
                    TextDecoration::new(4..5, second_style),
                ][..]
            )
        );

        assert!(collections.set(first, Vec::new()));
        assert_eq!(collections.get(first), Some(&[][..]));
        assert_eq!(
            collections.get(second),
            Some(&[TextDecoration::new(5..6, second_style)][..])
        );
    }

    #[test]
    fn decoration_ranges_follow_text_edits() {
        let style = HighlightStyle::default();
        let mut collections = DecorationCollections::default();
        let collection = collections.create(vec![TextDecoration::new(2..6, style)]);

        collections.adjust_for_edit(&(0..0), 2);
        assert_eq!(
            collections.get(collection),
            Some(&[TextDecoration::new(4..8, style)][..])
        );

        collections.adjust_for_edit(&(6..6), 2);
        assert_eq!(
            collections.get(collection),
            Some(&[TextDecoration::new(4..10, style)][..])
        );

        collections.adjust_for_edit(&(4..10), 3);
        assert_eq!(
            collections.get(collection),
            Some(&[TextDecoration::new(4..7, style)][..])
        );

        assert_eq!(adjust_range_for_edit(&(2..6), &(2..2), 2), 4..8);
        assert_eq!(adjust_range_for_edit(&(2..6), &(6..6), 2), 2..6);
        assert_eq!(adjust_range_for_edit(&(2..6), &(2..6), 3), 2..5);
    }

    /// Edits made one after another, the last first as a rename or a
    /// replace-all makes them and the first first as their undo does, move
    /// every decoration where each edit made alone in turn leaves it: a
    /// line's breakpoint between two of them stays on its line. Moved by
    /// the one edit from the first to the last, it went to the line of the
    /// first, and stayed there through the undo (elundqvist/kvist#404).
    #[test]
    fn decorations_follow_edits_made_one_after_another() {
        let style = HighlightStyle::default();
        let text = Rope::from((0..30).map(|i| format!("let v{i} = sum;\n")).collect::<String>());
        let line = |l: usize| text.line_start_offset(l)..text.line_start_offset(l + 1);
        // a decoration on every line, one over three lines, and some at
        // the edges of the edits below
        let mut ranges: Vec<Range<usize>> = (0..30).map(line).collect();
        ranges.push(line(9).start..line(11).end);
        let sum = |l: usize| {
            let start = text.line_start_offset(l) + text.slice(line(l)).to_string().find("sum").unwrap();
            start..start + 3
        };
        ranges.extend([sum(4), sum(20).start..sum(20).start, sum(20).end..sum(25).end]);
        ranges.retain(|range| !range.is_empty());
        // `sum` renamed on lines 4, 12, 20 and 25, the last first, and two
        // lines taken out between them, and one put in
        let steps: Vec<(Range<usize>, &str)> = vec![
            (sum(25), "total"),
            (sum(20), "total"),
            (line(16).start..line(17).end, ""),
            (sum(12), "s"),
            (line(8).start..line(8).start, "let w = 0;\n"),
            (sum(4), "total"),
        ];
        let made = |text: &Rope, steps: &[(Range<usize>, &str)]| {
            let mut after = text.clone();
            for (range, new) in steps {
                after.replace(range.clone(), new);
            }
            after
        };
        let one_by_one = |ranges: &[Range<usize>], steps: &[(Range<usize>, &str)]| {
            let mut each = DecorationCollections::default();
            let id = each.create(ranges.iter().map(|r| TextDecoration::new(r.clone(), style)).collect());
            for (range, new) in steps {
                each.adjust_for_edit(range, new.len());
            }
            each.get(id).unwrap().to_vec()
        };
        let together = |ranges: &[Range<usize>], steps: &[(Range<usize>, &str)]| {
            let mut all = DecorationCollections::default();
            let id = all.create(ranges.iter().map(|r| TextDecoration::new(r.clone(), style)).collect());
            let lens: Vec<(Range<usize>, usize)> = steps.iter().map(|(r, new)| (r.clone(), new.len())).collect();
            all.adjust_for_steps(&lens);
            all.get(id).unwrap().to_vec()
        };
        let after = made(&text, &steps);
        let moved = together(&ranges, &steps);
        assert_eq!(moved, one_by_one(&ranges, &steps), "the last first");
        // the line between the first two places is the line it was, whole
        let line_after = |l: usize| after.line_start_offset(l)..after.line_start_offset(l + 1);
        assert_eq!(moved[6].range, line_after(6), "line 6, between the first two places");
        // and one past the line put in and the two taken out moves by them
        assert_eq!(moved[14].range, line_after(15), "line 14, below the line put in on 8");
        assert_eq!(moved[20].range, line_after(21), "line 22, below the two taken out");
        // their undo: each the other way, the first first, on the text the
        // ones before it left
        let mut now = text.clone();
        let mut undo: Vec<(Range<usize>, String)> = Vec::new();
        for (range, new) in &steps {
            undo.push((range.start..range.start + new.len(), now.slice(range.clone()).to_string()));
            now.replace(range.clone(), new);
        }
        undo.reverse();
        let undo: Vec<(Range<usize>, &str)> = undo.iter().map(|(r, s)| (r.clone(), s.as_str())).collect();
        assert_eq!(made(&after, &undo).to_string(), text.to_string());
        let moved_ranges: Vec<Range<usize>> = moved.iter().map(|d| d.range.clone()).collect();
        assert_eq!(together(&moved_ranges, &undo), one_by_one(&moved_ranges, &undo), "the first first");
        // edits that overlap are made in turn
        let tangled: Vec<(Range<usize>, &str)> = vec![(line(3).start..line(3).start + 4, "ab\n"), (line(3).start + 1..line(3).start + 6, "")];
        assert_eq!(together(&ranges, &tangled), one_by_one(&ranges, &tangled), "tangled");
    }
}
