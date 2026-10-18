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
        self.take_edit(&[], edit, text, folding, window, cx);
    }

    fn update_steps(
        &mut self,
        edits: &[BaseInputEdit],
        edit: BaseInputEdit,
        text: &Rope,
        folding: bool,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<EditorState>,
    ) {
        let steps: Vec<InputEdit> = edits.iter().copied().map(to_tree_sitter_edit).collect();
        self.take_edit(&steps, Some(edit), text, folding, window, cx);
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

impl TreeSitterInputHighlighter {
    /// Take the text `edit` made, or the text made anew where there is no
    /// edit, and `steps`, the changes `edit` covers one by one where there
    /// are any (`InputHighlighter::update_steps`): parsed at the key where
    /// the text is small enough, else the trees moved with it and the
    /// parse left to the pause.
    fn take_edit(
        &mut self,
        steps: &[InputEdit],
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
                highlighter.edit_tree_in_steps(steps, edit, text);
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
                let completed = highlighter.update_in_steps(steps, edit, text, Some(SYNC_PARSE_TIMEOUT));
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
                    let mut read = |offset: usize, _| {
                        if offset >= text.len() {
                            ""
                        } else {
                            let (chunk, chunk_byte_ix) = text.chunk(offset);
                            &chunk[offset - chunk_byte_ix..]
                        }
                    };
                    let options = ParseOptions::new().progress_callback(&mut progress);
                    let mut tree =
                        parser.parse_with_options(&mut read, old_tree.as_ref(), Some(options))?;
                    // a tree made from nothing that the next parse would
                    // not reuse is parsed again from itself, here, so the
                    // first key after a README opens does not parse all
                    // of it on the drawing thread (elundqvist/kvist#376)
                    if old_tree.is_none() && super::highlighter::parsed_again_from_itself(grammar) {
                        let options = ParseOptions::new().progress_callback(&mut progress);
                        tree = parser.parse_with_options(&mut read, Some(&tree), Some(options))?;
                    }
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

    /// A character typed at `at` of `text`: the edit the editor hands its
    /// highlighter for it.
    #[cfg(feature = "tree-sitter-languages")]
    fn typed(text: &mut String, at: usize, c: char) -> BaseInputEdit {
        let row = text[..at].matches('\n').count();
        let column = at - text[..at].rfind('\n').map_or(0, |n| n + 1);
        text.insert(at, c);
        BaseInputEdit {
            start_byte: at,
            old_end_byte: at,
            new_end_byte: at + c.len_utf8(),
            start_position: gpui_base::input::Point::new(row, column),
            old_end_position: gpui_base::input::Point::new(row, column),
            new_end_position: gpui_base::input::Point::new(row, column + c.len_utf8()),
        }
    }

    /// Keys typed into a README just opened, its whole parse out of the
    /// 2 ms the keystroke gave it and the parse after the pause not yet
    /// in, are coloured once that parse lands, as a fresh parse colours the
    /// text. Each such key handed the tree of the empty text, edited by the
    /// key alone, to the parse as its start, which finished at once with a
    /// tree of nothing: the README stood plain, whatever was typed after,
    /// until it was opened again (elundqvist/kvist#376).
    #[gpui::test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_keys_typed_before_an_opened_readme_is_parsed_are_coloured_once_it_is(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::HighlightTheme;
        use super::super::highlighter::tests::{fresh_highlighter, readme};

        cx.update(crate::init);
        let (editor, cx) = cx.add_window_view(|window, cx| EditorState::new(window, cx));
        let mut text = readme(60);
        assert!(parsed_at_the_key("markdown", text.len(), false));
        let mut h = TreeSitterInputHighlighter::new("markdown");
        let mut handed = |h: &mut TreeSitterInputHighlighter, edit: Option<BaseInputEdit>, text: &str| {
            let rope = Rope::from(text);
            cx.update(|window, cx| editor.update(cx, |_, cx| h.update(edit, &rope, true, window, cx)));
        };
        // opened: the editor hands the text with an empty edit at its start
        let start = gpui_base::input::Point::new(0, 0);
        let open = BaseInputEdit {
            start_byte: 0,
            old_end_byte: 0,
            new_end_byte: 0,
            start_position: start,
            old_end_position: start,
            new_end_position: start,
        };
        handed(&mut h, Some(open), &text);
        assert!(h.inner.borrow().tree().is_none(), "the whole parse out of its time");
        // keys typed before the parse after the pause
        let at = text.find("Paragraph 3 says").unwrap() + "Paragraph 3 says".len();
        for (i, c) in "abc".chars().enumerate() {
            let edit = typed(&mut text, at + i, c);
            handed(&mut h, Some(edit), &text);
        }
        assert!(h.parse_task.borrow().is_some());
        // the pause
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert!(h.parse_task.borrow().is_some());
        let fresh = fresh_highlighter("markdown", &text);
        let inner = h.inner.borrow();
        assert_eq!(inner.tree().unwrap().root_node().to_sexp(), fresh.tree().unwrap().root_node().to_sexp());
        let theme = HighlightTheme::default_dark();
        assert!(inner.styles(&(0..text.len()), theme.as_ref()) == fresh.styles(&(0..text.len()), theme.as_ref()));
    }

    /// The parse after the pause of a README parsed from nothing parses it
    /// again from its own tree, which the next key's parse reuses:
    /// tree-sitter-md reused nothing of a tree made from nothing of a text
    /// that begins with front matter, and the first key after a README
    /// opened parsed all of it again on the drawing thread, out of its
    /// time (elundqvist/kvist#376).
    #[gpui::test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_readme_parsed_after_the_pause_from_nothing_is_parsed_again_from_itself(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::highlighter::tests::{key_steps, readme};

        cx.update(crate::init);
        let (editor, cx) = cx.add_window_view(|window, cx| EditorState::new(window, cx));
        // past what a keystroke parses: the whole parse is the pause's
        let text = readme(400);
        assert!(!parsed_at_the_key("markdown", text.len(), false));
        let mut h = TreeSitterInputHighlighter::new("markdown");
        let rope = Rope::from(text.as_str());
        cx.update(|window, cx| editor.update(cx, |_, cx| h.update(None, &rope, true, window, cx)));
        assert!(h.inner.borrow().tree().is_none());
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        let landed = h.inner.borrow().tree_of_text().cloned().expect("the parse after the pause");
        let config = LanguageRegistry::singleton().language("markdown").unwrap();
        let mut parser = Parser::new();
        parser.set_language(config.language.as_ref().unwrap()).unwrap();
        let fresh = parser.parse(&text, None).unwrap();
        let (from_landed, from_fresh) = (key_steps(&mut parser, &landed, &text), key_steps(&mut parser, &fresh, &text));
        assert!(from_landed * 10 < from_fresh, "{from_landed} steps from the tree that landed, {from_fresh} from a fresh one");
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

    /// A rename's changes in a text too large to parse at the key, handed
    /// one by one, move the tree its folds are found in as they moved the
    /// text: the candidates an edit's lines find are those a parse of the
    /// new text finds, so a fold a change lies within is found again and
    /// stays shut. Moved by the one edit from the first change to the last,
    /// every function between was taken into it, its candidates went, and
    /// the folds it held opened (elundqvist/kvist#404).
    #[gpui::test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_changes_one_by_one_keep_a_large_texts_fold_candidates(cx: &mut gpui::TestAppContext) {
        use gpui_base::input::{Point as BasePoint, RopeExt as _};

        cx.update(crate::init);
        let (editor, cx) = cx.add_window_view(|window, cx| EditorState::new(window, cx));
        let function = |i: usize| format!("fn f{i}(sum: u32) -> u32 {{\n    if sum > {i} {{\n        sum + 1\n    }} else {{\n        0\n    }}\n}}\n\n");
        let old: String = (0..1_500).map(function).collect();
        assert!(!parsed_at_the_key("rust", old.len(), false));
        let mut h = TreeSitterInputHighlighter::new("rust");
        let old_rope = Rope::from(old.as_str());
        assert!(h.inner.borrow_mut().update(None, &old_rope, None));
        // `sum` renamed `total` in every function, the last first, each
        // change where it is in the text the ones before it left
        let places: Vec<usize> = old.rmatch_indices("sum").map(|(at, _)| at).collect();
        let steps: Vec<BaseInputEdit> = places
            .iter()
            .map(|&at| {
                let start = old_rope.offset_to_point(at);
                BaseInputEdit {
                    start_byte: at,
                    old_end_byte: at + 3,
                    new_end_byte: at + 5,
                    start_position: start,
                    old_end_position: BasePoint::new(start.row, start.column + 3),
                    new_end_position: BasePoint::new(start.row, start.column + 5),
                }
            })
            .collect();
        let new = old.replace("sum", "total");
        let new_rope = Rope::from(new.as_str());
        let (first, last) = (*places.last().unwrap(), places[0]);
        let new_last = last + 3 + (new.len() - old.len());
        let covering = BaseInputEdit {
            start_byte: first,
            old_end_byte: last + 3,
            new_end_byte: new_last,
            start_position: old_rope.offset_to_point(first),
            old_end_position: old_rope.offset_to_point(last + 3),
            new_end_position: new_rope.offset_to_point(new_last),
        };
        cx.update(|window, cx| editor.update(cx, |_, cx| h.update_steps(&steps, covering, &new_rope, true, window, cx)));
        assert!(h.parse_task.borrow().is_some(), "moved, its parse left to the pause");
        let config = LanguageRegistry::singleton().language("rust").unwrap();
        let mut parser = Parser::new();
        parser.set_language(config.language.as_ref().unwrap()).unwrap();
        let fresh = parser.parse(&new, None).unwrap();
        let lines = 0..new.len();
        let found = h.fold_ranges_for_edit(lines.clone(), &new_rope);
        assert_eq!(found.len(), 4_500, "a function, its `if` and its `else` each");
        assert_eq!(found, extract_fold_ranges_in_range(&fresh, lines));
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
