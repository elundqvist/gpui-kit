use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
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

use super::{InjectionParseData, InjectionParseSource, LanguageRegistry, SyntaxHighlighter};

pub(crate) fn input_highlighter_factory() -> InputHighlighterFactory {
    Rc::new(|language| {
        let config = LanguageRegistry::singleton().language(language)?;
        config.has_grammar().then(|| {
            Box::new(TreeSitterInputHighlighter::new(language)) as Box<dyn InputHighlighter>
        })
    })
}

struct TreeSitterInputHighlighter {
    shared: Shared,
    /// The language's queries compiled on a thread and the highlighter
    /// built from them (`Shared::build`), where the process had none as it
    /// was made: begun with its first text, and gone with it, as when the
    /// editor's language changes.
    building: Option<Task<()>>,
}

/// What the editor's highlighter shares with the tasks it starts, the
/// parse after the pause and its build.
#[derive(Clone)]
struct Shared {
    /// The key the highlighter was asked for by (`new`): the registry's,
    /// which is not always the name of the config it holds, and the one the
    /// build asks for the language by (elundqvist/kvist#597).
    key: SharedString,
    inner: Rc<RefCell<SyntaxHighlighter>>,
    parse_task: Rc<RefCell<Option<Task<()>>>>,
    /// How long the parses at the key take, and whether the last took
    /// more than its share of the key (`ParsePace`).
    pace: Rc<RefCell<ParsePace>>,
    /// `inner` holds the text and neither parses nor colours it
    /// (`SyntaxHighlighter::unbuilt`): the process had not compiled the
    /// language's queries as the highlighter was made, and a thread
    /// compiles them (`TreeSitterInputHighlighter::building`).
    unbuilt: Rc<Cell<bool>>,
    /// Whether the editor looks for fold candidates, as the last text
    /// handed said: the build gives them for the text it parses.
    folding: Rc<Cell<bool>>,
    /// How long a parse at the key runs before it is left to the pause
    /// (`SYNC_PARSE_TIMEOUT`): a test sets another.
    sync_parse_timeout: Duration,
}

/// What of a keystroke a parse at the key may run before it is left to the
/// parse after the pause.
const SYNC_PARSE_TIMEOUT: Duration = Duration::from_millis(2);

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

/// The least and the most of a keystroke's 4 ms its parse may take
/// (`ParsePace::share`): the least what is left once the frame has its 2
/// to 3. On a machine busy with builds, a key in a 47 KB file took 4.6 to
/// 5.7 ms, its parse some 1 to 1.4 of it, every one of 31 over the least
/// (elundqvist/kvist#370).
const PARSE_SHARE_LEAST: Duration = Duration::from_millis(1);
const PARSE_SHARE_MOST: Duration = Duration::from_millis(4);

/// How many of the last parses at the key a parse's share is taken from.
const PACE_PARSES: usize = 9;

/// How long the parses at the key take: a parse that takes more than
/// its share of the key, twice the median of the ones before it, from
/// `PARSE_SHARE_LEAST` to `PARSE_SHARE_MOST`, leaves the keys after it to
/// edit the trees alone until the typing pauses. A fixed 1 ms made a
/// loaded machine, whose parses all took a little over it, flap: a key
/// parsed, the next only moved, its parse 150 ms later after the pause,
/// the next parsed again (elundqvist/kvist#598).
#[derive(Default)]
struct ParsePace {
    /// How long the last parses at the key took, the newest last.
    recent: VecDeque<Duration>,
    /// The last parse at the key took more than its share (`parsed`): the
    /// keys after it edit the trees alone, until the parse once the typing
    /// pauses lands (`parsed_at_the_key`).
    slow: bool,
}

impl ParsePace {
    /// What the next parse at the key may take.
    fn share(&self) -> Duration {
        let mut recent: Vec<Duration> = self.recent.iter().copied().collect();
        recent.sort_unstable();
        recent.get(recent.len() / 2).map_or(PARSE_SHARE_LEAST, |median| {
            (*median * 2).clamp(PARSE_SHARE_LEAST, PARSE_SHARE_MOST)
        })
    }

    /// A parse at the key took `took`, and ran out of its time where
    /// `ran_out`: then the parse is the pause's whatever its share, and a
    /// key's parse after it would run out as it did.
    fn parsed(&mut self, took: Duration, ran_out: bool) {
        self.slow = ran_out || took > self.share();
        if self.recent.len() == PACE_PARSES {
            self.recent.pop_front();
        }
        self.recent.push_back(took);
    }

    /// The parse after the pause landed: the next key parses again.
    fn paused(&mut self) {
        self.slow = false;
    }
}

/// Whether a keystroke in a text of `len` bytes in `language` parses it
/// then: one within its language's limit, where the last a key parsed
/// took its share of the key and no more (`slow`, `ParsePace`). A
/// Markdown text goes by its old limit alone
/// (`MARKDOWN_SYNC_PARSE_MAX_BYTES`).
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
        let Ok(mut inner) = self.shared.inner.try_borrow_mut() else {
            return;
        };
        let (parse, bytes) = inner.take_parse();
        if bytes > FREE_ELSEWHERE_BYTES && !parse.is_empty() {
            std::thread::spawn(move || drop(parse));
        }
    }
}

impl TreeSitterInputHighlighter {
    /// The editor's highlighter of `language`: built at once where the
    /// process has the language's queries, else unbuilt, the text plain,
    /// until a thread has compiled them (`SyntaxHighlighter::if_compiled`,
    /// elundqvist/kvist#597).
    fn new(language: &str) -> Self {
        let built = SyntaxHighlighter::if_compiled(language);
        let unbuilt = built.is_none();
        let mut highlighter = built.unwrap_or_else(|| SyntaxHighlighter::unbuilt(language));
        // a fence's language that no one has compiled the query of is
        // compiled on a thread, not at the key (elundqvist/kvist#593)
        highlighter.defer_cold_queries();
        Self {
            shared: Shared {
                key: SharedString::from(language.to_string()),
                inner: Rc::new(RefCell::new(highlighter)),
                parse_task: Rc::new(RefCell::new(None)),
                pace: Rc::default(),
                unbuilt: Rc::new(Cell::new(unbuilt)),
                folding: Rc::new(Cell::new(false)),
                sync_parse_timeout: SYNC_PARSE_TIMEOUT,
            },
            building: None,
        }
    }

    /// `Shared::take_edit`, and where the highlighter is unbuilt, its
    /// build begun, once.
    fn take_edit(
        &mut self,
        steps: &[InputEdit],
        edit: Option<BaseInputEdit>,
        text: &Rope,
        folding: bool,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<EditorState>,
    ) {
        self.shared.take_edit(steps, edit, text, folding, window, cx);
        if self.shared.unbuilt.get() && self.building.is_none() {
            self.building = Some(self.shared.build(window, cx));
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
        self.shared.inner.borrow().language().clone()
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
        self.shared.inner.borrow().styles(range, resolver)
    }
    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        self.shared
            .inner
            .borrow()
            .tree_of_text()
            .map(extract_fold_ranges)
            .unwrap_or_default()
    }

    fn fold_ranges_for_edit(&self, range: Range<usize>, _: &Rope) -> Vec<FoldRange> {
        self.shared
            .inner
            .borrow()
            .tree_of_text()
            .map(|tree| extract_fold_ranges_in_range(tree, range))
            .unwrap_or_default()
    }
}

impl Shared {
    /// Take the text `edit` made, or the text made anew where there is no
    /// edit, and `steps`, the changes `edit` covers one by one where there
    /// are any (`InputHighlighter::update_steps`): parsed at the key where
    /// the text is small enough, else the trees moved with it and the
    /// parse left to the pause. Unbuilt, the highlighter holds the text
    /// alone, for its build to take. Whether the text is parsed now.
    fn take_edit(
        &self,
        steps: &[InputEdit],
        edit: Option<BaseInputEdit>,
        text: &Rope,
        folding: bool,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<EditorState>,
    ) -> bool {
        const PARSE_DEBOUNCE: Duration = Duration::from_millis(150);

        self.folding.set(folding);
        if self.unbuilt.get() {
            // an unbuilt highlighter's update takes the text and no more
            self.inner.borrow_mut().update(None, text, None);
            return false;
        }
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
                return false;
            }
            let at_the_key =
                parsed_at_the_key(highlighter.language(), text.len(), self.pace.borrow().slow);
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
                let completed = highlighter.update_in_steps(steps, edit, text, Some(self.sync_parse_timeout));
                let ran_out = !completed && highlighter.is_stale();
                self.pace.borrow_mut().parsed(began.elapsed(), ran_out);
                completed
            }
        };
        if completed {
            self.parse_task.borrow_mut().take();
            return true;
        }

        let highlighter = self.inner.clone();
        let parse_task = self.parse_task.clone();
        let pace = self.pace.clone();
        let language = highlighter.borrow().language().clone();
        let cancel = Arc::new(AtomicBool::new(false));
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
            // the tree and the layers, taken once the typing has paused,
            // shared and made the parse's own on its thread
            // (`InjectionParseSource`): taken at the key, they were held
            // until the next key let go of the task, which the executor
            // does after the key, so the next key's edit copied the layers
            // to move them, and the tree's nodes it went through, a list of
            // 3,000 items' 3,000 children with them (elundqvist/kvist#376).
            // Trees kept from a text made anew are no start for its parse.
            let (old_tree, injection_source, again) = {
                let highlighter = highlighter.borrow();
                (
                    highlighter.tree_of_text().cloned(),
                    highlighter.injection_parse_source(),
                    highlighter.again_pending(),
                )
            };

            let parse_cancel = cancel.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    // the parse after the pause, as the one at the key, and
                    // marked as cut short where a key typed meanwhile
                    // cancelled it
                    let _parse =
                        super::highlighter::PauseParse::enter(&language, text.len(), &parse_cancel);
                    // the layers made the parse's own first, so that a key
                    // typed during the host's parse finds them held by its
                    // highlighter alone, and moves them in place
                    let injection_data = injection_source.map(InjectionParseSource::into_data);
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
                    // a tree made from nothing that the next parse would
                    // not reuse, which the key's parse left to this one,
                    // is parsed from itself, edited at its start, so the
                    // first key after a README opens does not build all of
                    // it again on the drawing thread, nor does this parse
                    // build it twice (elundqvist/kvist#376, #593)
                    let again_from = old_tree
                        .as_ref()
                        .filter(|_| again)
                        .map(super::highlighter::edited_at_its_start);
                    let mut tree = parser.parse_with_options(
                        &mut read,
                        again_from.as_ref().or(old_tree.as_ref()),
                        Some(options),
                    )?;
                    // and where there was none, the parse of the text from
                    // nothing is parsed again so
                    if old_tree.is_none() && super::highlighter::parsed_again_from_itself(grammar) {
                        let options = ParseOptions::new().progress_callback(&mut progress);
                        let again = super::highlighter::edited_at_its_start(&tree);
                        tree = parser.parse_with_options(&mut read, Some(&again), Some(options))?;
                    }
                    if parse_cancel.load(Ordering::Relaxed) {
                        return None;
                    }
                    // Off the UI thread an injected span may take as long as
                    // it needs, until the next edit cancels it: a span too
                    // large to parse in the time a keystroke gives it is
                    // finished here.
                    let injections = injection_data.map_or_else(Default::default, |data| {
                        let data = InjectionParseData {
                            parse_timeout: None,
                            cancel: Some(parse_cancel.clone()),
                            ..data
                        };
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
                pace.borrow_mut().paused();
                let _ = entity.update(cx, |state, cx| {
                    state.apply_highlighter_fold_candidates(folds, cx);
                });
            }
        });
        parse_task.borrow_mut().replace(task);
        false
    }

    /// Compile the language's queries on a thread, then build the
    /// highlighter from them on this one and take the text it held as an
    /// opened file's: parsed at once where it is small enough, its fold
    /// candidates and its colours drawn, else left to the parse after the
    /// pause. Compiled as the highlighter was made, they took 13 ms (Rust)
    /// to 39 (TypeScript) of the frame after the first file of a language
    /// opened (elundqvist/kvist#597).
    fn build(&self, window: &mut gpui::Window, cx: &mut gpui::Context<EditorState>) -> Task<()> {
        let shared = self.clone();
        // begun here, not by the task, which first runs once the frame it
        // is made in has been presented: some 30 to 50 ms later under Xvfb.
        // A second highlighter of the language made meanwhile waits for the
        // first's task, and the task goes on if the highlighter is gone
        let compiling = SyntaxHighlighter::compile_queries(&self.key, cx.background_executor());
        cx.spawn_in(window, async move |entity, cx| {
            if let Some(compiling) = compiling {
                compiling.await;
            }
            let Some(entity) = entity.upgrade() else {
                return;
            };
            let _ = entity.update_in(cx, |state, window, cx| {
                let text = shared.inner.borrow().text().clone();
                let mut highlighter = SyntaxHighlighter::new(&shared.key);
                highlighter.defer_cold_queries();
                *shared.inner.borrow_mut() = highlighter;
                shared.unbuilt.set(false);
                let folding = shared.folding.get();
                if shared.take_edit(&[], None, &text, folding, window, cx) {
                    let folds = shared
                        .inner
                        .borrow()
                        .tree_of_text()
                        .filter(|_| folding)
                        .map(extract_fold_ranges)
                        .unwrap_or_default();
                    state.apply_highlighter_fold_candidates(folds, cx);
                }
            });
        })
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

/// The fold candidates of the nodes of `tree` that `byte_range` reaches,
/// each node of two lines or more and those within it. The nodes still to
/// look at are kept on a stack of their own, in the order a call a level
/// took them: tree-sitter's error recovery nests the tree of a 1.4 MB
/// paste of Rust with a key typed before it some 7,700 levels deep, and a
/// call a level ran out of the stack of the worker thread the parse after
/// the pause runs this on, which took the whole app down.
fn extract_fold_ranges_in_range(
    tree: &tree_sitter::Tree,
    byte_range: Range<usize>,
) -> Vec<FoldRange> {
    fn push_children<'t>(
        node: tree_sitter::Node<'t>,
        bytes: &Range<usize>,
        nodes: &mut Vec<tree_sitter::Node<'t>>,
    ) {
        let first = nodes.len();
        for_children_reached(node, bytes, |child| nodes.push(child));
        nodes[first..].reverse();
    }

    let mut ranges = Vec::new();
    let mut nodes = Vec::new();
    push_children(tree.root_node(), &byte_range, &mut nodes);
    while let Some(node) = nodes.pop() {
        if node.end_byte() <= byte_range.start || node.start_byte() >= byte_range.end {
            continue;
        }
        let start = node.start_position().row;
        let end = node.end_position().row;
        if end.saturating_sub(start) < 2 {
            continue;
        }
        ranges.push(FoldRange::new(start, end));
        push_children(node, &byte_range, &mut nodes);
    }
    ranges.sort_by_key(|range| range.start_line);
    ranges.dedup_by_key(|range| range.start_line);
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The editor's highlighter of `language`, the language's queries
    /// compiled first, as a file of a language opened before finds them:
    /// built at once.
    #[cfg(feature = "tree-sitter-languages")]
    pub(super) fn built(language: &str) -> TreeSitterInputHighlighter {
        let config = LanguageRegistry::singleton().language(language).unwrap();
        super::super::queries::host(&config).unwrap();
        let h = TreeSitterInputHighlighter::new(language);
        assert!(!h.shared.unbuilt.get(), "{language} built at once");
        h
    }

    /// `h`'s parses at the key, the text's and its spans', given all the
    /// time they take, so that a test asserts what a key parsed and not
    /// how long it took: on a busy machine a parse at the key ran out of
    /// its 2 ms, and a test of one failed (elundqvist/kvist#608).
    #[cfg(feature = "tree-sitter-languages")]
    fn untimed(h: &mut TreeSitterInputHighlighter) {
        h.shared.sync_parse_timeout = Duration::from_secs(60);
        h.shared.inner.borrow_mut().untimed_spans();
    }

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

    /// A parse at the key may take twice the median of the last ones, from
    /// 1 to 4 ms; one that took more, or ran out of its time, leaves the
    /// keys after it to move the trees until the pause. With a fixed 1 ms
    /// a loaded machine whose parses all took a little over it flapped: a
    /// key parsed, the keys after it only moved until the pause, and the
    /// next key parsed again and went over again (elundqvist/kvist#598).
    #[test]
    fn test_a_parse_at_the_key_may_take_twice_the_median_of_the_last() {
        let us = Duration::from_micros;
        let mut pace = ParsePace::default();
        assert_eq!(pace.share(), PARSE_SHARE_LEAST, "nothing parsed yet");

        // a loaded machine, every parse a little over 1 ms: the first,
        // with nothing to go by, is over, and once the typing has paused
        // none after it is
        let loaded = [1200, 1100, 1400, 1300, 1500, 1200, 1100, 1300, 1400, 1200, 1500, 1100, 1300, 1200, 1400, 1300];
        pace.parsed(us(loaded[0]), false);
        assert!(pace.slow);
        pace.paused();
        for (i, took) in loaded[1..].iter().enumerate() {
            pace.parsed(us(*took), false);
            assert!(!pace.slow, "parse {} of {took} µs within {:?}", i + 2, pace.share());
        }
        assert_eq!(pace.share(), us(2600), "twice the median, 1.3 ms");
        // a parse far over the ones before it is over, and moves the
        // median little
        pace.parsed(us(5000), false);
        assert!(pace.slow);
        pace.paused();
        assert_eq!(pace.share(), us(2600));
        // one that ran out of its time is the pause's whatever it took
        pace.parsed(us(2000), true);
        assert!(pace.slow);
        pace.paused();

        // the share is taken from the last parses alone, at least 1 ms
        let mut quick = ParsePace::default();
        for _ in 0..PACE_PARSES {
            quick.parsed(us(5000), false);
        }
        assert_eq!(quick.share(), PARSE_SHARE_MOST, "at most 4 ms");
        for _ in 0..PACE_PARSES / 2 + 1 {
            quick.parsed(us(200), false);
        }
        assert_eq!(quick.share(), PARSE_SHARE_LEAST, "the newer, quicker parses");
        quick.parsed(us(1100), false);
        assert!(quick.slow);
        assert_eq!(quick.recent.len(), PACE_PARSES);
    }

    /// A text made anew that is the text it was, a paste of the whole text
    /// over itself, keeps its colours, its fold candidates and the parse
    /// on its way, at a size a keystroke parses and a larger one; a text
    /// made anew that differs is parsed afresh, a larger one coloured as
    /// the old was until then. The trees taken, the small text was taken
    /// for parsed, as it was the same, and stood plain and without its
    /// chevrons until the next edit (elundqvist/kvist#384), whatever the
    /// key's parse takes (`untimed`, elundqvist/kvist#608).
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
            let mut h = built("rust");
            untimed(&mut h);
            // parsed whole, as a pause's parse leaves it
            assert!(h.shared.inner.borrow_mut().update(None, &rope, None));
            assert!(coloured(&h, &source));
            let folds = h.fold_ranges(&rope);
            assert_eq!(folds.len(), 2 * functions, "a function and its `if` each");
            handed(&mut h, &rope);
            assert!(coloured(&h, &source), "the same text, {} bytes", source.len());
            assert_eq!(h.fold_ranges(&rope), folds);
            assert!(h.shared.parse_task.borrow().is_none(), "nothing to parse");
            // another text is the trees' no longer: one the keystroke
            // parses is parsed afresh, and a larger one keeps the old
            // colours until the parse after the pause, where it stood
            // plain (elundqvist/kvist#401)
            let other = source.replace("let v", "let w");
            let other_rope = Rope::from(other.as_str());
            handed(&mut h, &other_rope);
            let parsed_now = parsed_at_the_key("rust", other.len(), false);
            assert!(coloured(&h, &other), "{} bytes", other.len());
            assert_eq!(h.shared.inner.borrow().tree_of_text().is_some(), parsed_now);
            assert_eq!(h.shared.parse_task.borrow().is_none(), parsed_now);
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
    /// until it was opened again (elundqvist/kvist#376). The key's parse
    /// has no time at all, so that it runs out of it on a fast machine too
    /// (elundqvist/kvist#608).
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
        let mut h = built("markdown");
        h.shared.sync_parse_timeout = Duration::ZERO;
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
        assert!(h.shared.inner.borrow().tree().is_none(), "the whole parse out of its time");
        // keys typed before the parse after the pause
        let at = text.find("Paragraph 3 says").unwrap() + "Paragraph 3 says".len();
        for (i, c) in "abc".chars().enumerate() {
            let edit = typed(&mut text, at + i, c);
            handed(&mut h, Some(edit), &text);
        }
        assert!(h.shared.parse_task.borrow().is_some());
        // the pause
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert!(h.shared.parse_task.borrow().is_some());
        let fresh = fresh_highlighter("markdown", &text);
        let inner = h.shared.inner.borrow();
        assert_eq!(inner.tree().unwrap().root_node().to_sexp(), fresh.tree().unwrap().root_node().to_sexp());
        let theme = HighlightTheme::default_dark();
        assert!(inner.styles(&(0..text.len()), theme.as_ref()) == fresh.styles(&(0..text.len()), theme.as_ref()));
    }

    /// The parse after the pause of a README parsed from nothing parses it
    /// again from its own tree, which the next key's parse reuses:
    /// tree-sitter-md reused nothing of a tree made from nothing of a text
    /// that begins with front matter, and the first key after a README
    /// opened parsed all of it again on the drawing thread, out of its
    /// time (elundqvist/kvist#376). Nor of one with none, where the parse
    /// again from itself, unedited, reused the tree whole and left it as it
    /// was (elundqvist/kvist#593).
    #[gpui::test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_readme_parsed_after_the_pause_from_nothing_is_parsed_again_from_itself(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::highlighter::tests::{key_steps, readme, without_front_matter};

        cx.update(crate::init);
        let (editor, cx) = cx.add_window_view(|window, cx| EditorState::new(window, cx));
        // past what a keystroke parses: the whole parse is the pause's
        let with_front_matter = readme(400);
        for text in [without_front_matter(&with_front_matter), with_front_matter] {
            assert!(!parsed_at_the_key("markdown", text.len(), false));
            let mut h = built("markdown");
            let rope = Rope::from(text.as_str());
            cx.update(|window, cx| {
                editor.update(cx, |_, cx| h.update(None, &rope, true, window, cx))
            });
            assert!(h.shared.inner.borrow().tree().is_none());
            cx.executor().advance_clock(Duration::from_secs(1));
            cx.run_until_parked();
            let landed = h
                .shared
                .inner
                .borrow()
                .tree_of_text()
                .cloned()
                .expect("the parse after the pause");
            let config = LanguageRegistry::singleton().language("markdown").unwrap();
            let mut parser = Parser::new();
            parser
                .set_language(config.language.as_ref().unwrap())
                .unwrap();
            let fresh = parser.parse(&text, None).unwrap();
            let (from_landed, from_fresh) = (
                key_steps(&mut parser, &landed, &text),
                key_steps(&mut parser, &fresh, &text),
            );
            assert!(
                from_landed * 10 < from_fresh,
                "{from_landed} steps from the tree that landed, {from_fresh} from a fresh one"
            );
        }
    }

    /// A README opened at the key, parsed there from nothing, is left to
    /// the parse after the pause to parse again from itself, which the keys
    /// after it reuse: on the drawing thread that rebuilt every node of the
    /// text, and where it ran out of its time left the tree as it was, for
    /// the first key to build again (elundqvist/kvist#593).
    #[gpui::test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_readme_opened_at_the_key_is_parsed_again_by_the_pause_not_at_the_key(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::highlighter::tests::{key_steps, readme, without_front_matter};

        cx.update(crate::init);
        let (editor, cx) = cx.add_window_view(|window, cx| EditorState::new(window, cx));
        // small enough for the key's 2 ms, even unoptimised, where it
        // does not run out of them (then the pause parses it from nothing,
        // as the test of `readme(60)` has it)
        let text = without_front_matter(&readme(12));
        assert!(parsed_at_the_key("markdown", text.len(), false));
        let rope = Rope::from(text.as_str());
        let mut h = built("markdown");
        cx.update(|window, cx| editor.update(cx, |_, cx| h.update(None, &rope, true, window, cx)));
        if h.shared.inner.borrow().tree().is_some() {
            assert!(h.shared.inner.borrow().again_pending(), "not parsed again at the key");
        }
        assert!(h.shared.parse_task.borrow().is_some(), "left to the pause");
        // the same text handed again with an empty edit, as an open is,
        // leaves the pause its parse
        let start = gpui_base::input::Point::new(0, 0);
        let open = BaseInputEdit {
            start_byte: 0,
            old_end_byte: 0,
            new_end_byte: 0,
            start_position: start,
            old_end_position: start,
            new_end_position: start,
        };
        cx.update(|window, cx| editor.update(cx, |_, cx| h.update(Some(open), &rope, true, window, cx)));
        assert!(h.shared.parse_task.borrow().is_some(), "still left to the pause");
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert!(!h.shared.inner.borrow().again_pending());
        let landed = h.shared.inner.borrow().tree_of_text().cloned().expect("the parse after the pause");
        let config = LanguageRegistry::singleton().language("markdown").unwrap();
        let mut parser = Parser::new();
        parser.set_language(config.language.as_ref().unwrap()).unwrap();
        let fresh = parser.parse(&text, None).unwrap();
        let (from_landed, from_fresh) = (
            key_steps(&mut parser, &landed, &text),
            key_steps(&mut parser, &fresh, &text),
        );
        assert!(from_landed * 5 < from_fresh, "{from_landed} steps from the tree that landed, {from_fresh} from a fresh one");
    }

    /// A fence of a language no one has compiled the highlight query of
    /// stands plain at the key, which compiles nothing, and is coloured
    /// once the parse after the pause lands; the same text opened again
    /// after it is coloured at the key (elundqvist/kvist#593), whatever
    /// the key's parse takes (`untimed`, elundqvist/kvist#608).
    #[gpui::test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_fence_of_a_language_not_compiled_yet_is_coloured_once_the_pause_parse_lands(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::HighlightTheme;
        use super::super::highlighter::tests::{fresh_highlighter, register_rust_as};

        cx.update(crate::init);
        let (editor, cx) = cx.add_window_view(|window, cx| EditorState::new(window, cx));
        // two names of a length, the second compiled for the colours to
        // compare with
        let (name, twin) = ("kvist-test-adapter-cold", "kvist-test-adapter-twin");
        register_rust_as(name);
        register_rust_as(twin);
        let make = |name: &str| format!("# Title\n\nA paragraph.\n\n```{name}\nfn f() {{}}\n```\n");
        let (text, twin_text) = (make(name), make(twin));
        let theme = HighlightTheme::default_dark();
        let keyword = text.find("fn f").unwrap();
        let range = keyword..keyword + 2;
        let want = fresh_highlighter("markdown", &twin_text).styles(&range, theme.as_ref());
        assert!(want.iter().any(|(_, style)| style.color.is_some()));

        let rope = Rope::from(text.as_str());
        let mut h = built("markdown");
        untimed(&mut h);
        cx.update(|window, cx| editor.update(cx, |_, cx| h.update(None, &rope, true, window, cx)));
        assert!(
            h.shared.inner.borrow().tree().is_some(),
            "the text parsed at the key"
        );
        assert_ne!(
            h.styles(&range, theme.as_ref()),
            want,
            "the fence left to the pause"
        );
        assert!(h.shared.parse_task.borrow().is_some(), "left to the pause");
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(h.styles(&range, theme.as_ref()), want);

        let mut again = built("markdown");
        untimed(&mut again);
        cx.update(|window, cx| editor.update(cx, |_, cx| again.update(None, &rope, true, window, cx)));
        assert_eq!(again.styles(&range, theme.as_ref()), want, "at the key");
    }

    /// The editor's highlighter of a language whose queries no one has
    /// compiled compiles nothing as it is made and handed its text: the
    /// text stands plain, a key typed meanwhile is held, and once a thread
    /// has compiled them the highlighter is built and colours the text as
    /// a fresh parse colours it, and hands the editor its fold candidates
    /// and a repaint. The next of the language is built at once, and one
    /// gone before its build lands is built for no one. Compiled where the
    /// highlighter was made, they took the frame after the first file of a
    /// language opened 13 ms for Rust and 39 for TypeScript
    /// (elundqvist/kvist#597).
    #[gpui::test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_language_not_compiled_yet_is_built_once_a_thread_has_compiled_it(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::HighlightTheme;
        use super::super::highlighter::tests::{fresh_highlighter, register_rust_as};

        cx.update(crate::init);
        let (editor, cx) = cx.add_window_view(|window, cx| EditorState::new(window, cx));
        let (name, gone) = ("kvist-test-adapter-unbuilt", "kvist-test-adapter-unbuilt-gone");
        register_rust_as(name);
        register_rust_as(gone);
        let config = LanguageRegistry::singleton().language(name).unwrap();
        let theme = HighlightTheme::default_dark();
        let coloured = |h: &TreeSitterInputHighlighter, text: &str| {
            h.styles(&(0..text.len()), theme.as_ref()).iter().any(|(_, style)| style.color.is_some())
        };
        let notified = Rc::new(Cell::new(0));
        let count = notified.clone();
        cx.update(|_, cx| cx.observe(&editor, move |_, _| count.set(count.get() + 1)).detach());
        let mut text = (0..20)
            .map(|i| format!("fn f{i}() {{\n    if {i} > 1 {{\n        let v = {i};\n    }}\n}}\n"))
            .collect::<String>();
        let mut h = TreeSitterInputHighlighter::new(name);
        assert!(h.shared.unbuilt.get());
        // the build's parse is not out of its time on a busy machine
        h.shared.sync_parse_timeout = Duration::from_secs(60);
        let rope = Rope::from(text.as_str());
        cx.update(|window, cx| editor.update(cx, |_, cx| h.update(None, &rope, true, window, cx)));
        assert!(!super::super::queries::host_compiled(&config), "nothing compiled on the drawing thread");
        assert!(!coloured(&h, &text), "plain until built");
        assert!(h.fold_ranges(&rope).is_empty());
        let at = text.find("= 7;").unwrap() + "= 7".len();
        let edit = typed(&mut text, at, '0');
        let rope = Rope::from(text.as_str());
        cx.update(|window, cx| editor.update(cx, |_, cx| h.update(Some(edit), &rope, true, window, cx)));
        assert!(h.building.is_some() && h.shared.unbuilt.get());
        let before = notified.get();
        cx.run_until_parked();
        assert!(!h.shared.unbuilt.get());
        assert!(super::super::queries::host_compiled(&config));
        let fresh = fresh_highlighter(name, &text);
        assert!(coloured(&h, &text));
        assert!(h.styles(&(0..text.len()), theme.as_ref()) == fresh.styles(&(0..text.len()), theme.as_ref()));
        assert_eq!(h.fold_ranges(&rope), extract_fold_ranges(fresh.tree().unwrap()));
        assert_eq!(h.fold_ranges(&rope).len(), 40, "a function and its `if` each");
        // and the editor has them, and is told to draw again
        assert!(notified.get() > before, "repainted");
        let candidates: Vec<usize> =
            (0..text.lines().count()).filter(|&line| editor.read_with(cx, |state, _| state.is_fold_candidate(line))).collect();
        let want: Vec<usize> = extract_fold_ranges(fresh.tree().unwrap()).iter().map(|fold| fold.start_line).collect();
        assert_eq!(candidates, want, "the fold candidates reached the editor");

        assert!(!TreeSitterInputHighlighter::new(name).shared.unbuilt.get(), "built at once");

        let mut h = TreeSitterInputHighlighter::new(gone);
        cx.update(|window, cx| editor.update(cx, |_, cx| h.update(None, &rope, true, window, cx)));
        drop(h);
        cx.run_until_parked();
    }

    /// A text over what a keystroke parses that an unbuilt highlighter
    /// held is, once built, left to the parse after the pause as an opened
    /// file's is: plain until it lands, and then coloured, with its fold
    /// candidates given to the editor by that parse.
    #[gpui::test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_large_text_held_by_an_unbuilt_highlighter_is_parsed_after_the_pause_once_built(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::HighlightTheme;
        use super::super::highlighter::tests::{fresh_highlighter, register_rust_as};

        cx.update(crate::init);
        let (editor, cx) = cx.add_window_view(|window, cx| EditorState::new(window, cx));
        let name = "kvist-test-adapter-unbuilt-large";
        register_rust_as(name);
        let theme = HighlightTheme::default_dark();
        let coloured = |h: &TreeSitterInputHighlighter, text: &str| {
            h.styles(&(0..text.len()), theme.as_ref()).iter().any(|(_, style)| style.color.is_some())
        };
        let text: String = (0..2_000)
            .map(|i| format!("fn f{i}() {{\n    if {i} > 1 {{\n        let v = {i};\n    }}\n}}\n"))
            .collect();
        assert!(!parsed_at_the_key("rust", text.len(), false));
        let rope = Rope::from(text.as_str());
        let mut h = TreeSitterInputHighlighter::new(name);
        assert!(h.shared.unbuilt.get());
        cx.update(|window, cx| editor.update(cx, |_, cx| h.update(None, &rope, true, window, cx)));
        cx.run_until_parked();
        assert!(!h.shared.unbuilt.get(), "built");
        assert!(h.shared.parse_task.borrow().is_some(), "its parse left to the pause");
        assert!(h.shared.inner.borrow().tree_of_text().is_none());
        assert!(!coloured(&h, &text), "plain until then");
        assert!(!editor.read_with(cx, |state, _| state.is_fold_candidate(0)));
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        let fresh = fresh_highlighter(name, &text);
        assert!(h.styles(&(0..text.len()), theme.as_ref()) == fresh.styles(&(0..text.len()), theme.as_ref()));
        assert!(coloured(&h, &text));
        assert_eq!(h.fold_ranges(&rope).len(), 4_000);
        assert!(editor.read_with(cx, |state, _| state.is_fold_candidate(0)), "the pause's parse gave the candidates");
    }

    /// A highlighter asked for by a key the registry holds under another
    /// name than its config's is built by the key, as it is made: built by
    /// the config's name, the language was another or none, and the text
    /// stood plain for good (elundqvist/kvist#597).
    #[gpui::test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_key_unlike_the_name_of_its_config_is_built_by_the_key(cx: &mut gpui::TestAppContext) {
        use super::super::HighlightTheme;
        use super::super::highlighter::tests::fresh_highlighter;

        cx.update(crate::init);
        let (editor, cx) = cx.add_window_view(|window, cx| EditorState::new(window, cx));
        let (key, name) = ("kvist-test-adapter-key", "kvist-test-adapter-key-name");
        let rust = LanguageRegistry::singleton().language("rust").unwrap();
        let config = super::super::LanguageConfig::new(name, rust.language.clone().unwrap(), vec![], &rust.highlights, "", "");
        LanguageRegistry::singleton().register(key, &config);
        assert!(LanguageRegistry::singleton().language(name).is_none(), "the name is no key");
        let theme = HighlightTheme::default_dark();
        let text = "fn f() {\n    let v = 1;\n}\n";
        let rope = Rope::from(text);
        let mut h = TreeSitterInputHighlighter::new(key);
        assert!(h.shared.unbuilt.get());
        h.shared.sync_parse_timeout = Duration::from_secs(60);
        cx.update(|window, cx| editor.update(cx, |_, cx| h.update(None, &rope, true, window, cx)));
        cx.run_until_parked();
        assert!(!h.shared.unbuilt.get());
        assert_eq!(h.language(), name, "the config's name");
        let range = 0..text.len();
        assert!(h.styles(&range, theme.as_ref()).iter().any(|(_, style)| style.color.is_some()));
        assert!(h.styles(&range, theme.as_ref()) == fresh_highlighter(key, text).styles(&range, theme.as_ref()));
    }

    /// A second highlighter of a language, made while the first's queries
    /// are being compiled, waits for that task: another would hold a thread
    /// of the background executor in `queries::host` until the first ended,
    /// up to 59 ms for TypeScript in a dev build. A highlighter gone before
    /// the task ended leaves it to end, and the next finds the queries.
    #[gpui::test(iterations = 30)]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_second_highlighter_made_while_the_queries_compile_waits_for_the_task(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::highlighter::tests::register_rust_as;

        cx.update(crate::init);
        let (editor, cx) = cx.add_window_view(|window, cx| EditorState::new(window, cx));
        // another config per iteration, whose queries no one has compiled
        static ITERATIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let name = &format!("kvist-test-adapter-unbuilt-twice-{}", ITERATIONS.fetch_add(1, Ordering::Relaxed));
        register_rust_as(name);
        let config = LanguageRegistry::singleton().language(name).unwrap();
        let rope = Rope::from("fn f() {\n    let v = 1;\n}\n");
        let mut highlighters: Vec<_> = (0..3).map(|_| TreeSitterInputHighlighter::new(name)).collect();
        for h in &mut highlighters {
            assert!(h.shared.unbuilt.get());
            cx.update(|window, cx| editor.update(cx, |_, cx| h.update(None, &rope, true, window, cx)));
        }
        assert_eq!(super::super::queries::host_tasks_sent(&config), 1, "one task for the three");
        drop(highlighters.remove(0));
        cx.run_until_parked();
        for h in &highlighters {
            assert!(!h.shared.unbuilt.get(), "built once the one task ended");
        }
        assert_eq!(super::super::queries::host_tasks_sent(&config), 1);
        assert!(super::super::queries::host_compiled(&config));
    }

    /// A parse at the key that ran out of its 2 ms is the pause's whatever
    /// share the key allows, and one that returned for another reason, a
    /// fence whose query no one has compiled left to the pause, is not: it
    /// is judged by the time it took. The share is the most the key gives,
    /// 4 ms, and a parse that stopped at once took none of it.
    #[gpui::test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_only_a_parse_that_ran_out_of_its_time_is_slow_whatever_the_share(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::highlighter::tests::register_rust_as;

        cx.update(crate::init);
        let (editor, cx) = cx.add_window_view(|window, cx| EditorState::new(window, cx));
        let generous = |h: &TreeSitterInputHighlighter| {
            let mut pace = h.shared.pace.borrow_mut();
            for _ in 0..PACE_PARSES {
                pace.parsed(Duration::from_millis(5), false);
            }
            pace.paused();
            assert_eq!(pace.share(), PARSE_SHARE_MOST);
        };
        let function = |i: usize| format!("fn f{i}() {{\n    if {i} > 1 {{\n        let v = {i};\n    }}\n}}\n");
        let source: String = (0..800).map(function).collect();
        assert!(parsed_at_the_key("rust", source.len(), false));

        // a text made anew, parsed at the key, which has no time at all
        let mut h = built("rust");
        h.shared.sync_parse_timeout = Duration::from_secs(60);
        let rope = Rope::from(source.as_str());
        cx.update(|window, cx| editor.update(cx, |_, cx| h.update(None, &rope, true, window, cx)));
        assert!(h.shared.inner.borrow().tree_of_text().is_some() && !h.shared.inner.borrow().is_stale());
        generous(&h);
        assert!(!h.shared.pace.borrow().slow);
        h.shared.sync_parse_timeout = Duration::ZERO;
        let other = source.replace("let v", "let w");
        let other_rope = Rope::from(other.as_str());
        cx.update(|window, cx| editor.update(cx, |_, cx| h.update(None, &other_rope, true, window, cx)));
        assert!(h.shared.inner.borrow().is_stale(), "ran out");
        assert!(h.shared.pace.borrow().slow, "the pause's, whatever its share");
        assert!(h.shared.parse_task.borrow().is_some());

        // a README whose fence is of a language no one has compiled: the
        // update returns false, the parse is the pause's, and the trees
        // are not stale
        let name = "kvist-test-adapter-pace-fence";
        register_rust_as(name);
        let text = format!("# Title\n\nA paragraph.\n\n```{name}\nfn f() {{}}\n```\n");
        let mut h = built("markdown");
        h.shared.sync_parse_timeout = Duration::from_secs(60);
        generous(&h);
        let rope = Rope::from(text.as_str());
        cx.update(|window, cx| editor.update(cx, |_, cx| h.update(None, &rope, true, window, cx)));
        assert!(h.shared.parse_task.borrow().is_some(), "left to the pause");
        assert!(!h.shared.inner.borrow().is_stale(), "not out of its time");
        let pace = h.shared.pace.borrow();
        let took = *pace.recent.back().unwrap();
        assert_eq!(pace.recent.len(), PACE_PARSES);
        // judged by the time it took, if a busy machine let it take little
        assert!(pace.slow == (took > PARSE_SHARE_MOST), "took {took:?}");
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
        let mut h = built("rust");
        assert!(h.shared.inner.borrow_mut().update(None, &Rope::from(source.as_str()), None));
        let other_rope = Rope::from(other.as_str());
        cx.update(|window, cx| editor.update(cx, |_, cx| h.update(None, &other_rope, true, window, cx)));
        assert!(h.shared.inner.borrow().tree_of_text().is_none(), "kept, not of the text");
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert!(h.shared.inner.borrow().tree_of_text().is_some(), "of the text once its parse lands");
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
        let mut h = built("rust");
        let old_rope = Rope::from(old.as_str());
        assert!(h.shared.inner.borrow_mut().update(None, &old_rope, None));
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
        assert!(h.shared.parse_task.borrow().is_some(), "moved, its parse left to the pause");
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

    /// The fold candidates of a tree nested thousands of levels deep are
    /// found on a thread's stack as those of a flat one are: tree-sitter's
    /// error recovery nests the tree of a 1.4 MB paste of Rust with a key
    /// typed before it some 7,700 levels deep, and the walk, a call a
    /// level, ran out of the stack of the worker thread the parse after the
    /// pause runs on, which took the whole app down. Parentheses nested a
    /// line each nest a tree as deep, and on a thread of 256 KB of stack the
    /// walk found as it did before.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_folds_of_a_tree_nested_deep_are_found_on_a_small_stack() {
        const DEPTH: usize = 20_000;
        let source = format!(
            "fn deep() {{\n    let v = {}1{};\n}}\nfn after() {{\n\n}}\n",
            "(\n".repeat(DEPTH),
            "\n)".repeat(DEPTH)
        );
        let config = LanguageRegistry::singleton().language("rust").unwrap();
        let mut parser = Parser::new();
        parser.set_language(config.language.as_ref().unwrap()).unwrap();
        let tree = parser.parse(&source, None).unwrap();
        let middle = source.find("1\n").unwrap();
        let (whole, reached) = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                (
                    extract_fold_ranges(&tree),
                    extract_fold_ranges_in_range(&tree, middle..middle + 1),
                )
            })
            .unwrap()
            .join()
            .unwrap();
        // the function, each parenthesis from the `let`'s on, and the
        // function after it, on lines of their own
        let lines = 2 * DEPTH + 2;
        let mut expected = vec![FoldRange::new(0, lines)];
        expected.extend((1..=DEPTH).map(|row| FoldRange::new(row, 2 * DEPTH + 2 - row)));
        expected.push(FoldRange::new(lines + 1, lines + 3));
        assert_eq!(whole, expected);
        assert_eq!(reached, expected[..=DEPTH]);
    }
}

/// Keys typed into a large Rust file and into a Markdown fence at any pace
/// end coloured as a fresh parse colours the text (elundqvist/kvist#423).
#[cfg(test)]
#[cfg(feature = "tree-sitter-languages")]
mod typing_at_any_pace {
    use super::super::HighlightTheme;
    use super::super::highlighter::tests::fresh_highlighter;
    use super::tests::built;
    use super::*;

    /// A character typed at `at` of `text`: the edit the editor hands its
    /// highlighter for it.
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

    const LITERALS: &str = "fn literals(value: i32) -> i32 {\n    let doubled = value * 2;\n    doubled\n}\n";

    /// A Rust file of 97 KB, too large to parse at the key.
    fn big_rust() -> String {
        let mut rust = String::from(LITERALS);
        for i in 0.. {
            if rust.len() >= 97_000 {
                break;
            }
            rust += &format!("/// doc {i}\nfn f{i}(x: u32) -> u32 {{\n    let y = x + {i};\n    y * 2\n}}\n");
        }
        rust
    }

    /// A Markdown file of 34 KB with a Rust fence of 10 KB, QA's SMALL.md.
    fn small_md() -> String {
        let mut fence = String::from(LITERALS);
        for i in 0.. {
            if fence.len() >= 10_000 {
                break;
            }
            fence += &format!("fn f{i}(x: u32) -> u32 {{\n    let y = x + {i};\n    y * 2\n}}\n");
        }
        let mut md = format!("# Small\n\nSome text first.\n\n```rust\n{fence}```\n\n");
        while md.len() < 34_000 {
            md += "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod.\n\n";
        }
        md
    }

    /// ` 42 "s" true 7` typed after `let doubled = value * 2;` in `source`,
    /// for each of `seeds` at its own pace: between two keys 30 or 70 ms go
    /// by, or 200, the parse after the pause begun and some of the tasks
    /// after it run, or all of them, or only some of the tasks with no
    /// time going by. Once the typing has paused, the line is coloured as
    /// a fresh parse of the text colours it.
    fn typed_at_any_pace(cx: &mut gpui::TestAppContext, language: &str, source: String, seeds: std::ops::Range<u64>) {
        cx.update(crate::init);
        let (editor, cx) = cx.add_window_view(|window, cx| EditorState::new(window, cx));
        let theme = HighlightTheme::default_dark();
        let typing = " 42 \"s\" true 7";
        let mut failures = Vec::new();
        for seed in seeds {
            let mut state = seed;
            let mut next = |n: u64| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (state >> 33) % n
            };
            let mut text = source.clone();
            let mut h = built(language);
            let rope = Rope::from(text.as_str());
            cx.update(|window, cx| editor.update(cx, |_, cx| h.update(None, &rope, true, window, cx)));
            cx.executor().advance_clock(Duration::from_secs(1));
            cx.run_until_parked();
            let at = text.find("value * 2;").unwrap() + "value * 2;".len();
            for (i, c) in typing.chars().enumerate() {
                let edit = typed(&mut text, at + i, c);
                let rope = Rope::from(text.as_str());
                cx.update(|window, cx| editor.update(cx, |_, cx| h.update(Some(edit), &rope, true, window, cx)));
                match next(5) {
                    0 => cx.executor().advance_clock(Duration::from_millis(30)),
                    1 => cx.executor().advance_clock(Duration::from_millis(70)),
                    2 => {
                        cx.executor().advance_clock(Duration::from_millis(200));
                        for _ in 0..next(6) {
                            cx.executor().tick();
                        }
                    }
                    3 => {
                        cx.executor().advance_clock(Duration::from_millis(200));
                        cx.run_until_parked();
                    }
                    _ => {
                        for _ in 0..next(4) {
                            cx.executor().tick();
                        }
                    }
                }
            }
            cx.executor().advance_clock(Duration::from_secs(1));
            cx.run_until_parked();
            let fresh = fresh_highlighter(language, &text);
            let line = at - "    let doubled = value * 2;".len()..at + typing.len();
            let got = h.shared.inner.borrow().styles(&line, theme.as_ref());
            let want = fresh.styles(&line, theme.as_ref());
            if got != want {
                failures.push(format!("seed {seed}:\n got {got:?}\nwant {want:?}"));
            }
        }
        assert!(failures.is_empty(), "{} paces:\n{}", failures.len(), failures.join("\n"));
    }

    #[gpui::test]
    fn in_a_large_rust_file(cx: &mut gpui::TestAppContext) {
        typed_at_any_pace(cx, "rust", big_rust(), 0..40);
    }

    #[gpui::test]
    fn in_a_markdown_fence(cx: &mut gpui::TestAppContext) {
        typed_at_any_pace(cx, "markdown", small_md(), 0..40);
    }

    /// A key typed into a text too large to parse at the key has its
    /// trees moved at the key, and its parse after the pause, as spans of
    /// the performance recorder's, of the whole text (elundqvist/kvist#589,
    /// #594).
    #[gpui::test]
    fn the_parse_after_the_pause_is_a_perf_span(cx: &mut gpui::TestAppContext) {
        let (closed, _watching) = super::super::highlighter::tests::watch_perf_spans();
        cx.update(crate::init);
        let (editor, cx) = cx.add_window_view(|window, cx| EditorState::new(window, cx));
        let mut text = big_rust();
        let mut h = built("rust");
        let rope = Rope::from(text.as_str());
        cx.update(|window, cx| editor.update(cx, |_, cx| h.update(None, &rope, true, window, cx)));
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        closed.lock().unwrap().clear();
        let at = text.find("value * 2;").unwrap() + "value * 2;".len();
        let edit = typed(&mut text, at, ' ');
        let rope = Rope::from(text.as_str());
        cx.update(|window, cx| {
            editor.update(cx, |_, cx| h.update(Some(edit), &rope, true, window, cx))
        });
        let moved = format!("move language=rust bytes={}", text.len());
        assert_eq!(
            *closed.lock().unwrap(),
            [moved.as_str()],
            "the key moves the trees and parses nothing"
        );
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(
            *closed.lock().unwrap(),
            [moved, format!("parse language=rust bytes={}", text.len())]
        );
    }
}
