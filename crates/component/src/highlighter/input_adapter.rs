use std::{
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::{HighlightStyle, SharedString, Task};
use gpui_base::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit as BaseInputEdit, InputHighlighter,
    InputHighlighterFactory,
};
use ropey::Rope;
use tree_sitter::{InputEdit, ParseOptions, Parser, Point};

use super::{InjectionParseData, LanguageRegistry, SyntaxHighlighter};

pub(crate) fn input_highlighter_factory() -> InputHighlighterFactory {
    Rc::new(|language| {
        let config = LanguageRegistry::singleton().language(language)?;
        config.has_grammar().then(|| {
            Box::new(TreeSitterInputHighlighter::new(language)) as Box<dyn InputHighlighter>
        })
    })
}

struct TreeSitterInputHighlighter {
    inner: Rc<RefCell<SyntaxHighlighter>>,
    parse_task: Rc<RefCell<Option<Task<()>>>>,
    /// The last parse a keystroke made took more than its share of the
    /// key (`PARSE_SHARE`): the keys after it edit the trees alone, until
    /// the parse once the typing pauses lands (`parsed_at_the_key`).
    slow: Rc<Cell<bool>>,
}

/// The largest text a keystroke parses on the drawing thread: a larger
/// one has its trees edited there, what the edit touched left plain, and
/// is parsed off it once the typing pauses, as the largest always were.
/// The parse grows with the text, some 1 ms for each 50 KB of Rust, and
/// the frame after a key takes 2 to 3 ms of the 4 ms a keystroke has
/// (Kvist's ARCHITECTURE.md §9): parsed up to 256 KB, a key in a 150 KB
/// file took 4.6 to 5.5 ms, its parse 1.8 to 2.2 (elundqvist/kvist#370).
const SYNC_PARSE_MAX_BYTES: usize = 48 * 1024;

/// The largest Markdown text a keystroke parses there, every text's
/// limit before: whether a Markdown paragraph waits for the pause for its
/// colours is elundqvist/kvist#376's to settle, by what tree-sitter-md
/// costs a key, and an emphasis is coloured as it is typed in a README
/// of 190 KB (#54).
const MARKDOWN_SYNC_PARSE_MAX_BYTES: usize = 256 * 1024;

/// What of a keystroke's 4 ms its parse may take, what is left once the
/// frame has its 2 to 3: a parse that took longer leaves the keys after
/// it to edit the trees alone until the typing pauses. On a machine
/// busy with builds, a key in a 47 KB file took 4.6 to 5.7 ms, its parse
/// some 1 to 1.4 of it, every one of 31 over (elundqvist/kvist#370).
const PARSE_SHARE: Duration = Duration::from_millis(1);

/// Whether a keystroke in a text of `len` bytes in `language` parses it
/// then: one within its language's limit, where the last a key parsed
/// took its share of the key and no more (`slow`). A Markdown text goes
/// by its old limit alone (`MARKDOWN_SYNC_PARSE_MAX_BYTES`).
fn parsed_at_the_key(language: &str, len: usize, slow: bool) -> bool {
    if language == "markdown" {
        return len <= MARKDOWN_SYNC_PARSE_MAX_BYTES;
    }
    len <= SYNC_PARSE_MAX_BYTES && !slow
}

/// A text above this size has its trees freed on a thread of their own
/// when its highlighter goes; a smaller one's take no time to free.
const FREE_ELSEWHERE_BYTES: usize = 256 * 1024;

impl Drop for TreeSitterInputHighlighter {
    fn drop(&mut self) {
        let Ok(mut inner) = self.inner.try_borrow_mut() else {
            return;
        };
        let (parse, bytes) = inner.take_parse();
        if bytes > FREE_ELSEWHERE_BYTES && !parse.is_empty() {
            std::thread::spawn(move || drop(parse));
        }
    }
}

impl TreeSitterInputHighlighter {
    fn new(language: &str) -> Self {
        Self {
            inner: Rc::new(RefCell::new(SyntaxHighlighter::new(language))),
            parse_task: Rc::new(RefCell::new(None)),
            slow: Rc::new(Cell::new(false)),
        }
    }
}

impl SyntaxHighlighter {
    pub(crate) fn update_input(
        &mut self,
        edit: Option<BaseInputEdit>,
        text: &Rope,
        timeout: Option<Duration>,
    ) -> bool {
        self.update(edit.map(to_tree_sitter_edit), text, timeout)
    }
}

impl InputHighlighter for TreeSitterInputHighlighter {
    fn language(&self) -> SharedString {
        self.inner.borrow().language().clone()
    }

    fn update(
        &mut self,
        edit: Option<BaseInputEdit>,
        text: &Rope,
        folding: bool,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<EditorState>,
    ) {
        const SYNC_PARSE_TIMEOUT: Duration = Duration::from_millis(2);
        const PARSE_DEBOUNCE: Duration = Duration::from_millis(150);

        let edit = edit.map(to_tree_sitter_edit);
        let completed = {
            let mut highlighter = self.inner.borrow_mut();
            // a text made anew that is the text it was, a paste of the
            // whole text over itself, changes nothing: the trees, the
            // colours and a parse on its way stand. The trees taken, a text
            // the keystroke parses was taken for parsed already, as it was
            // the same, and stood plain and without its fold chevrons until
            // the next edit; a larger one was plain until its parse
            if edit.is_none() && highlighter.text().eq(text) {
                return;
            }
            let at_the_key = parsed_at_the_key(highlighter.language(), text.len(), self.slow.get());
            // a text made anew that waits for the parse after the pause:
            // the old text's trees colour it until then, as VS Code and
            // IntelliJ keep the old colours, where it stood plain
            // (elundqvist/kvist#401)
            if edit.is_none() && !at_the_key {
                highlighter.keep_colours_through(text);
                false
            } else if !at_the_key {
                highlighter.edit_tree(edit, text);
                false
            } else {
                // a text made anew, parsed now: the trees are of none of
                // it, and a large text's are freed on a thread of their
                // own, as when the highlighter goes
                if edit.is_none() {
                    let (parse, bytes) = highlighter.take_parse();
                    if bytes > FREE_ELSEWHERE_BYTES && !parse.is_empty() {
                        std::thread::spawn(move || drop(parse));
                    }
                }
                let began = Instant::now();
                let completed = highlighter.update(edit, text, Some(SYNC_PARSE_TIMEOUT));
                self.slow.set(began.elapsed() > PARSE_SHARE);
                completed
            }
        };
        if completed {
            self.parse_task.borrow_mut().take();
            return;
        }

        let highlighter = self.inner.clone();
        let parse_task = self.parse_task.clone();
        let slow = self.slow.clone();
        let language = highlighter.borrow().language().clone();
        // trees kept from a text made anew are no start for its parse
        let old_tree = highlighter.borrow().tree_of_text().cloned();
        let cancel = Arc::new(AtomicBool::new(false));
        // Off the UI thread an injected span may take as long as it needs,
        // until the next edit cancels it: a span too large to parse in the
        // time a keystroke gives it is finished here.
        let injection_data =
            highlighter
                .borrow()
                .injection_parse_data()
                .map(|data| InjectionParseData {
                    parse_timeout: None,
                    cancel: Some(cancel.clone()),
                    ..data
                });
        let text = text.clone();
        let text_for_apply = text.clone();

        let task = cx.spawn_in(window, async move |entity, cx| {
            struct CancelOnDrop(Arc<AtomicBool>);
            impl Drop for CancelOnDrop {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Relaxed);
                }
            }
            let _cancel_guard = CancelOnDrop(cancel.clone());
            cx.background_executor().timer(PARSE_DEBOUNCE).await;

            let parse_cancel = cancel.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let config = LanguageRegistry::singleton().language(&language)?;
                    let grammar = config.language.as_ref()?;
                    let mut parser = Parser::new();
                    parser.set_language(grammar).ok()?;
                    let mut progress = |_: &tree_sitter::ParseState| {
                        if parse_cancel.load(Ordering::Relaxed) {
                            std::ops::ControlFlow::Break(())
                        } else {
                            std::ops::ControlFlow::Continue(())
                        }
                    };
                    let options = ParseOptions::new().progress_callback(&mut progress);
                    let tree = parser.parse_with_options(
                        &mut |offset, _| {
                            if offset >= text.len() {
                                ""
                            } else {
                                let (chunk, chunk_byte_ix) = text.chunk(offset);
                                &chunk[offset - chunk_byte_ix..]
                            }
                        },
                        old_tree.as_ref(),
                        Some(options),
                    )?;
                    if parse_cancel.load(Ordering::Relaxed) {
                        return None;
                    }
                    let injections = injection_data.map_or_else(Default::default, |data| {
                        SyntaxHighlighter::compute_injection_layers(data, &tree, &text).0
                    });
                    if parse_cancel.load(Ordering::Relaxed) {
                        return None;
                    }
                    let folds = if folding {
                        extract_fold_ranges(&tree)
                    } else {
                        Vec::new()
                    };
                    Some((tree, injections, folds))
                })
                .await;

            if let Some((tree, injections, folds)) = result {
                let replaced = highlighter
                    .borrow_mut()
                    .apply_background_tree(tree, &text_for_apply, injections);
                // the trees kept through a text made anew to colour it,
                // the old text's whole trees, which share nothing with
                // the new ones, are freed on a thread of their own, as
                // when the highlighter goes
                if let Some(replaced) = replaced.filter(|parse| !parse.is_empty()) {
                    std::thread::spawn(move || drop(replaced));
                }
                // the typing paused: the next key tries a parse again
                slow.set(false);
                let _ = entity.update(cx, |state, cx| {
                    state.apply_highlighter_fold_candidates(folds, cx);
                });
            }
        });
        parse_task.borrow_mut().replace(task);
    }

    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        self.inner.borrow().styles(range, resolver)
    }
    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        self.inner
            .borrow()
            .tree_of_text()
            .map(extract_fold_ranges)
            .unwrap_or_default()
    }

    fn fold_ranges_for_edit(&self, range: Range<usize>, _: &Rope) -> Vec<FoldRange> {
        self.inner
            .borrow()
            .tree_of_text()
            .map(|tree| extract_fold_ranges_in_range(tree, range))
            .unwrap_or_default()
    }
}

/// `each` of the named children of `node` that `bytes` may reach: from
/// the first that ends past its start, which the cursor finds down the
/// tree's own balanced runs of siblings, to the last that begins before
/// its end. Every child was walked to find them: a keystroke in a file of
/// 35,000 functions walked all 35,000 for its fold candidates, 8 to 9 ms
/// of every key (elundqvist/kvist#370).
fn for_children_reached<'t>(
    node: tree_sitter::Node<'t>,
    bytes: &Range<usize>,
    mut each: impl FnMut(tree_sitter::Node<'t>),
) {
    let mut cursor = node.walk();
    if cursor.goto_first_child_for_byte(bytes.start).is_none() {
        return;
    }
    loop {
        let child = cursor.node();
        if child.start_byte() >= bytes.end {
            break;
        }
        if child.is_named() {
            each(child);
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn to_tree_sitter_edit(edit: BaseInputEdit) -> InputEdit {
    InputEdit {
        start_byte: edit.start_byte,
        old_end_byte: edit.old_end_byte,
        new_end_byte: edit.new_end_byte,
        start_position: Point::new(edit.start_position.row, edit.start_position.column),
        old_end_position: Point::new(edit.old_end_position.row, edit.old_end_position.column),
        new_end_position: Point::new(edit.new_end_position.row, edit.new_end_position.column),
    }
}

fn extract_fold_ranges(tree: &tree_sitter::Tree) -> Vec<FoldRange> {
    extract_fold_ranges_in_range(tree, 0..usize::MAX)
}

fn extract_fold_ranges_in_range(
    tree: &tree_sitter::Tree,
    byte_range: Range<usize>,
) -> Vec<FoldRange> {
    fn collect(node: tree_sitter::Node, bytes: &Range<usize>, ranges: &mut Vec<FoldRange>) {
        if node.end_byte() <= bytes.start || node.start_byte() >= bytes.end {
            return;
        }
        let start = node.start_position().row;
        let end = node.end_position().row;
        if end.saturating_sub(start) < 2 {
            return;
        }
        ranges.push(FoldRange::new(start, end));
        for_children_reached(node, bytes, |child| collect(child, bytes, ranges));
    }

    let root = tree.root_node();
    let mut ranges = Vec::new();
    for_children_reached(root, &byte_range, |child| {
        collect(child, &byte_range, &mut ranges)
    });
    ranges.sort_by_key(|range| range.start_line);
    ranges.dedup_by_key(|range| range.start_line);
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the whole walk found: every named child walked, and those the
    /// range does not reach passed over.
    #[cfg(feature = "tree-sitter-languages")]
    fn walked_whole(tree: &tree_sitter::Tree, bytes: Range<usize>) -> Vec<FoldRange> {
        fn collect(node: tree_sitter::Node, bytes: &Range<usize>, ranges: &mut Vec<FoldRange>) {
            if node.end_byte() <= bytes.start || node.start_byte() >= bytes.end {
                return;
            }
            let (start, end) = (node.start_position().row, node.end_position().row);
            if end.saturating_sub(start) < 2 {
                return;
            }
            ranges.push(FoldRange::new(start, end));
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect(child, bytes, ranges);
            }
        }
        let root = tree.root_node();
        let mut ranges = Vec::new();
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            collect(child, &bytes, &mut ranges);
        }
        ranges.sort_by_key(|range| range.start_line);
        ranges.dedup_by_key(|range| range.start_line);
        ranges
    }

    /// A keystroke parses a text on the drawing thread only where the
    /// parse leaves the frame its share of the 4 ms a key has: a file of
    /// 150 KB took 4.6 to 5.5 ms a key, 1.8 to 2.2 of it the parse, and
    /// one of 47 KB on a busy machine 4.6 to 5.7, 1 to 1.4 of it the parse
    /// (elundqvist/kvist#370). A Markdown text keeps its old limit, which
    /// elundqvist/kvist#376 is to settle.
    #[test]
    fn test_a_keystroke_parses_only_a_small_text() {
        assert!(parsed_at_the_key("rust", 25_000, false));
        assert!(parsed_at_the_key("rust", 48 * 1024, false));
        assert!(!parsed_at_the_key("rust", 100_000, false), "100 KB took 1.3 to 1.6 ms to parse");
        assert!(!parsed_at_the_key("rust", 150_000, false));
        assert!(!parsed_at_the_key("rust", 25_000, true), "the last key's parse took more than its share");
        assert!(parsed_at_the_key("markdown", 190_000, false), "#54's README");
        assert!(parsed_at_the_key("markdown", 190_000, true));
        assert!(!parsed_at_the_key("markdown", 300_000, false));
    }

    /// A text made anew that is the text it was, a paste of the whole text
    /// over itself, keeps its colours, its fold candidates and the parse
    /// on its way, at a size a keystroke parses and a larger one; a text
    /// made anew that differs is parsed afresh, a larger one coloured as
    /// the old was until then. The trees taken, the small text was taken
    /// for parsed, as it was the same, and stood plain and without its
    /// chevrons until the next edit (elundqvist/kvist#384).
    #[gpui::test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_text_made_anew_as_it_was_keeps_its_colours(cx: &mut gpui::TestAppContext) {
        use super::super::HighlightTheme;

        cx.update(crate::init);
        let (editor, cx) = cx.add_window_view(|window, cx| EditorState::new(window, cx));
        let function = |i: usize| format!("fn f{i}() {{\n    if {i} > 1 {{\n        let v = {i};\n    }}\n}}\n");
        let small: String = (0..20).map(function).collect();
        let large: String = (0..2_000).map(function).collect();
        assert!(parsed_at_the_key("rust", small.len(), false));
        assert!(!parsed_at_the_key("rust", large.len(), false));
        let theme = HighlightTheme::default_dark();
        let coloured = |h: &TreeSitterInputHighlighter, text: &str| {
            h.styles(&(0..text.len()), theme.as_ref()).iter().any(|(_, style)| style.color.is_some())
        };
        let mut handed = |h: &mut TreeSitterInputHighlighter, text: &Rope| {
            cx.update(|window, cx| editor.update(cx, |_, cx| h.update(None, text, true, window, cx)));
        };
        for (source, functions) in [(small, 20), (large, 2_000)] {
            let rope = Rope::from(source.as_str());
            let mut h = TreeSitterInputHighlighter::new("rust");
            // parsed whole, as a pause's parse leaves it
            assert!(h.inner.borrow_mut().update(None, &rope, None));
            assert!(coloured(&h, &source));
            let folds = h.fold_ranges(&rope);
            assert_eq!(folds.len(), 2 * functions, "a function and its `if` each");
            handed(&mut h, &rope);
            assert!(coloured(&h, &source), "the same text, {} bytes", source.len());
            assert_eq!(h.fold_ranges(&rope), folds);
            assert!(h.parse_task.borrow().is_none(), "nothing to parse");
            // another text is the trees' no longer: one the keystroke
            // parses is parsed afresh, and a larger one keeps the old
            // colours until the parse after the pause, where it stood
            // plain (elundqvist/kvist#401)
            let other = source.replace("let v", "let w");
            let other_rope = Rope::from(other.as_str());
            handed(&mut h, &other_rope);
            let parsed_now = parsed_at_the_key("rust", other.len(), false);
            assert!(coloured(&h, &other), "{} bytes", other.len());
            assert_eq!(h.inner.borrow().tree_of_text().is_some(), parsed_now);
            assert_eq!(h.parse_task.borrow().is_none(), parsed_now);
        }
    }

    /// A large text made anew is kept coloured by the old text's trees
    /// until the parse after the pause lands, and from then on the trees
    /// are of it: the next key's parse starts from them and an edit's lines
    /// find their folds. Left marked as kept, every key's parse after a
    /// paste over a large file started from nothing, and an edit found no
    /// folds on its lines, so a rename after it opened every fold between
    /// its first and last change (elundqvist/kvist#401, #404).
    #[gpui::test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_large_text_made_anew_is_the_trees_once_its_parse_lands(cx: &mut gpui::TestAppContext) {
        cx.update(crate::init);
        let (editor, cx) = cx.add_window_view(|window, cx| EditorState::new(window, cx));
        let function = |i: usize| format!("fn f{i}() {{\n    if {i} > 1 {{\n        let v = {i};\n    }}\n}}\n");
        let source: String = (0..2_000).map(function).collect();
        let other = source.replace("let v", "let w");
        assert!(!parsed_at_the_key("rust", other.len(), false));
        let mut h = TreeSitterInputHighlighter::new("rust");
        assert!(h.inner.borrow_mut().update(None, &Rope::from(source.as_str()), None));
        let other_rope = Rope::from(other.as_str());
        cx.update(|window, cx| editor.update(cx, |_, cx| h.update(None, &other_rope, true, window, cx)));
        assert!(h.inner.borrow().tree_of_text().is_none(), "kept, not of the text");
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert!(h.inner.borrow().tree_of_text().is_some(), "of the text once its parse lands");
        let at = other.find("fn f1000()").unwrap();
        let line = at..at + function(1000).len();
        assert_eq!(h.fold_ranges_for_edit(line, &other_rope).len(), 2, "a function and its `if`");
        assert_eq!(h.fold_ranges(&other_rope).len(), 4_000);
    }

    /// An edit's fold candidates are those of the whole text it reaches,
    /// found by looking at the children it reaches and no others: in a
    /// file of 35,000 functions every one was walked at every key, 8 to
    /// 9 ms of it (elundqvist/kvist#370).
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_an_edit_looks_only_at_the_children_it_reaches() {
        let function = |i: usize| format!("fn f{i}() {{\n    if {i} > 1 {{\n        let v = {i};\n\n    }}\n}}\n");
        let source: String = (0..5_000).map(function).collect();
        let config = LanguageRegistry::singleton().language("rust").unwrap();
        let mut parser = Parser::new();
        parser.set_language(config.language.as_ref().unwrap()).unwrap();
        let tree = parser.parse(&source, None).unwrap();
        assert_eq!(extract_fold_ranges(&tree).len(), 10_000, "a function and its `if` each");

        let at = (0..2_500).map(|i| function(i).len()).sum::<usize>() + "fn f2500() {\n    if".len();
        let ranges = [
            at..at,
            at..at + 40,
            at - 30..at + 300,
            0..0,
            0..source.len(),
            source.len()..source.len(),
            source.len() - 3..source.len(),
        ];
        for range in ranges {
            assert_eq!(
                extract_fold_ranges_in_range(&tree, range.clone()),
                walked_whole(&tree, range.clone()),
                "{range:?}"
            );
        }
        let mut looked_at = Vec::new();
        for_children_reached(tree.root_node(), &(at..at + 1), |child| {
            looked_at.push(child.start_position().row)
        });
        assert_eq!(looked_at, [2_500 * 6], "the function typed in, of 5,000");
    }
}
