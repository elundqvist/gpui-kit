#[cfg(test)]
use crate::highlighter::HighlightTheme;
use crate::highlighter::LanguageConfig;
use crate::highlighter::LanguageRegistry;
use crate::highlighter::queries;

use anyhow::{Context, Result, anyhow};
use gpui::{HighlightStyle, SharedString};
use gpui_base::input::RopeExt as _;
use ropey::{ChunkCursor, Rope};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use std::{
    collections::{BTreeSet, HashMap},
    ops::{ControlFlow, Range},
    usize,
};
use sum_tree::Bias;
use tree_sitter::{
    InputEdit, ParseOptions, Parser, Point, Query, QueryCursor, StreamingIterator, Tree,
};

/// When a node spans more than this many bytes beyond the requested query
/// range, we recurse into its children instead of querying it directly.
const LARGE_NODE_THRESHOLD: usize = 8 * 1024;
const MAX_INJECTION_RANGES: usize = 4096;
const MAX_INJECTION_BYTES: usize = 512 * 1024;
const MAX_INJECTION_LANGUAGE_BYTES: usize = 64;
/// Parse attempts, not resulting layers: a failed parse still spends budget.
/// Matches past it keep host highlighting but get no injected tokens.
const MAX_NON_COMBINED_INJECTION_PARSES: usize = 512;
const INJECTION_PARSE_TIMEOUT: Duration = Duration::from_millis(20);
/// The most a text made anew may differ from the old between what the
/// two share at their start and their end for the old trees to be moved
/// over it as an edit (`SyntaxHighlighter::keep_colours_through`): an edit
/// of the trees costs what it reaches, 33 ms for all of 1.4 MB.
const KEEP_ANEW_EDIT_BYTES: usize = 64 * 1024;

/// Whether a tracing subscriber takes trace-level spans. A highlighter's
/// parse, its trees moved with an edit and not parsed, and its highlight
/// query are spans of target `gpui::perf` then, as gpui's frames are, for
/// an app that times its drawing thread (elundqvist/kvist#589, #594):
/// with no subscriber, or one that takes less, each costs this one
/// relaxed load and allocates nothing.
#[inline]
pub(crate) fn perf_spans() -> bool {
    tracing::level_filters::LevelFilter::current() == tracing::level_filters::LevelFilter::TRACE
}

/// The span of the parse that runs after the typing pauses, off the
/// drawing thread. A key typed meanwhile cancels the parse and lets its
/// tree go: the span says so as it closes (`outcome` is `cancelled`), so
/// that a parse cut short is not counted with the ones that ran to their
/// end (elundqvist/kvist#589).
pub(crate) struct PauseParse(Option<(tracing::span::EnteredSpan, Arc<AtomicBool>)>);

impl PauseParse {
    /// The parse of `bytes` of `language` begins, `cancel` the flag a key
    /// typed meanwhile sets.
    pub(crate) fn enter(language: &str, bytes: usize, cancel: &Arc<AtomicBool>) -> Self {
        Self(perf_spans().then(|| {
            let span = tracing::trace_span!(target: "gpui::perf", "parse", language, bytes = bytes as u64, outcome = tracing::field::Empty);
            (span.entered(), cancel.clone())
        }))
    }
}

impl Drop for PauseParse {
    fn drop(&mut self) {
        if let Some((span, cancel)) = &self.0 {
            if cancel.load(Ordering::Relaxed) {
                span.record("outcome", "cancelled");
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    /// How many injection layers this thread has parsed, for the tests.
    static LAYER_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many layers' trees this thread has moved with an edit.
    static LAYER_TREE_EDITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A syntax highlighter that supports incremental parsing, multiline text,
/// and caching of highlight results.
#[allow(unused)]
pub struct SyntaxHighlighter {
    language: SharedString,
    query: Option<Arc<Query>>,
    /// The full injections query. This is used to build injection layers during parsing.
    injections_query: Option<Arc<Query>>,
    /// The kind of node each pattern of the injections query starts at
    /// (`pattern_root_kinds`).
    injection_roots: Arc<[Option<SharedString>]>,

    locals_pattern_index: usize,
    highlights_pattern_index: usize,
    // highlight_indices: Vec<Option<Highlight>>,
    non_local_variable_patterns: Arc<[bool]>,
    injection_content_capture_index: Option<u32>,
    injection_language_capture_index: Option<u32>,
    local_scope_capture_index: Option<u32>,
    local_def_capture_index: Option<u32>,
    local_def_value_capture_index: Option<u32>,
    local_ref_capture_index: Option<u32>,

    /// The last parsed source text.
    text: Rope,
    parser: Parser,
    /// The last parsed tree.
    tree: Option<Tree>,

    /// Parsed injection trees.
    /// These are built once in update() and queried multiple times in match_styles().
    /// Shared with a parse off the UI thread, which makes its own of them
    /// there (`InjectionParseSource`): copied here, the 3,000 of a
    /// CHANGELOG took 0.2 to 0.6 ms of every key (elundqvist/kvist#376).
    injection_layers: Arc<Vec<InjectionLayer>>,
    /// The layers are every span the last query over the whole text found,
    /// each parsed to its end, and moved with the edits since: an edit
    /// that reaches none of them leaves them as they are.
    layers_whole: bool,
    /// How long one injected span may parse in update().
    injection_parse_timeout: Duration,
    /// An update with a time limit leaves a span whose language's highlight
    /// query no one has compiled yet to the parse after the pause, and has
    /// a thread compile it (`queries`): compiled there, a Rust fence took
    /// 12 ms of the drawing thread and a Kotlin one 22, the open of a
    /// Markdown file with fences 18 to 43 and the first key into a new
    /// Rust fence 12 to 15 (elundqvist/kvist#593). Set by the editor's
    /// adapter; a caller that has no parse after the pause compiles there.
    defer_cold_queries: bool,
    /// `tree` was edited to the text but not parsed again, as it is while
    /// a large file waits for the background parse or a parse ran out of
    /// time: what the edits touched in it says nothing true about what is
    /// there now, and is left plain until a parse finishes.
    stale: bool,
    /// `tree` is a tree a parse made from nothing that the next parse would
    /// not reuse whole (`parsed_again_from_itself`), left to the parse after
    /// the pause, which parses it again from itself, edited at its start
    /// (`edited_at_its_start`), as the one that finds none does, and does
    /// not do it on the drawing thread: it rebuilds every node of the text
    /// (elundqvist/kvist#593). Cleared by any parse of a tree made from an
    /// edit, which is one the next parse does reuse.
    again_pending: bool,
    /// Where the edits since the last finished parse lie in the text. A
    /// node an edit lands in has changes, but so has the first node after
    /// an edit in the space before it, which the edit did not touch: only
    /// a node one of these reaches is left plain.
    edited: Vec<Range<usize>>,
    /// `tree` and the layers are of a text that was made anew since, a
    /// paste over all of it (`keep_colours_through`): they colour the new
    /// text as they coloured the old, where nothing the two do not share
    /// reaches, until a parse of the new text, which starts afresh rather
    /// than from them, takes their place.
    anew: bool,
    /// Where the trees kept through a text made anew stop colouring it:
    /// the end of what the two texts share at their start, where the
    /// trees were not moved over what differs (`keep_colours_through`).
    /// Past it a node of the old text says nothing of what is there now.
    coloured_to: Option<usize>,
}

/// The trees a highlighter's parse left, taken out of it to be freed
/// elsewhere ([`SyntaxHighlighter::take_parse`]).
pub(crate) struct Parse {
    tree: Option<Tree>,
    injection_layers: Arc<Vec<InjectionLayer>>,
}

impl Parse {
    /// Whether it holds any tree at all.
    pub(crate) fn is_empty(&self) -> bool {
        self.tree.is_none() && self.injection_layers.is_empty()
    }
}

/// A parsed injection layer.
/// Stores the parsed tree and the ranges it covers.
#[derive(Clone)]
pub(crate) struct InjectionLayer {
    pub(crate) language_name: SharedString,
    highlight_query: Arc<Query>,
    pub(crate) ranges: Vec<tree_sitter::Range>,
    pub(crate) byte_range: Range<usize>,
    pub(crate) tree: Tree,
    /// The span's parse ran out of time, and `tree` is its old tree, edited
    /// to the text but not parsed again: what the edits touched is left
    /// plain until a parse of the span finishes.
    stale: bool,
    /// Where the host's node the injection pattern matched lies
    /// (`match_range`), moved with the edits as `ranges` are: an edit or a
    /// change of the host's structure inside it may make the match or
    /// unmake it, and one outside it cannot. `None` where the pattern's
    /// node could not be told.
    pub(crate) match_range: Option<tree_sitter::Range>,
    /// Where the tree has the first of `ranges` start, where the edits
    /// since it was parsed or last moved all lay before the layer, and
    /// moved its ranges but not the tree (`edit_injection_layers`).
    moved_from: Option<(usize, Point)>,
}

impl InjectionLayer {
    /// How far the layer's text lies past where its tree has it: what the
    /// edits before it since the tree last moved put in, less what they
    /// took out (`moved_from`).
    fn shift(&self) -> isize {
        match (self.moved_from, self.ranges.first()) {
            (Some((from, _)), Some(first)) => first.start_byte as isize - from as isize,
            _ => 0,
        }
    }

    /// Move the tree to where the layer's text is now, by the one edit
    /// that every edit before it since comes to (`moved_from`).
    fn catch_up(&mut self) {
        catch_up(&mut self.tree, self.moved_from.take(), &self.ranges);
    }
}

/// Move `tree`, of `ranges`, from where it has the first of them start,
/// `from`, to where that is now: the edits before a layer's text, put
/// together, are one edit of what lies before it. tree-sitter moves a tree
/// by such an edit as by each of them in turn, a node's columns included,
/// as it moves `ranges` (`InputEdit::edit_range`).
fn catch_up(tree: &mut Tree, from: Option<(usize, Point)>, ranges: &[tree_sitter::Range]) {
    if let (Some((byte, point)), Some(first)) = (from, ranges.first()) {
        tree.edit(&InputEdit {
            start_byte: 0,
            old_end_byte: byte,
            new_end_byte: first.start_byte,
            start_position: Point::new(0, 0),
            old_end_position: point,
            new_end_position: first.start_point,
        });
    }
}

/// Data needed to compute injection layers on a background thread.
pub(crate) struct InjectionParseData {
    pub(crate) query: Arc<Query>,
    pub(crate) content_capture_index: Option<u32>,
    pub(crate) language_capture_index: Option<u32>,
    /// The previous layers, edited to the current text: a span's old tree is
    /// reused when its ranges match the span's new ones, or, for a span that
    /// is not combined, differ only in where the first one starts.
    pub(crate) old_layers: Vec<ReusableInjectionLayer>,
    /// How long one layer may parse before it keeps its old tree, or `None`
    /// for as long as it takes.
    pub(crate) parse_timeout: Option<Duration>,
    /// Stops every parse still running once it is set.
    pub(crate) cancel: Option<Arc<AtomicBool>>,
    /// The kind of node each pattern of `query` starts at.
    pub(crate) roots: Arc<[Option<SharedString>]>,
    /// What the edits since the old layers were made reach in the host's
    /// new tree, where `old_layers` are every span found then, each
    /// parsed to its end: only this is queried again, and an old layer
    /// whose match lies outside it is kept as it is. `None` queries the
    /// whole text.
    pub(crate) reach: Option<Range<usize>>,
    /// The spans of a paragraph language (`injection_spans_are_paragraphs`)
    /// have `parse_timeout` between them, not each: they are made on the UI
    /// thread, by an update with a time limit.
    pub(crate) paragraphs_share_timeout: bool,
    /// A span whose language's highlight query is not compiled yet is not
    /// parsed: the parse is not finished, and a thread compiles the query,
    /// for the parse after the pause (`SyntaxHighlighter::defer_cold_queries`).
    pub(crate) defer_cold_queries: bool,
}

pub(crate) struct ReusableInjectionLayer {
    pub(crate) language_name: SharedString,
    highlight_query: Arc<Query>,
    pub(crate) ranges: Vec<tree_sitter::Range>,
    pub(crate) tree: Tree,
    pub(crate) match_range: Option<tree_sitter::Range>,
    /// No edit has reached the layer's text since its tree was parsed to
    /// its end: the text it was parsed from is the text there now.
    intact: bool,
    /// `InjectionLayer::moved_from`: the tree is moved where it is reused,
    /// and a layer kept as it is keeps it.
    moved_from: Option<(usize, Point)>,
}

impl ReusableInjectionLayer {
    /// `layer`, intact where none of `edited`, the edits since the last
    /// finished parse, reaches it, its edges included.
    fn of(layer: InjectionLayer, edited: &[Range<usize>]) -> Self {
        let intact = !edited
            .iter()
            .any(|r| r.start <= layer.byte_range.end && layer.byte_range.start <= r.end);
        Self {
            language_name: layer.language_name,
            highlight_query: layer.highlight_query,
            ranges: layer.ranges,
            tree: layer.tree,
            match_range: layer.match_range,
            intact,
            moved_from: layer.moved_from,
        }
    }

    fn catch_up(&mut self) {
        catch_up(&mut self.tree, self.moved_from.take(), &self.ranges);
    }
}

/// What a parse of the injections off the UI thread needs of the
/// highlighter, taken there for next to nothing: the layers are shared,
/// not copied, and made the parse's own on its thread (`into_data`).
/// Copied on the UI thread, with every tree, the 3,000 of a CHANGELOG took
/// 0.2 to 0.6 ms of every key typed into it (elundqvist/kvist#376).
pub(crate) struct InjectionParseSource {
    query: Arc<Query>,
    content_capture_index: Option<u32>,
    language_capture_index: Option<u32>,
    layers: Arc<Vec<InjectionLayer>>,
    /// The edits since the last finished parse.
    edited: Vec<Range<usize>>,
    parse_timeout: Duration,
    roots: Arc<[Option<SharedString>]>,
}

impl InjectionParseSource {
    /// The data a parse of the whole text's injections takes, the old
    /// layers its own.
    pub(crate) fn into_data(self) -> InjectionParseData {
        let old_layers = self
            .layers
            .iter()
            .map(|layer| ReusableInjectionLayer::of(layer.clone(), &self.edited))
            .collect();
        InjectionParseData {
            query: self.query,
            content_capture_index: self.content_capture_index,
            language_capture_index: self.language_capture_index,
            old_layers,
            parse_timeout: Some(self.parse_timeout),
            cancel: None,
            roots: self.roots,
            reach: None,
            paragraphs_share_timeout: false,
            defer_cold_queries: false,
        }
    }
}

/// The text a query's predicates (`#match?`, `#eq?`) test a node against,
/// for a tree that has it `shift` bytes back (`InjectionLayer::shift`):
/// a layer's tree the edits before it have not moved yet. Read where the
/// tree has the node, it was as many bytes off as the edits put in or took
/// out, and past the text's end where they took out more than lies after
/// the node, which panicked (elundqvist/kvist#376).
struct TextProvider<'a>(&'a Rope, isize);
struct ByteChunks<'a> {
    cursor: ChunkCursor<'a>,
    node_start: usize,
    node_end: usize,
    at_first: bool,
}
impl<'a> tree_sitter::TextProvider<&'a [u8]> for TextProvider<'a> {
    type I = ByteChunks<'a>;

    fn text(&mut self, node: tree_sitter::Node) -> Self::I {
        let TextProvider(text, shift) = *self;
        let at = |byte: usize| byte.saturating_add_signed(shift).min(text.len());
        let range = at(node.start_byte())..at(node.end_byte());
        let cursor = text.chunk_cursor_at(range.start);

        ByteChunks {
            cursor,
            node_start: range.start,
            node_end: range.end,
            at_first: true,
        }
    }
}

impl<'a> Iterator for ByteChunks<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if !self.at_first {
            if !self.cursor.next() {
                return None;
            }
        }
        self.at_first = false;

        let chunk_byte_start = self.cursor.byte_offset();
        if chunk_byte_start >= self.node_end {
            return None;
        }

        let chunk = self.cursor.chunk().as_bytes();

        // Slice the chunk to only include bytes within the node's range.
        let start_in_chunk = self.node_start.saturating_sub(chunk_byte_start);
        let end_in_chunk = (self.node_end - chunk_byte_start).min(chunk.len());

        if start_in_chunk >= end_in_chunk {
            return None;
        }

        Some(&chunk[start_in_chunk..end_in_chunk])
    }
}

fn injection_range_len(range: &tree_sitter::Range) -> usize {
    range.end_byte.saturating_sub(range.start_byte)
}

fn injection_ranges_byte_count(ranges: &[tree_sitter::Range]) -> usize {
    ranges.iter().map(injection_range_len).sum()
}

fn bounding_byte_range(ranges: &[tree_sitter::Range]) -> Option<Range<usize>> {
    let start = ranges.iter().map(|r| r.start_byte).min()?;
    let end = ranges.iter().map(|r| r.end_byte).max()?;
    Some(start..end)
}

fn injection_ranges_within_limits(ranges: &[tree_sitter::Range]) -> bool {
    ranges.len() <= MAX_INJECTION_RANGES
        && injection_ranges_byte_count(ranges) <= MAX_INJECTION_BYTES
}

/// Read a captured injection language without ever allocating an unbounded
/// amount of source text. Language identifiers in fenced code blocks are tiny;
/// longer captures cannot name a registered language and are ignored.
fn captured_injection_language(text: &Rope, range: Range<usize>) -> Option<SharedString> {
    if range.end > text.len()
        || range.start >= range.end
        || range.end.saturating_sub(range.start) > MAX_INJECTION_LANGUAGE_BYTES
    {
        return None;
    }

    let language = text.slice(range).to_string();
    let language = language.trim();
    (!language.is_empty()).then(|| SharedString::from(language.to_string()))
}

/// Combined markdown inline injections are parsed as one tree with
/// `set_included_ranges`. If we include only the inline nodes that contain
/// trigger bytes, the parser sees those ranges as adjacent and can merge a
/// closing backtick from one list item with an opening backtick from the next.
/// Re-inserting the separator bytes between retained inline ranges preserves
/// the original boundaries.
///
/// The separator bytes count against the same `MAX_INJECTION_RANGES` /
/// `MAX_INJECTION_BYTES` budget as the content ranges, so on a very large
/// document the tail ranges may be dropped here even though `push_limited`
/// already admitted them.
fn normalize_combined_injection_ranges(
    language_name: &SharedString,
    ranges: Vec<tree_sitter::Range>,
) -> Vec<tree_sitter::Range> {
    if language_name.as_ref() != "markdown_inline" || ranges.len() <= 1 {
        return ranges;
    }

    let mut normalized = Vec::with_capacity(ranges.len().min(MAX_INJECTION_RANGES));
    let mut byte_count = 0usize;
    let mut previous_range: Option<tree_sitter::Range> = None;

    for range in ranges {
        let mut pending_ranges = Vec::with_capacity(2);
        if let Some(previous) = previous_range {
            if previous.end_byte < range.start_byte {
                pending_ranges.push(tree_sitter::Range {
                    start_byte: previous.end_byte,
                    end_byte: range.start_byte,
                    start_point: previous.end_point,
                    end_point: range.start_point,
                });
            }
        }
        pending_ranges.push(range);

        let pending_len = pending_ranges
            .iter()
            .map(injection_range_len)
            .sum::<usize>();
        if normalized.len().saturating_add(pending_ranges.len()) > MAX_INJECTION_RANGES
            || byte_count.saturating_add(pending_len) > MAX_INJECTION_BYTES
        {
            break;
        }

        byte_count += pending_len;
        normalized.extend(pending_ranges);
        previous_range = Some(range);
    }

    normalized
}

/// The kind of node each pattern of an injection query starts at, read
/// from the query's source: `(fenced_code_block …)` starts at a fence and
/// `((html_block) @injection.content …)` at an HTML block. A pattern that
/// starts at a wildcard, an alternation or a string has none, and so has
/// one that is not one node with the rest within it (tree-sitter's
/// `is_pattern_rooted`): `((comment) @injection.content . (function))`
/// is two nodes side by side, and the match hangs on the second, outside
/// the first, which holds every capture.
pub(super) fn pattern_root_kinds(query: &Query, source: &str) -> Vec<Option<SharedString>> {
    (0..query.pattern_count())
        .map(|i| {
            if !query.is_pattern_rooted(i) {
                return None;
            }
            let pattern = source.get(query.start_byte_for_pattern(i)..)?;
            let name = pattern.trim_start_matches(|c: char| c == '(' || c.is_whitespace());
            if !pattern[..pattern.len() - name.len()].contains('(') {
                return None;
            }
            let kind = &name[..name
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(name.len())];
            (!kind.is_empty() && kind != "_").then(|| SharedString::from(kind.to_string()))
        })
        .collect()
}

/// Where the node a match of an injection pattern starts at lies, or a
/// node around it: the outermost node of the pattern's root kind on the way
/// down from the root to the smallest node that holds every capture. The
/// node the pattern matched is one of those, and the outermost holds it.
/// Nothing outside it can make the match or unmake it but a change of the
/// tree's structure, which the tree's changed ranges say.
fn match_range(
    root: tree_sitter::Node,
    captures: &[tree_sitter::QueryCapture],
    root_kind: Option<&str>,
) -> Option<tree_sitter::Range> {
    let root_kind = root_kind?;
    let first = captures.first()?.node;
    let (start, end) = captures.iter().fold((usize::MAX, 0), |(start, end), capture| {
        (start.min(capture.node.start_byte()), end.max(capture.node.end_byte()))
    });
    let mut node = root;
    loop {
        if node.kind() == root_kind {
            return Some(node.range());
        }
        if node.id() == first.id() {
            return None;
        }
        match node.child_with_descendant(first) {
            Some(child) if child.start_byte() <= start && end <= child.end_byte() => node = child,
            _ => return None,
        }
    }
}

/// Whether a tree of `grammar` that a parse made from nothing is parsed
/// again once, from itself edited at its start (`edited_at_its_start`)
/// and the same text, before an edit is parsed from it: tree-sitter-md's
/// block grammar (0.5.3) leaves a tree made from nothing whose nodes the
/// parse after an edit cannot reuse whole, and breaks down to their
/// leaves and builds again, from the edit to the end of the text. The
/// first key after a README opened did so on the drawing thread: 34 to
/// 40 ms of a 190 KB README that begins with front matter, where the keys
/// after it take 0.2 to 0.7 (elundqvist/kvist#376), and in one without,
/// 213 of the parser's steps for a key in a 40 KB file where the keys
/// after it take 12: 1.9 ms, or all of the 2 ms a key has, against 0.13
/// (elundqvist/kvist#593). The parse again builds those nodes as a
/// parse after an edit builds them, and the tree is one the next key's
/// parse reuses; it takes a quarter to three quarters of what the first
/// parse took, and a call with a time limit leaves it to the parse after
/// the pause, off the drawing thread
/// (`SyntaxHighlighter::again_pending`). Other grammars reuse a tree made
/// from nothing as they reuse any: Rust's first key costs what its
/// second does, and so do those of HTML, JavaScript, Python, YAML and the
/// rest but Java and PHP, whose first key in 30 KB takes 73 and 112 steps
/// where it takes 20 and 71 after the parse again, too little to parse
/// them again for.
pub(crate) fn parsed_again_from_itself(grammar: &tree_sitter::Language) -> bool {
    is_markdown_blocks(grammar)
}

/// `tree`, made from nothing, edited by nothing at its start, for the
/// parse again from itself to start from (`parsed_again_from_itself`).
/// Unedited, that parse reused the tree whole and left it as it was, but
/// for a text that begins with front matter, whose first node is never
/// reused: every node after it was built again, by chance, as the edit
/// at the start has every one built again now.
pub(crate) fn edited_at_its_start(tree: &Tree) -> Tree {
    let mut tree = tree.clone();
    let start = Point::new(0, 0);
    tree.edit(&InputEdit {
        start_byte: 0,
        old_end_byte: 0,
        new_end_byte: 0,
        start_position: start,
        old_end_position: start,
        new_end_position: start,
    });
    tree
}

/// Whether `grammar` is tree-sitter-md's block grammar, told by its own
/// name, whatever the language is registered as (Kvist's is
/// `kvist-markdown`).
fn is_markdown_blocks(grammar: &tree_sitter::Language) -> bool {
    grammar.name() == Some("markdown")
}

/// Whether a language's spans are parsed from scratch on every change,
/// never from their old tree.
///
/// tree-sitter-markdown's inline grammar (tree-sitter-md 0.5.3) is not sound
/// under incremental parsing: parsed again from its edited old tree, it can
/// give another tree than a fresh parse of the same text, and the difference
/// is always an emphasis or strikethrough delimiter run (`*`, `_`, `~`) read
/// as opening or closing otherwise than a fresh parse reads it. A random edit
/// of a short text, with no included ranges, differed in 3% of 20,000 tries;
/// texts with no delimiter runs never differed in 20,000. The paragraph the
/// edit lands in does not bound it: an edit in one paragraph changed the
/// emphasis of another. A fresh inline parse costs about what the parse from
/// the old tree does here (198 KB of prose: 180 to 200 ms fresh, 160 ms from
/// the old tree), so this layer gains nothing by reuse but the wrong colour.
/// Its old tree is still what a span keeps when a parse runs out of time.
fn injection_parses_from_scratch(language_name: &str) -> bool {
    language_name == "markdown_inline"
}

/// Whether a language's spans, where its pattern is not combined, are a
/// document's paragraphs, a layer each: Markdown's inline syntax as
/// tree-sitter-md's own query injects it, one layer to every paragraph,
/// heading and table cell, so that a backtick or a `*` left open in one
/// paragraph pairs with nothing in the next (elundqvist/kvist#54). A README
/// has hundreds of them and a CHANGELOG thousands, each a few hundred bytes
/// that parse in a fraction of a millisecond: the 373 of a README of 190 KB
/// in 60 ms, the longest in 0.3.
///
/// - They are not counted against `MAX_NON_COMBINED_INJECTION_PARSES`, which
///   bounds spans that may each take their 20 ms: counted, a README's
///   paragraphs used it up, the fences after them went plain, and every
///   keystroke looked for injections over the whole text.
/// - On an update with a time limit they share one span's time limit
///   between them (`InjectionParseData::paragraphs_share_timeout`): a
///   keystroke that turns a fence back into prose would otherwise parse all
///   the prose after it on the UI thread. What is left over keeps its old
///   tree, stale, until the background parse.
/// - One whose ranges are as they were and whose text no edit has reached
///   since its parse keeps its tree and is not parsed again: parsed from
///   scratch as they are (`injection_parses_from_scratch`), the whole query
///   of the background parse parsed them all.
fn injection_spans_are_paragraphs(language_name: &str) -> bool {
    language_name == "markdown_inline"
}

fn should_include_injection_range(
    language_name: &SharedString,
    range: &tree_sitter::Range,
    text: &Rope,
) -> bool {
    if language_name.as_ref() != "markdown_inline" {
        return true;
    }

    markdown_inline_range_has_trigger(text, range.start_byte..range.end_byte)
}

/// Returns whether an inline range contains any byte that could start a
/// Markdown inline construct, so plain prose ranges skip the injected parse.
///
/// The byte set must stay a superset of the trigger characters for every node
/// captured by `languages/markdown_inline/highlights.scm` (emphasis, code
/// spans, links, images, autolinks). If that query gains a construct with a new
/// trigger character (e.g. GFM bare autolinks), add it here or the construct
/// will silently lose highlighting.
fn markdown_inline_range_has_trigger(text: &Rope, range: Range<usize>) -> bool {
    text.slice(range).bytes().any(|byte| {
        matches!(
            byte,
            b'*' | b'_' | b'`' | b'[' | b']' | b'(' | b')' | b'<' | b'>' | b'!' | b'~' | b'$'
        )
    })
}

#[derive(Debug, Default, Clone)]
struct HighlightSummary {
    count: usize,
    start: usize,
    end: usize,
    min_start: usize,
    max_end: usize,
}

/// The highlight item, the range is offset of the token in the tree.
#[derive(Debug, Default, Clone)]
struct HighlightItem {
    /// The byte range of the highlight in the text.
    range: Range<usize>,
    /// The highlight name, like `function`, `string`, `comment`, etc.
    name: SharedString,
}

impl HighlightItem {
    pub fn new(range: Range<usize>, name: impl Into<SharedString>) -> Self {
        Self {
            range,
            name: name.into(),
        }
    }
}

impl sum_tree::Item for HighlightItem {
    type Summary = HighlightSummary;
    fn summary(&self, _cx: &()) -> Self::Summary {
        HighlightSummary {
            count: 1,
            start: self.range.start,
            end: self.range.end,
            min_start: self.range.start,
            max_end: self.range.end,
        }
    }
}

impl sum_tree::Summary for HighlightSummary {
    type Context<'a> = &'a ();
    fn zero(_: Self::Context<'_>) -> Self {
        HighlightSummary {
            count: 0,
            start: usize::MIN,
            end: usize::MAX,
            min_start: usize::MAX,
            max_end: usize::MIN,
        }
    }

    fn add_summary(&mut self, other: &Self, _: Self::Context<'_>) {
        self.min_start = self.min_start.min(other.min_start);
        self.max_end = self.max_end.max(other.max_end);
        self.start = other.start;
        self.end = other.end;
        self.count += other.count;
    }
}

impl<'a> sum_tree::Dimension<'a, HighlightSummary> for usize {
    fn zero(_: &()) -> Self {
        0
    }

    fn add_summary(&mut self, _: &'a HighlightSummary, _: &()) {}
}

impl<'a> sum_tree::Dimension<'a, HighlightSummary> for Range<usize> {
    fn zero(_: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a HighlightSummary, _: &()) {
        self.start = summary.start;
        self.end = summary.end;
    }
}

impl SyntaxHighlighter {
    /// Create a new SyntaxHighlighter for the given language.
    pub fn new(lang: &str) -> Self {
        match Self::build_for_language(&lang) {
            Ok(result) => result,
            Err(err) => {
                tracing::warn!(
                    "SyntaxHighlighter init failed, fallback to use `text`, {}",
                    err
                );
                Self::build_for_language("text").unwrap()
            }
        }
    }

    /// `new`, where it compiles nothing: `None` where `lang` has a grammar
    /// whose queries no one has compiled yet (`queries::host`), which
    /// `compile_queries` compiles elsewhere. The editor's highlighter is
    /// made so, as a file opens, and stands as an `unbuilt` one until a
    /// thread has compiled them: compiled where it was made, they took the
    /// drawing thread 4 ms (Markdown), 13 (Rust) and 39 (TypeScript) as
    /// the first file of a language opened (elundqvist/kvist#597).
    pub(crate) fn if_compiled(lang: &str) -> Option<Self> {
        let cold = LanguageRegistry::singleton()
            .language(lang)
            .is_some_and(|config| config.has_grammar() && !queries::host_compiled(&config));
        (!cold).then(|| Self::new(lang))
    }

    /// Send a task of `executor` to compile the queries a highlighter of
    /// `lang` takes, where no one has and none is at it, for `new` to find:
    /// what `if_compiled` leaves to a thread. The future is ready once they
    /// are compiled; `None` for a language without a grammar.
    pub(super) fn compile_queries(
        lang: &str,
        executor: &gpui::BackgroundExecutor,
    ) -> Option<queries::HostCompiled> {
        let config = LanguageRegistry::singleton().language(lang)?;
        config
            .has_grammar()
            .then(|| queries::compiling_host(&config, executor))
    }

    /// A highlighter of `lang` that holds the text and neither parses nor
    /// colours it, for one whose queries are not compiled yet
    /// (`if_compiled`).
    pub(crate) fn unbuilt(lang: &str) -> Self {
        let language = LanguageRegistry::singleton()
            .language(lang)
            .map_or_else(|| SharedString::from(lang.to_string()), |config| config.name.clone());
        Self::build_inert(language)
    }

    /// Build an inert highlighter that never parses and creates no styles,
    /// for languages without a grammar.
    fn build_inert(language: SharedString) -> Self {
        Self {
            language,
            query: None,
            injections_query: None,
            injection_roots: Arc::from([]),
            locals_pattern_index: 0,
            highlights_pattern_index: 0,
            non_local_variable_patterns: Arc::from([]),
            injection_content_capture_index: None,
            injection_language_capture_index: None,
            local_scope_capture_index: None,
            local_def_capture_index: None,
            local_def_value_capture_index: None,
            local_ref_capture_index: None,
            text: Rope::new(),
            parser: Parser::new(),
            tree: None,
            injection_layers: Arc::default(),
            layers_whole: false,
            injection_parse_timeout: INJECTION_PARSE_TIMEOUT,
            defer_cold_queries: false,
            stale: false,
            again_pending: false,
            edited: Vec::new(),
            anew: false,
            coloured_to: None,
        }
    }

    /// Build the highlighter for the given language, from the queries the
    /// process keeps for it (`queries`), compiled where it is the first.
    fn build_for_language(lang: &str) -> Result<Self> {
        let Some(config) = LanguageRegistry::singleton().language(&lang) else {
            return Err(anyhow!(
                "language {:?} is not registered in `LanguageRegistry`",
                lang
            ));
        };

        // Languages without grammar default to a highlighter that never
        // parses and creates no styles.
        let Some(grammar) = config.language.as_ref() else {
            return Ok(Self::build_inert(config.name.clone()));
        };

        let mut parser = Parser::new();
        parser.set_language(grammar).context("parse set_language")?;

        let queries = queries::host(&config)?;
        Ok(Self {
            language: config.name.clone(),
            query: Some(queries.query.clone()),
            injections_query: queries.injections_query.clone(),
            injection_roots: queries.injection_roots.clone(),

            locals_pattern_index: queries.locals_pattern_index,
            highlights_pattern_index: queries.highlights_pattern_index,
            non_local_variable_patterns: queries.non_local_variable_patterns.clone(),
            injection_content_capture_index: queries.injection_content_capture_index,
            injection_language_capture_index: queries.injection_language_capture_index,
            local_scope_capture_index: queries.local_scope_capture_index,
            local_def_capture_index: queries.local_def_capture_index,
            local_def_value_capture_index: queries.local_def_value_capture_index,
            local_ref_capture_index: queries.local_ref_capture_index,
            text: Rope::new(),
            parser,
            tree: None,
            injection_layers: Arc::default(),
            layers_whole: false,
            injection_parse_timeout: INJECTION_PARSE_TIMEOUT,
            defer_cold_queries: false,
            stale: false,
            again_pending: false,
            edited: Vec::new(),
            anew: false,
            coloured_to: None,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.text.len() == 0
    }

    /// Get the parsed tree (if available)
    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    /// The tree, where it is of the text, edited to it or parsed from it:
    /// none where the text was made anew since and the tree only keeps
    /// the old text's colours (`keep_colours_through`), whose nodes say
    /// nothing of the new text's folds, and which a parse must not start
    /// from.
    pub(crate) fn tree_of_text(&self) -> Option<&Tree> {
        self.tree.as_ref().filter(|_| !self.anew)
    }

    /// The spans parsed at the key given all the time they take, as the
    /// parse after the pause gives them, for a test that asserts what a
    /// key parsed and not how long it took (elundqvist/kvist#608).
    #[cfg(all(test, feature = "tree-sitter-languages"))]
    pub(crate) fn untimed_spans(&mut self) {
        self.injection_parse_timeout = Duration::MAX;
    }

    /// Whether `tree_of_text` is a tree a parse made from nothing that the
    /// next parse would not reuse whole, which the parse after the pause
    /// is to parse again from itself, edited at its start
    /// (`edited_at_its_start`) rather than from as it is.
    pub(crate) fn again_pending(&self) -> bool {
        self.again_pending && !self.anew
    }

    /// Whether the tree was moved with an edit and not parsed since, as a
    /// parse with a time limit that ran out of it leaves it.
    pub(crate) fn is_stale(&self) -> bool {
        self.stale
    }

    /// Leave a span whose language's highlight query no one has compiled
    /// to the parse after the pause, by an update with a time limit
    /// (`defer_cold_queries`).
    pub(crate) fn defer_cold_queries(&mut self) {
        self.defer_cold_queries = true;
    }

    /// Take `text`, made anew, a paste over all of the old, and keep the
    /// old text's trees to colour it until a parse of it lands, as VS Code
    /// and IntelliJ keep the old colours meanwhile: the text a paste put
    /// over a file too large to parse at the key stood plain until the
    /// parse after it, a few hundred milliseconds for 1.4 MB
    /// (elundqvist/kvist#401). Where the two texts differ only between what
    /// they share at their start and at their end, by no more than
    /// `KEEP_ANEW_EDIT_BYTES`, the trees are moved over that as an edit
    /// moves them, so what is alike keeps its colours where it is now and
    /// what went in between is plain, where an edit's nodes are. Else the
    /// trees stand as they are and colour only what the two share at their
    /// start, where they are of the text still, and the rest is plain
    /// (`coloured_to`): an edit of the trees costs what it reaches, and
    /// over the whole of a text it took 33 ms of the paste's 35 to 62
    /// (elundqvist/kvist#384), and the old colours where they stood, over
    /// a text shifted by many small changes, gave its plain words the
    /// colours of keywords. VS Code and IntelliJ never colour text with
    /// tokens found for other text.
    pub(crate) fn keep_colours_through(&mut self, text: &Rope) {
        if self.tree.is_none() && self.injection_layers.is_empty() {
            self.text = text.clone();
            return;
        }
        let _move = self.move_span(text);
        let old = &self.text;
        let (old_len, new_len) = (old.len(), text.len());
        let head = common_prefix(old, text);
        let tail = common_suffix(old, text, old_len.min(new_len) - head);
        if old_len - tail - head <= KEEP_ANEW_EDIT_BYTES {
            let point = |text: &Rope, at: usize| {
                let p = text.offset_to_point(at);
                Point::new(p.row, p.column)
            };
            let edit = InputEdit {
                start_byte: head,
                old_end_byte: old_len - tail,
                new_end_byte: new_len - tail,
                start_position: point(text, head),
                old_end_position: point(old, old_len - tail),
                new_end_position: point(text, new_len - tail),
            };
            self.edited.clear();
            self.move_trees(&[], &edit);
        } else {
            self.edited.clear();
            self.coloured_to = Some(self.coloured_to.map_or(head, |end| end.min(head)));
        }
        self.anew = true;
        self.layers_whole = false;
        self.mark_stale();
        self.text = text.clone();
    }

    /// Apply only the structural `edit` to the existing tree and update the stored text,
    /// without re-parsing. The tree and every layer are then stale: what
    /// the edit touched in them is left plain until a parse finishes, since
    /// a token an edit lands in grows over all it inserts, and the rest
    /// keeps its colours, moved with the text.
    pub fn edit_tree(&mut self, edit: Option<InputEdit>, text: &Rope) {
        self.edit_tree_in_steps(&[], edit, text)
    }

    /// `edit_tree` for changes made one after another, `steps`, which
    /// `edit` covers (`move_trees`).
    pub(crate) fn edit_tree_in_steps(&mut self, steps: &[InputEdit], edit: Option<InputEdit>, text: &Rope) {
        let _move = self.move_span(text);
        match edit {
            Some(edit) => self.move_trees(steps, &edit),
            // the whole text is new, and the tree knows none of it: it is
            // plain until the parse, which starts afresh, rather than
            // coloured as the old text was where the old tree's nodes
            // happen to fall, and parsed again from a tree never told of
            // the change
            None => {
                self.tree = None;
                self.injection_layers = Arc::default();
                self.layers_whole = false;
                self.edited.clear();
                self.anew = false;
                self.again_pending = false;
                self.coloured_to = None;
            }
        }
        self.mark_stale();
        self.text = text.clone();
    }

    /// The span of the trees moved with an edit and not parsed, of the
    /// text's size, while a subscriber takes trace-level spans
    /// (`perf_spans`): what a key in a text too large to parse at the key,
    /// or a text made anew over all of one (a paste, a reload from disk, an
    /// undo or redo through an action), costs the drawing thread, where the
    /// parse that follows the pause has a span of its own. Of a text made
    /// anew it is the comparing of the two texts too, where they differ by
    /// more than `KEEP_ANEW_EDIT_BYTES` and nothing is moved
    /// (elundqvist/kvist#594).
    fn move_span(&self, text: &Rope) -> Option<tracing::span::EnteredSpan> {
        perf_spans().then(|| {
            tracing::trace_span!(target: "gpui::perf", "move", language = self.language.as_ref(), bytes = text.len() as u64)
                .entered()
        })
    }

    /// Move the tree and every layer with `edit`, not parsed again, and
    /// keep where it lies: what it touched is left plain until a parse.
    /// The end of what trees kept through a text made anew colour moves
    /// with it, and an edit that reaches past it ends it at the edit's
    /// start. Where `edit` covers changes made one after another, `steps`
    /// (`InputHighlighter::update_steps`), the tree is moved by each in
    /// turn, so that what lies between them keeps its place in it, and a
    /// fold one lies within is found again as long as it was: moved by the
    /// one edit, every node between the first change and the last was
    /// taken into it, and in a text too large to parse at the key a rename
    /// or a Replace All opened every fold a change lay within
    /// (elundqvist/kvist#404). The layers are moved by the one edit, and
    /// what it reaches of them looked at again, as before.
    fn move_trees(&mut self, steps: &[InputEdit], edit: &InputEdit) {
        if let Some(tree) = self.tree.as_mut() {
            edit_by_steps(tree, steps, edit);
        }
        self.edit_injection_layers(edit);
        self.note_edit(edit);
        if let Some(end) = self.coloured_to.as_mut() {
            if edit.old_end_byte <= *end {
                *end = *end - edit.old_end_byte + edit.new_end_byte;
            } else if edit.start_byte < *end {
                *end = edit.start_byte;
            }
        }
    }

    /// The tree and every layer were edited to the text and not parsed
    /// again.
    fn mark_stale(&mut self) {
        self.stale = true;
        if self.injection_layers.is_empty() {
            return;
        }
        for layer in Arc::make_mut(&mut self.injection_layers) {
            layer.stale = true;
        }
    }

    /// Keep where an edit lies, in the text as it is after it, and move
    /// the earlier ones with it, as the trees are moved.
    fn note_edit(&mut self, edit: &InputEdit) {
        let (start, old_end, new_end) = (edit.start_byte, edit.old_end_byte, edit.new_end_byte);
        let delta = new_end as isize - old_end as isize;
        let moved = |at: usize| (at as isize + delta).max(0) as usize;
        let mut points = Vec::new();
        for r in &mut self.edited {
            if r.end < start {
                continue;
            }
            if r.start > old_end {
                *r = moved(r.start)..moved(r.end);
                continue;
            }
            // a deletion is a point, and stays one: widened into a later
            // insertion at the same place it would lose its reach to the
            // nodes at its edges, and the `/*` deleted then a space typed
            // would serve the comment again. The node that stood at the
            // point stands at the insertion's end now, so the point is
            // kept there too
            if Range::is_empty(r) {
                points.push(new_end..new_end);
                *r = start..start;
                continue;
            }
            r.start = r.start.min(start);
            r.end = if r.end >= old_end {
                moved(r.end)
            } else {
                new_end
            }
            .max(new_end);
        }
        self.edited.extend(points);
        // one that lies within another already is that one's
        if !self
            .edited
            .iter()
            .any(|r| r.start <= start && new_end <= r.end && !r.is_empty())
        {
            self.edited.push(start..new_end);
        }
        // and one kept already is kept once: a point was kept again for
        // every point at its place, so the points doubled at each deletion
        // made before a parse, and a held Backspace or an undo of 28 keys
        // in a text too large to parse at each key kept 268 million of
        // them, seconds and gigabytes to make and to look through
        // (elundqvist/kvist#368)
        self.edited.sort_unstable_by_key(|r| (r.start, r.end));
        self.edited.dedup();
    }

    /// Whether an edit since the last finished parse reached the node: a
    /// stale node's colour is worth keeping only where nothing touched it.
    /// Reached means what was inserted lies within the node, which has
    /// grown over it, or something was deleted at or within its edges: a
    /// character typed before a word leaves the word as it was, and the
    /// word being typed is the one that goes plain; a deletion at a node's
    /// edge may have taken the `/*` that opened it, so the node beside a
    /// deletion goes plain too.
    fn touched(&self, node: &tree_sitter::Node) -> bool {
        self.touched_at(node, 0)
    }

    /// `touched`, for a node of a tree that has the text `shift` bytes
    /// before where it is now (`InjectionLayer::shift`).
    fn touched_at(&self, node: &tree_sitter::Node, shift: isize) -> bool {
        let at = |byte: usize| byte.saturating_add_signed(shift);
        let (start, end) = (at(node.start_byte()), at(node.end_byte()));
        node.has_changes()
            && self.edited.iter().any(|r| {
                if r.is_empty() {
                    start <= r.start && r.start <= end
                } else {
                    r.start < end && start < r.end
                }
            })
    }

    /// Whether an injected layer colours part of the range: a fence or a
    /// script the host reads as one node, whose tokens the layer reads
    /// one by one. A keystroke in it touches the host's node, which keeps
    /// its colour under the layer's, or every keystroke in a large fence
    /// would blank the fence; the layer's own touched tokens go plain.
    fn injected_within(&self, range: Range<usize>) -> bool {
        self.injection_layers
            .iter()
            .any(|l| l.byte_range.start < range.end && range.start < l.byte_range.end)
    }

    /// Apply `edit` to the injection layers: every layer's ranges move with
    /// the text, and the tree of a layer the edit reaches, its edges
    /// included, is edited as the main tree is. A layer is then found again
    /// at its new place and parsed again only as far as the edit reaches, and
    /// an edit that leaves a span's length as it was (an overtype) is parsed
    /// again rather than hidden by an old tree that never saw it.
    ///
    /// The tree of a layer after the edit is not moved until it is used for
    /// a parse (`InjectionLayer::catch_up`), and is read, its predicates'
    /// text too, where its text is now (`InjectionLayer::shift`); one before
    /// the edit is as it was. Every tree edited, a key in a CHANGELOG of 3,000 items, each
    /// a layer, took 1.4 to 2.6 ms of the drawing thread to move the trees,
    /// on screen, where the key is to take 4 in all (elundqvist/kvist#376).
    /// What is left is arithmetic on each layer's ranges, 0.04 ms for the
    /// 3,000.
    fn edit_injection_layers(&mut self, edit: &InputEdit) {
        if self.injection_layers.is_empty() {
            return;
        }
        for layer in Arc::make_mut(&mut self.injection_layers) {
            if edit.start_byte <= layer.byte_range.end && layer.byte_range.start <= edit.old_end_byte
            {
                layer.catch_up();
                layer.tree.edit(edit);
                #[cfg(test)]
                LAYER_TREE_EDITS.with(|edits| edits.set(edits.get() + 1));
            } else if edit.old_end_byte < layer.byte_range.start && layer.moved_from.is_none() {
                layer.moved_from = layer.ranges.first().map(|r| (r.start_byte, r.start_point));
            }
            for range in &mut layer.ranges {
                edit.edit_range(range);
            }
            if let Some(range) = layer.match_range.as_mut() {
                edit.edit_range(range);
            }
            if let Some(byte_range) = bounding_byte_range(&layer.ranges) {
                layer.byte_range = byte_range;
            }
        }
    }

    /// Whether `edit`, of the text there now to `text`, is one that lies
    /// in a line of a fenced code block in Markdown, after the line's first
    /// character, which is not one a fence is made of, and puts in or takes
    /// out no line break: one that can neither close the fence nor end
    /// what holds it, a list item or a quote, as only a line's start says
    /// where a block ends. Such an edit leaves the host's structure as it
    /// was, and an update with a time limit, a keystroke's, moves the
    /// host's tree over it and parses again only the fence's layer
    /// (`parse_layers_around`), the host left to the parse after the pause.
    /// tree-sitter-md's block grammar parses the whole of a fence again for
    /// a key typed in it, its every punctuation mark a node: a key in a
    /// fence of 10 KB took 4.2 ms of the highlighter, 2 of them the host's
    /// parse, against 1 for a key in a paragraph (elundqvist/kvist#376).
    /// The fence must have a layer of its content, one range: one with
    /// none is the host's alone.
    fn in_a_fence_line(&self, edit: &InputEdit, text: &Rope) -> bool {
        let (Some(tree), Some(grammar)) = (self.tree.as_ref(), self.parser.language()) else {
            return false;
        };
        // a tree a key's parse that ran out of time left is parsed at the
        // next key, as far as both edits reach, as before
        if !is_markdown_blocks(&grammar)
            || self.stale
            || self.anew
            || self.coloured_to.is_some()
        {
            return false;
        }
        let old = &self.text;
        let breaks = |text: &Rope, range: Range<usize>| {
            range.end > text.len() || text.slice(range).bytes().any(|b| b == b'\n' || b == b'\r')
        };
        if edit.old_end_byte < edit.start_byte
            || edit.new_end_byte < edit.start_byte
            || breaks(old, edit.start_byte..edit.old_end_byte)
            || breaks(text, edit.start_byte..edit.new_end_byte)
        {
            return false;
        }
        // the layer of the fence's content, one range: a combined layer's
        // many are gathered by the host's whole query
        let injected = self.injection_layers.iter().any(|layer| {
            matches!(layer.ranges.as_slice(), [r] if r.start_byte < edit.start_byte && edit.old_end_byte <= r.end_byte)
        });
        if !injected {
            return false;
        }
        let Some(mut node) = tree
            .root_node()
            .descendant_for_byte_range(edit.start_byte, edit.old_end_byte)
        else {
            return false;
        };
        let content = loop {
            if node.kind() == "code_fence_content" {
                break node;
            }
            match node.parent() {
                Some(parent) => node = parent,
                None => return false,
            }
        };
        if edit.start_byte <= content.start_byte() || content.end_byte() <= edit.old_end_byte {
            return false;
        }
        // the line's own text begins after what continues a list item or a
        // quote on it, and on the fence's first line where the content does
        let line_start = old.line_start_offset(old.offset_to_point(edit.start_byte).row);
        let mut own_start = line_start.max(content.start_byte());
        let mut cursor = content.walk();
        if cursor.goto_first_child_for_byte(line_start.saturating_sub(1)).is_some() {
            loop {
                let child = cursor.node();
                if child.start_byte() > edit.start_byte {
                    break;
                }
                if child.kind() == "block_continuation" && child.start_byte() >= line_start {
                    own_start = own_start.max(child.end_byte());
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        if own_start >= edit.start_byte {
            return false;
        }
        old.slice(own_start..edit.start_byte)
            .chars()
            .find(|c| !c.is_whitespace())
            .is_some_and(|c| c != '`' && c != '~')
    }

    /// Parse again each layer whose text holds all `edit` put in, from its
    /// tree moved over it, as the whole query would find it again: a key
    /// typed in a fence's line (`in_a_fence_line`). Returns whether each
    /// finished in its time.
    fn parse_layers_around(&mut self, edit: &InputEdit) -> bool {
        let mut finished = true;
        let timeout = Some(self.injection_parse_timeout);
        let text = self.text.clone();
        for layer in Arc::make_mut(&mut self.injection_layers) {
            let within = matches!(
                layer.ranges.as_slice(),
                [r] if r.start_byte < edit.start_byte && edit.new_end_byte <= r.end_byte
            );
            if !within {
                continue;
            }
            layer.catch_up();
            let (parsed, done) = Self::parse_injection_layer(
                &layer.language_name,
                layer.highlight_query.clone(),
                layer.ranges.clone(),
                Some(&layer.tree),
                &text,
                None,
                timeout,
            );
            finished &= done;
            if let Some(parsed) = parsed {
                *layer = InjectionLayer {
                    match_range: layer.match_range,
                    ..parsed
                };
            }
        }
        finished
    }

    /// Returns the language name for this highlighter.
    pub fn language(&self) -> &SharedString {
        &self.language
    }

    /// Returns a reference to the current text.
    pub fn text(&self) -> &Rope {
        &self.text
    }

    /// Take the trees of the last parse out, with the size of the text
    /// they are of: freeing the trees of a large text takes a while, some
    /// 80 ms for 20 MB of HTML, which the UI thread stood still for when an
    /// editor's text or language was replaced.
    pub(crate) fn take_parse(&mut self) -> (Parse, usize) {
        self.anew = false;
        self.again_pending = false;
        self.coloured_to = None;
        let parse = Parse {
            tree: self.tree.take(),
            injection_layers: std::mem::take(&mut self.injection_layers),
        };
        (parse, self.text.len())
    }

    /// Highlight the given text, returning a map from byte ranges to highlight captures.
    ///
    /// Uses incremental parsing by `edit` to efficiently update the highlighter's state.
    /// When `timeout` is `Some`, aborts if parsing exceeds the given duration
    /// and returns `false`. On timeout the old tree is preserved so highlighting
    /// still works with stale data, but `self.text` is updated so that the
    /// caller can send the current text to a background parse. It also
    /// returns `false` when an injected span runs out of the time it has of
    /// its own: the span keeps its old tree, or has no layer if it had none,
    /// until the background parse, which gives it all the time it needs,
    /// finishes it.
    /// With a `timeout`, a key typed in a line of a Markdown fence moves the
    /// host's tree and parses the fence's layer alone, and returns `false`
    /// for the host's parse after the pause (`in_a_fence_line`). It returns
    /// `false` too for a Markdown tree made from nothing, which the parse
    /// after the pause parses again from itself (`again_pending`), and for a
    /// span whose language's highlight query no one has compiled, where the
    /// highlighter defers those (`defer_cold_queries`).
    /// When `timeout` is `None`, parsing runs to completion and always returns `true`.
    pub fn update(
        &mut self,
        edit: Option<InputEdit>,
        text: &Rope,
        timeout: Option<Duration>,
    ) -> bool {
        self.update_in_steps(&[], edit, text, timeout)
    }

    /// `update` for changes made one after another, `steps`, which `edit`
    /// covers: the tree is moved by each in turn before it is parsed
    /// (`move_trees`).
    pub(crate) fn update_in_steps(
        &mut self,
        steps: &[InputEdit],
        edit: Option<InputEdit>,
        text: &Rope,
        timeout: Option<Duration>,
    ) -> bool {
        // the trees kept through a text made anew are not of it, however
        // alike the texts; and a text whose parse is the pause's still
        // (a tree edited and not parsed, one halted, one it is to parse
        // again) is the pause's, which this call does not cancel
        if self.text.eq(text) && !self.anew {
            return timeout.is_none() || !(self.stale || self.again_pending);
        }

        // If there's no grammar for the language, just update the text.
        if self.parser.language().is_none() {
            self.text = text.clone();
            return true;
        }
        // the parse, its injections' and what the edit moves, wherever it
        // ends
        let _parse = perf_spans().then(|| {
            tracing::trace_span!(target: "gpui::perf", "parse", language = self.language.as_ref(), bytes = text.len() as u64)
                .entered()
        });

        // trees kept from a text made anew are no start for a parse of
        // this one: it starts afresh, and they colour it until it finishes
        let kept = self.anew.then(|| Parse {
            tree: self.tree.take(),
            injection_layers: std::mem::take(&mut self.injection_layers),
        });
        // the key's own edit, which moves the kept trees where the parse
        // runs out of its time
        let key_edit = edit;
        // With no tree to start from, an edit says nothing: the tree of
        // the empty text is edited to all of this one. Edited by a key
        // alone, as if it were of the text before the key, it was a start
        // the parse reused whole, and it finished at once with a tree of
        // nothing that every key after it built on: a README typed into a
        // moment after it opened, where the whole parse ran out of its time
        // and left no tree, stood plain until it was opened again
        // (elundqvist/kvist#376).
        let edit = if self.tree.is_none() { None } else { edit };
        let key_steps = steps;
        let steps = if edit.is_some() { steps } else { &[] };

        // An edit of a text parsed before: what it reaches is all the
        // injections need looked at again.
        let edited = edit.is_some() && self.tree.is_some() && self.injections_query.is_some();
        let edit = edit.unwrap_or(InputEdit {
            start_byte: 0,
            old_end_byte: 0,
            new_end_byte: text.len(),
            start_position: Point::new(0, 0),
            old_end_position: Point::new(0, 0),
            new_end_position: Point::new(0, 0),
        });

        // a key typed in a line of a fenced code block, which a parse with
        // a time limit, a keystroke's, does not give the host's parser
        // (`in_a_fence_line`)
        let in_a_fence = timeout.is_some()
            && kept.is_none()
            && edited
            && steps.is_empty()
            && self.in_a_fence_line(&edit, text);

        // The tree of the empty text is made only where there is no tree:
        // made for every call, as `unwrap_or` made it, it was made before
        // anything else, and on a parser a time limit had halted it went on
        // with the halted parse instead.
        let from_nothing = self.tree.is_none();
        let mut old_tree = match self.tree.take() {
            Some(tree) => tree,
            None => self.parser.parse("", None).unwrap(),
        };
        edit_by_steps(&mut old_tree, steps, &edit);
        if kept.is_none() {
            self.edit_injection_layers(&edit);
            self.note_edit(&edit);
        }
        if in_a_fence {
            // the host's tree is moved, not parsed, and left to the parse
            // after the pause; the fence's layer is parsed again. The tree
            // moved is what a parse would give, with the fence's nodes
            // grown or shrunk over the key, but for the punctuation in the
            // fence, which no query colours: it is not stale
            self.tree = Some(old_tree);
            self.text = text.clone();
            let finished = self.parse_layers_around(&edit);
            self.layers_whole &= finished;
            return false;
        }

        let mut timed_out = false;
        let start = Instant::now();
        let mut progress = |_: &tree_sitter::ParseState| -> ControlFlow<()> {
            let Some(budget) = timeout else {
                return ControlFlow::Continue(());
            };

            if start.elapsed() > budget {
                timed_out = true;
                return ControlFlow::Break(()); // Cancel execution
            }

            ControlFlow::Continue(())
        };

        let options = ParseOptions::new().progress_callback(&mut progress);
        let new_tree = self.parser.parse_with_options(
            &mut move |offset, _| {
                if offset >= text.len() {
                    ""
                } else {
                    let (chunk, chunk_byte_ix) = text.chunk(offset);
                    &chunk[offset - chunk_byte_ix..]
                }
            },
            Some(&old_tree),
            Some(options),
        );

        if timed_out || new_tree.is_none() {
            // A halted parse is taken up again by the parser's next call,
            // from the stack and the old tree it was halted with, over
            // whatever text that call hands it, the tree that call hands it
            // passed over (tree-sitter's `Parser::reset`): reset, the next
            // call starts afresh from its own tree.
            self.parser.reset();
            // Restore the old tree so highlighting continues with stale data.
            match kept {
                // the trees of the text before it was made anew, which
                // colour it still
                Some(kept) => {
                    self.tree = kept.tree;
                    self.injection_layers = kept.injection_layers;
                    // moved with the key, as `edit_tree` moves them
                    if let Some(key_edit) = key_edit {
                        self.move_trees(key_steps, &key_edit);
                    }
                }
                // the tree of the empty text, edited to this one, colours
                // nothing, and the parse that finishes this one is to
                // start from nothing, so that a tree it would not reuse
                // is parsed again from itself
                None if from_nothing => self.again_pending = false,
                None => self.tree = Some(old_tree),
            }
            self.text = text.clone();
            self.mark_stale();
            return false;
        }

        let mut new_tree = new_tree.unwrap();
        // A tree made from nothing that the next parse would not reuse
        // is parsed again from itself, edited at its start
        // (`parsed_again_from_itself`), and that rebuilds every node of
        // the text, a quarter to three quarters of the whole parse: not on
        // the drawing thread, where it ran out of its time and left the
        // tree as it was, for every key after it to build again, and took
        // the open's frame longer where it did not. A call with a time
        // limit has the parse after the pause do it (`again_pending`); one
        // without has no other.
        let mut again_pending = false;
        if from_nothing && parsed_again_from_itself(&new_tree.language()) {
            if timeout.is_some() {
                again_pending = true;
            } else {
                let again = self.parser.parse_with_options(
                    &mut move |offset, _| {
                        if offset >= text.len() {
                            ""
                        } else {
                            let (chunk, chunk_byte_ix) = text.chunk(offset);
                            &chunk[offset - chunk_byte_ix..]
                        }
                    },
                    Some(&edited_at_its_start(&new_tree)),
                    None,
                );
                if let Some(again) = again {
                    new_tree = again;
                }
            }
        }
        self.again_pending = again_pending;
        self.tree = Some(new_tree.clone());
        self.anew = false;
        self.coloured_to = None;
        self.stale = false;
        self.text = text.clone();
        let reach = edited.then(|| self.reach(&old_tree, &new_tree)).flatten();
        let injections_finished =
            self.parse_injection_layers(&new_tree, reach, timeout.is_some());
        if injections_finished {
            self.edited.clear();
        }
        // Only a caller with a time limit parses again in the background.
        (injections_finished && !again_pending) || timeout.is_none()
    }

    /// What the edits since the last finished parse reach in `new_tree`,
    /// parsed from `old_tree` edited by them: where they lie, a byte
    /// either side, and where the two trees' structure differs. `None`
    /// where the layers are not every span a whole query found, or nothing
    /// was edited.
    fn reach(&self, old_tree: &Tree, new_tree: &Tree) -> Option<Range<usize>> {
        if !self.layers_whole {
            return None;
        }
        let changed = old_tree.changed_ranges(new_tree).map(|r| r.start_byte..r.end_byte);
        let (start, end) = self
            .edited
            .iter()
            .cloned()
            .chain(changed)
            .fold((usize::MAX, 0), |(start, end), r| (start.min(r.start), end.max(r.end)));
        (start <= end).then(|| start.saturating_sub(1)..(end + 1).min(self.text.len()))
    }

    /// Returns the data needed to compute injection layers on a background thread.
    /// Returns `None` if this language has no injections.
    #[cfg(all(test, feature = "tree-sitter-languages"))]
    pub(crate) fn injection_parse_data(&self) -> Option<InjectionParseData> {
        self.injection_parse_source().map(InjectionParseSource::into_data)
    }

    /// What a parse of the injections off the UI thread needs, the layers
    /// shared rather than copied (`InjectionParseSource`). Returns `None`
    /// if this language has no injections.
    pub(crate) fn injection_parse_source(&self) -> Option<InjectionParseSource> {
        let query = self.injections_query.clone()?;
        Some(InjectionParseSource {
            query,
            content_capture_index: self.injection_content_capture_index,
            language_capture_index: self.injection_language_capture_index,
            // a text made anew shares nothing with the old layers' trees
            layers: if self.anew { Arc::default() } else { self.injection_layers.clone() },
            // the edits since the last finished parse hold every edit a
            // layer's tree has not seen, its edges included
            edited: self.edited.clone(),
            parse_timeout: self.injection_parse_timeout,
            roots: self.injection_roots.clone(),
        })
    }

    /// Compute injection layers from a freshly-parsed main tree, and whether
    /// every span's parse finished in time.
    /// This is pure computation with no side effects and is safe to run on a
    /// background thread.
    ///
    /// With a `reach` (`InjectionParseData::reach`), an old layer whose
    /// match lies outside it is kept as it is, its tree moved with the text
    /// and not parsed again, and only the reach, grown over the matches of
    /// the old layers it touches, is queried again. The query over the whole
    /// text visits every node of it, and on a keystroke in a README of 190
    /// KB it was nearly all of the keystroke: 9 ms of 10, the parse of the
    /// host and of the layers the rest.
    pub(crate) fn compute_injection_layers(
        mut data: InjectionParseData,
        tree: &Tree,
        text: &Rope,
    ) -> (Vec<InjectionLayer>, bool) {
        if let Some((span, kept)) = Self::layers_beyond(&data) {
            // the layers the query looks at again are reused moved to
            // where their text is
            for (layer, _) in data.old_layers.iter_mut().zip(&kept).filter(|(_, kept)| !**kept) {
                layer.catch_up();
            }
            let (mut layers, finished, parses) =
                Self::query_injection_layers(&data, tree, text, Some(span));
            let kept_count = kept
                .iter()
                .zip(&data.old_layers)
                .filter(|(kept, layer)| {
                    **kept && !injection_spans_are_paragraphs(&layer.language_name)
                })
                .count();
            // past the cap a whole query keeps only the first spans
            if parses + kept_count < MAX_NON_COMBINED_INJECTION_PARSES {
                for (layer, _) in data.old_layers.into_iter().zip(kept).filter(|(_, kept)| *kept) {
                    let Some(byte_range) = bounding_byte_range(&layer.ranges) else {
                        continue;
                    };
                    layers.push(InjectionLayer {
                        language_name: layer.language_name,
                        highlight_query: layer.highlight_query,
                        ranges: layer.ranges,
                        byte_range,
                        tree: layer.tree,
                        stale: false,
                        match_range: layer.match_range,
                        moved_from: layer.moved_from,
                    });
                }
                layers.sort_by_key(|layer| layer.byte_range.start);
                return (layers, finished);
            }
        }
        for layer in &mut data.old_layers {
            layer.catch_up();
        }
        let (layers, finished, _) = Self::query_injection_layers(&data, tree, text, None);
        (layers, finished)
    }

    /// The span a query must look at again for `data.reach`, and which of
    /// the old layers are kept: the reach, grown over the match of every old
    /// layer it touches, as that match is looked for again. `None` where
    /// the whole text must be queried: no reach, a combined pattern, whose
    /// one layer gathers spans from all over the text, an old layer whose
    /// match is not known, or as many as the cap counts.
    fn layers_beyond(data: &InjectionParseData) -> Option<(Range<usize>, Vec<bool>)> {
        let mut span = data.reach.clone()?;
        let combined = (0..data.query.pattern_count()).any(|i| {
            data.query
                .property_settings(i)
                .iter()
                .any(|prop| prop.key.as_ref() == "injection.combined")
        });
        let counted = data
            .old_layers
            .iter()
            .filter(|layer| !injection_spans_are_paragraphs(&layer.language_name))
            .count();
        if combined || counted >= MAX_NON_COMBINED_INJECTION_PARSES {
            return None;
        }
        let matched = data
            .old_layers
            .iter()
            .map(|layer| layer.match_range.map(|r| r.start_byte..r.end_byte))
            .collect::<Option<Vec<_>>>()?;
        let mut kept = vec![true; matched.len()];
        loop {
            let mut grown = false;
            for (range, kept) in matched.iter().zip(kept.iter_mut()) {
                if *kept && range.start <= span.end && span.start <= range.end {
                    *kept = false;
                    if range.start < span.start || range.end > span.end {
                        span = span.start.min(range.start)..span.end.max(range.end);
                        grown = true;
                    }
                }
            }
            if !grown {
                return Some((span, kept));
            }
        }
    }

    /// Query the injections of `tree`, over the whole of it or over `span`,
    /// and parse a layer for each: the layers, whether every span's parse
    /// finished in time, and how many spans that are not combined were
    /// parsed, which the cap counts.
    fn query_injection_layers(
        data: &InjectionParseData,
        tree: &Tree,
        text: &Rope,
        span: Option<Range<usize>>,
    ) -> (Vec<InjectionLayer>, bool, usize) {
        struct CombinedRanges {
            ranges: Vec<tree_sitter::Range>,
            byte_count: usize,
        }

        impl CombinedRanges {
            /// Ranges are already filtered by `should_include_injection_range`
            /// before being pushed here; this only enforces the count/byte caps.
            fn push_limited(&mut self, ranges: Vec<tree_sitter::Range>) {
                for range in ranges {
                    if self.ranges.len() >= MAX_INJECTION_RANGES {
                        break;
                    }

                    let range_len = injection_range_len(&range);
                    if self.byte_count.saturating_add(range_len) > MAX_INJECTION_BYTES {
                        break;
                    }

                    self.byte_count += range_len;
                    self.ranges.push(range);
                }
            }
        }

        fn sort_ranges(ranges: &mut [tree_sitter::Range]) {
            ranges.sort_unstable_by(|a, b| {
                a.start_byte
                    .cmp(&b.start_byte)
                    .then_with(|| a.end_byte.cmp(&b.end_byte))
            });
        }

        fn ranges_cache_key(ranges: &[tree_sitter::Range]) -> Vec<(usize, usize)> {
            ranges.iter().map(|r| (r.start_byte, r.end_byte)).collect()
        }

        /// The key of ranges that differ only in where the first one starts.
        /// Text typed at the very start of a span moves that start and no
        /// other, so the span's ranges match none of the old ones exactly; the
        /// parser reads the new bytes at the front and reuses what follows.
        /// Ranges that differ anywhere else get no old tree: the parser can
        /// reuse an old node that was read up to a range end that has since
        /// moved, and highlight otherwise than a fresh parse does.
        fn front_moved_key(ranges: &[tree_sitter::Range]) -> Vec<(usize, usize)> {
            let mut key = ranges_cache_key(ranges);
            if let Some(first) = key.first_mut() {
                first.0 = usize::MAX;
            }
            key
        }

        /// What a language name a span asks for comes to.
        #[derive(Clone)]
        enum Resolved {
            /// The language, by its own name, and its highlight query.
            Ready(SharedString, Arc<Query>),
            /// A language whose query no one has compiled, which this
            /// update leaves to the parse after the pause
            /// (`InjectionParseData::defer_cold_queries`).
            Cold,
            Unknown,
        }

        fn resolve_language(
            language_name: &str,
            query_cache: &mut HashMap<SharedString, Arc<Query>>,
            defer_cold: bool,
            cold: &mut Vec<LanguageConfig>,
        ) -> Resolved {
            let Some(config) = LanguageRegistry::singleton().language(language_name) else {
                return Resolved::Unknown;
            };
            if let Some(query) = query_cache.get(&config.name) {
                return Resolved::Ready(config.name, query.clone());
            }
            if config.language.is_none() {
                return Resolved::Unknown;
            }
            // a paragraph's language is every document's, and takes some
            // 0.5 ms
            let compiled = if defer_cold && !injection_spans_are_paragraphs(&config.name) {
                match queries::injected_if_compiled(&config) {
                    Some(compiled) => compiled,
                    None => {
                        cold.push(config);
                        return Resolved::Cold;
                    }
                }
            } else {
                queries::injected(&config)
            };
            let Some(query) = compiled else {
                return Resolved::Unknown;
            };
            query_cache.insert(config.name.clone(), query.clone());
            Resolved::Ready(config.name, query)
        }

        let root_node = tree.root_node();
        let mut cursor = QueryCursor::new();
        if let Some(span) = span.clone() {
            cursor.set_byte_range(span);
        }
        let mut matches = cursor.matches(&data.query, root_node, TextProvider(text, 0));

        let mut combined_ranges: HashMap<SharedString, CombinedRanges> = HashMap::new();
        // A span the query finds over `span` lies across it, and so does an
        // old layer of the same ranges: the rest, a paragraph layer to every
        // paragraph of a long document, are kept or gone and need no key
        let within_span = |layer: &&ReusableInjectionLayer| {
            span.as_ref().is_none_or(|span| {
                bounding_byte_range(&layer.ranges)
                    .is_some_and(|range| range.start <= span.end && span.start <= range.end)
            })
        };
        let old_layer_trees: HashMap<_, _> = data
            .old_layers
            .iter()
            .filter(within_span)
            .map(|layer| {
                (
                    (layer.language_name.clone(), ranges_cache_key(&layer.ranges)),
                    layer,
                )
            })
            .collect();
        let front_moved_old_layer_trees: HashMap<_, _> = data
            .old_layers
            .iter()
            .filter(within_span)
            .map(|layer| {
                (
                    (layer.language_name.clone(), front_moved_key(&layer.ranges)),
                    &layer.tree,
                )
            })
            .collect();
        // A combined layer takes an old tree only when its ranges match
        // exactly: one whose first range had moved was seen to highlight
        // otherwise than a fresh parse.
        let old_tree_for =
            |language_name: &SharedString, ranges: &[tree_sitter::Range], combined: bool| {
                let exact = old_layer_trees.get(&(language_name.clone(), ranges_cache_key(ranges)));
                if exact.is_some() || combined {
                    return exact.map(|layer| &layer.tree);
                }
                front_moved_old_layer_trees
                    .get(&(language_name.clone(), front_moved_key(ranges)))
                    .copied()
            };
        // a paragraph's old tree, where its ranges are as they were and no
        // edit has reached its text: what a parse of it would give again
        let intact_tree_for = |language_name: &SharedString, ranges: &[tree_sitter::Range]| {
            old_layer_trees
                .get(&(language_name.clone(), ranges_cache_key(ranges)))
                .filter(|layer| layer.intact)
                .map(|layer| layer.tree.clone())
        };
        // what is left of the time the paragraphs share, where they share it
        let mut paragraphs_left = data
            .paragraphs_share_timeout
            .then_some(data.parse_timeout)
            .flatten();
        let mut finished = true;
        // Query objects are relatively expensive. Reuse one Arc per language
        // from the previous parse and compile only languages present in this
        // document, rather than eagerly retaining every registered grammar.
        let mut highlight_queries: HashMap<SharedString, Arc<Query>> = HashMap::new();
        // a run of layers of one language, the paragraphs between two
        // fences, is that language once
        let mut last_language: Option<&SharedString> = None;
        for layer in &data.old_layers {
            if last_language != Some(&layer.language_name) {
                highlight_queries
                    .entry(layer.language_name.clone())
                    .or_insert_with(|| layer.highlight_query.clone());
                last_language = Some(&layer.language_name);
            }
        }
        // Cache raw names as well as canonical queries. Otherwise every fence with the
        // same info string would lock the registry and clone its language configuration.
        let mut resolved_languages: HashMap<SharedString, Resolved> = HashMap::new();
        let mut cold_languages = Vec::new();
        let mut new_layers = Vec::new();
        let mut non_combined_parses = 0usize;
        while let Some(query_match) = matches.next() {
            let mut language_name: Option<SharedString> = None;
            let mut combined = false;
            for prop in data.query.property_settings(query_match.pattern_index) {
                match prop.key.as_ref() {
                    "injection.language" => {
                        language_name = prop
                            .value
                            .as_ref()
                            .map(|v| SharedString::from(v.to_string()));
                    }
                    "injection.combined" => combined = true,
                    _ => {}
                }
            }

            // a paragraph's span is not counted (`injection_spans_are_paragraphs`)
            let paragraph = !combined
                && language_name
                    .as_ref()
                    .is_some_and(|name| injection_spans_are_paragraphs(name));
            // Skip rather than break, so later combined ranges are still collected.
            if !combined && !paragraph && non_combined_parses >= MAX_NON_COMBINED_INJECTION_PARSES
            {
                continue;
            }

            if language_name.is_none() {
                language_name = query_match
                    .captures
                    .iter()
                    .find(|cap| Some(cap.index) == data.language_capture_index)
                    .and_then(|capture| {
                        captured_injection_language(text, capture.node.byte_range())
                    });
            }

            let Some(raw_language_name) = language_name else {
                continue;
            };
            let resolved_language =
                if let Some(resolved) = resolved_languages.get(&raw_language_name) {
                    resolved.clone()
                } else {
                    let resolved = resolve_language(
                        &raw_language_name,
                        &mut highlight_queries,
                        data.defer_cold_queries,
                        &mut cold_languages,
                    );
                    resolved_languages.insert(raw_language_name, resolved.clone());
                    resolved
                };
            let (language_name, highlight_query) = match resolved_language {
                Resolved::Ready(name, query) => (name, query),
                Resolved::Cold => {
                    finished = false;
                    continue;
                }
                Resolved::Unknown => continue,
            };

            let mut ranges = query_match
                .captures
                .iter()
                .filter(|cap| Some(cap.index) == data.content_capture_index)
                .map(|capture| capture.node.range())
                .collect::<Vec<_>>();

            if ranges.is_empty() {
                continue;
            }
            ranges.retain(|range| should_include_injection_range(&language_name, range, text));
            if ranges.is_empty() {
                continue;
            }
            sort_ranges(&mut ranges);

            if combined {
                combined_ranges
                    .entry(language_name.clone())
                    .or_insert_with(|| CombinedRanges {
                        ranges: Vec::new(),
                        byte_count: 0,
                    })
                    .push_limited(ranges);
            } else {
                if !injection_ranges_within_limits(&ranges) {
                    continue;
                }

                let root_kind = data.roots.get(query_match.pattern_index).cloned().flatten();
                let match_range = match_range(root_node, query_match.captures, root_kind.as_deref());
                if !paragraph {
                    non_combined_parses += 1;
                } else if let Some(tree) = intact_tree_for(&language_name, &ranges) {
                    if let Some(byte_range) = bounding_byte_range(&ranges) {
                        new_layers.push(InjectionLayer {
                            language_name,
                            highlight_query,
                            ranges,
                            byte_range,
                            tree,
                            stale: false,
                            match_range,
                            moved_from: None,
                        });
                    }
                    continue;
                }
                let old_tree = old_tree_for(&language_name, &ranges, false);
                let timeout = if paragraph && paragraphs_left.is_some() {
                    paragraphs_left
                } else {
                    data.parse_timeout
                };
                // the paragraphs' time is spent: the rest keep their old
                // trees, stale, for the background parse, and are not
                // begun, a parser made for nothing for each of thousands
                let began = Instant::now();
                let layer = if timeout.is_some_and(|timeout| timeout.is_zero()) && paragraph {
                    finished = false;
                    old_tree.cloned().and_then(|tree| {
                        Some(InjectionLayer {
                            language_name: language_name.clone(),
                            highlight_query,
                            byte_range: bounding_byte_range(&ranges)?,
                            ranges,
                            tree,
                            stale: true,
                            match_range: None,
                            moved_from: None,
                        })
                    })
                } else {
                    let (layer, done) = Self::parse_injection_layer(
                        &language_name,
                        highlight_query,
                        ranges,
                        old_tree,
                        text,
                        data.cancel.as_deref(),
                        timeout,
                    );
                    finished &= done;
                    layer
                };
                if paragraph {
                    paragraphs_left =
                        paragraphs_left.map(|left| left.saturating_sub(began.elapsed()));
                }
                if let Some(layer) = layer {
                    new_layers.push(InjectionLayer { match_range, ..layer });
                }
            }
        }

        for (language_name, combined) in combined_ranges {
            let mut ranges = combined.ranges;
            if ranges.is_empty() {
                continue;
            }
            sort_ranges(&mut ranges);
            ranges = normalize_combined_injection_ranges(&language_name, ranges);
            if ranges.is_empty() {
                continue;
            }
            let old_tree = old_tree_for(&language_name, &ranges, true);
            let Some(highlight_query) = highlight_queries.get(&language_name).cloned() else {
                continue;
            };
            let (layer, done) = Self::parse_injection_layer(
                &language_name,
                highlight_query,
                ranges,
                old_tree,
                text,
                data.cancel.as_deref(),
                data.parse_timeout,
            );
            finished &= done;
            if let Some(layer) = layer {
                new_layers.push(layer);
            }
        }
        if !cold_languages.is_empty() {
            queries::compile_in_background(cold_languages);
        }
        new_layers.sort_by_key(|layer| layer.byte_range.start);
        (new_layers, finished, non_combined_parses)
    }

    /// Parse one injection layer over the given included ranges, from the
    /// span's old tree (edited to the text) when there is one and the
    /// language's incremental parse can be trusted (`injection_parses_from_scratch`).
    ///
    /// A parse that runs out of `timeout` (the data's `parse_timeout`, or
    /// what the paragraphs have left of it), or is cancelled, says so (the
    /// second of what it returns is false) and keeps the old tree as a
    /// stale layer, so the span keeps its last highlights where the edits
    /// did not touch it until a parse of it finishes, instead of going
    /// plain; a span with no old tree is left out.
    fn parse_injection_layer(
        language_name: &SharedString,
        highlight_query: Arc<Query>,
        ranges: Vec<tree_sitter::Range>,
        old_tree: Option<&Tree>,
        text: &Rope,
        cancel: Option<&AtomicBool>,
        timeout: Option<Duration>,
    ) -> (Option<InjectionLayer>, bool) {
        let Some(config) = LanguageRegistry::singleton().language(language_name) else {
            return (None, true);
        };
        let mut parser = Parser::new();
        let Some(language) = config.language.as_ref() else {
            return (None, true);
        };
        if parser.set_language(language).is_err() || parser.set_included_ranges(&ranges).is_err()
        {
            return (None, true);
        }
        #[cfg(test)]
        LAYER_PARSES.with(|parses| parses.set(parses.get() + 1));
        let parse_start = Instant::now();
        let mut timed_out = false;
        let mut progress = |_: &tree_sitter::ParseState| -> ControlFlow<()> {
            if timeout.is_some_and(|timeout| parse_start.elapsed() > timeout)
                || cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed))
            {
                timed_out = true;
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let options = ParseOptions::new().progress_callback(&mut progress);

        let parse_from = if injection_parses_from_scratch(language_name) {
            None
        } else {
            old_tree
        };
        let new_tree = parser.parse_with_options(
            &mut |offset, _| {
                if offset >= text.len() {
                    ""
                } else {
                    let (chunk, chunk_byte_ix) = text.chunk(offset);
                    &chunk[offset - chunk_byte_ix..]
                }
            },
            parse_from,
            Some(options),
        );
        let (tree, stale) = match new_tree {
            Some(tree) if !timed_out => (tree, false),
            _ => match old_tree {
                Some(tree) => (tree.clone(), true),
                None => return (None, false),
            },
        };

        let layer = bounding_byte_range(&ranges).map(|byte_range| InjectionLayer {
            language_name: language_name.clone(),
            highlight_query,
            ranges,
            byte_range,
            tree,
            stale,
            match_range: None,
            moved_from: None,
        });
        (layer, !stale)
    }

    /// Apply a tree that was parsed on a background thread.
    ///
    /// `injection_layers` must also be pre-computed in the background via
    /// [`compute_injection_layers`] to avoid blocking the main thread.
    /// Take the trees a parse off the UI thread made of `text`, where the
    /// text is still that. The trees they take the place of are handed
    /// back, for the caller to free where it likes, where they were kept
    /// through a text made anew to colour it: an old text's whole trees,
    /// which share nothing with the new. Others share nearly every node
    /// with the new ones, and are freed here for next to nothing.
    pub(crate) fn apply_background_tree(
        &mut self,
        tree: Tree,
        text: &Rope,
        injection_layers: Vec<InjectionLayer>,
    ) -> Option<Parse> {
        // Only apply if the text still matches what was parsed.
        if !self.text.eq(text) {
            return None;
        }

        let replaced = Parse {
            tree: self.tree.replace(tree),
            injection_layers: std::mem::replace(
                &mut self.injection_layers,
                Arc::new(injection_layers),
            ),
        };
        self.stale = false;
        self.again_pending = false;
        self.layers_whole = true;
        self.edited.clear();
        // the trees are of the text now: a key's parse starts from them,
        // and the fold candidates are found in them. Left set, every key's
        // parse after a paste over a large file started from nothing, and
        // an edit's lines found no folds, so a rename opened every fold
        // between its first and last change (elundqvist/kvist#401)
        let kept = std::mem::take(&mut self.anew);
        self.coloured_to = None;
        kept.then_some(replaced)
    }

    /// Parse injection layers after the main tree is updated, and return
    /// whether every span's parse finished in time. With a `reach`, only
    /// what it reaches is queried again (`InjectionParseData::reach`), and
    /// where the update has a time limit, the paragraphs share theirs
    /// (`InjectionParseData::paragraphs_share_timeout`).
    /// pattern: parse once in update, query many times in render.
    fn parse_injection_layers(
        &mut self,
        tree: &Tree,
        reach: Option<Range<usize>>,
        timed: bool,
    ) -> bool {
        let Some(query) = self.injections_query.clone() else {
            self.injection_layers = Arc::default();
            return true;
        };
        // the old layers are moved into the parse, not copied: the parse
        // after the pause makes its own of them as it begins, and holds
        // them no longer (`InjectionParseSource`)
        let layers = Arc::try_unwrap(std::mem::take(&mut self.injection_layers))
            .unwrap_or_else(|shared| (*shared).clone());
        let data = InjectionParseData {
            query,
            content_capture_index: self.injection_content_capture_index,
            language_capture_index: self.injection_language_capture_index,
            old_layers: layers
                .into_iter()
                .map(|layer| ReusableInjectionLayer::of(layer, &self.edited))
                .collect(),
            parse_timeout: Some(self.injection_parse_timeout),
            cancel: None,
            roots: self.injection_roots.clone(),
            reach,
            paragraphs_share_timeout: timed,
            defer_cold_queries: self.defer_cold_queries && timed,
        };
        let (layers, finished) = Self::compute_injection_layers(data, tree, &self.text.clone());
        self.injection_layers = Arc::new(layers);
        self.layers_whole = finished;
        finished
    }

    /// Match the visible ranges of nodes in the Tree for highlighting.
    fn match_styles(&self, range: Range<usize>) -> Vec<HighlightItem> {
        let mut highlights = vec![];
        let mut injection_highlights = vec![];
        let Some(tree) = &self.tree else {
            return highlights;
        };
        // trees kept through a text made anew colour only what the two
        // texts share at their start
        let range = match self.coloured_to {
            Some(end) if end <= range.start => return highlights,
            Some(end) => range.start..range.end.min(end),
            None => range,
        };

        let Some(query) = &self.query else {
            return highlights;
        };

        let root_node = tree.root_node();
        let source = &self.text;

        // Query pre-parsed injection layers.
        let mut last_layer_start = 0;
        for layer in self.injection_layers.iter() {
            debug_assert!(layer.byte_range.start >= last_layer_start);
            last_layer_start = layer.byte_range.start;

            if layer.byte_range.end <= range.start {
                continue;
            }

            // Layers are sorted by start byte in compute_injection_layers.
            if layer.byte_range.start >= range.end {
                break;
            }

            let query = &layer.highlight_query;
            // a tree the edits before its text have not moved yet has it
            // `shift` bytes back, the text its predicates test included
            let shift = layer.shift();
            let in_tree = |byte: usize| byte.saturating_add_signed(-shift);
            let in_text = |byte: usize| byte.saturating_add_signed(shift);

            // A layer's tree has nothing to say outside the layer's span.
            let mut query_cursor = QueryCursor::new();
            query_cursor.set_byte_range(
                in_tree(range.start.max(layer.byte_range.start))
                    ..in_tree(range.end.min(layer.byte_range.end)),
            );

            let mut matches =
                query_cursor.matches(query, layer.tree.root_node(), TextProvider(&self.text, shift));

            let mut last_end = 0usize;
            while let Some(m) = matches.next() {
                let allow_overlapping_captures = query
                    .property_settings(m.pattern_index)
                    .iter()
                    .any(|prop| prop.key.as_ref() == "highlight.allow-overlap");

                for cap in m.captures {
                    // In a stale layer, a node an edit touched stretches over
                    // or shrinks with whatever the edit did, and says nothing
                    // true about what is there now: a token grown over a paste
                    // would colour all of it.
                    if layer.stale && self.touched_at(&cap.node, shift) {
                        continue;
                    }

                    let node_range = in_text(cap.node.start_byte())..in_text(cap.node.end_byte());

                    if !allow_overlapping_captures && node_range.start < last_end {
                        continue;
                    }

                    if let Some(highlight_name) = query.capture_names().get(cap.index as usize) {
                        if !allow_overlapping_captures {
                            last_end = node_range.end;
                        }
                        injection_highlights.push(HighlightItem::new(
                            node_range,
                            SharedString::from(highlight_name.to_string()),
                        ));
                    }
                }
            }
        }

        let (query_nodes, gone_into) = collect_query_nodes(root_node, &range);

        // What matches on a node gone into, at that node alone, first: it
        // is the outer, and a string or a function far larger than the view
        // keeps its colour. Then the nodes in view, whole.
        let gone_into = gone_into.iter().map(|node| (node, Some(0)));
        for (query_node, max_start_depth) in gone_into.chain(query_nodes.iter().map(|n| (n, None)))
        {
            let mut query_cursor = QueryCursor::new();
            query_cursor.set_byte_range(range.clone());
            query_cursor.set_max_start_depth(max_start_depth);

            let mut matches = query_cursor.matches(&query, *query_node, TextProvider(source, 0));

            while let Some(query_match) = matches.next() {
                for cap in query_match.captures {
                    let node = cap.node;

                    // In a stale tree, as in a stale layer, a node an edit
                    // touched stretches over or shrinks with whatever the
                    // edit did: a string pasted into would colour the paste
                    if self.stale && self.touched(&node) && !self.injected_within(node.byte_range())
                    {
                        continue;
                    }

                    let Some(highlight_name) = query.capture_names().get(cap.index as usize) else {
                        continue;
                    };

                    let node_range: Range<usize> = node.start_byte()..node.end_byte();
                    let highlight_name = SharedString::from(highlight_name.to_string());

                    // Merge near range and same highlight name
                    let last_item = highlights.last();
                    let last_range = last_item.map(|item| &item.range).unwrap_or(&(0..0));
                    let last_highlight_name = last_item.map(|item| item.name.clone());

                    if last_range == &node_range {
                        // case:
                        // last_range: 213..220, last_highlight_name: Some("property")
                        // last_range: 213..220, last_highlight_name: Some("string")
                        highlights.push(HighlightItem::new(
                            node_range,
                            last_highlight_name.unwrap_or(highlight_name),
                        ));
                    } else {
                        highlights.push(HighlightItem::new(node_range, highlight_name.clone()));
                    }
                }
            }
        }

        // Injected languages are more specific than the host language. Keep
        // them last so their colors win over broad Markdown captures such as
        // `fenced_code_block @text.literal`.
        highlights.extend(injection_highlights);
        if let Some(end) = self.coloured_to {
            highlights.retain_mut(|item| {
                item.range.end = item.range.end.min(end);
                item.range.start < item.range.end
            });
        }

        // DO NOT REMOVE THIS PRINT, it's useful for debugging
        // for item in highlights {
        //     println!("item: {:?}", item);
        // }

        highlights
    }

    /// Returns the syntax highlight styles for a range of text.
    ///
    /// The argument `range` is the range of bytes in the text to highlight.
    ///
    /// Returns a vector of tuples where each tuple contains:
    /// - A byte range relative to the text
    /// - The corresponding highlight style for that range
    ///
    /// # Example
    ///
    /// ```no_run
    /// use gpui_kit::component::highlighter::{HighlightTheme, SyntaxHighlighter};
    /// use ropey::Rope;
    ///
    /// let code = "fn main() {\n    println!(\"Hello\");\n}";
    /// let rope = Rope::from_str(code);
    /// let mut highlighter = SyntaxHighlighter::new("rust");
    /// highlighter.update(None, &rope, None);
    ///
    /// let theme = HighlightTheme::default_dark();
    /// let range = 0..code.len();
    /// let styles = highlighter.styles(&range, &theme);
    /// ```
    pub fn styles(
        &self,
        range: &Range<usize>,
        theme: &dyn gpui_base::input::HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let _highlight = perf_spans().then(|| {
            tracing::trace_span!(target: "gpui::perf", "highlight", language = self.language.as_ref(), bytes = range.len() as u64)
                .entered()
        });
        let mut styles = vec![];
        let start_offset = range.start;

        let highlights = self.match_styles(range.clone());

        // let mut iter_count = 0;
        for item in highlights {
            // iter_count += 1;
            let node_range = &item.range;
            let name = &item.name;

            // Avoid start larger than end
            let mut node_range = node_range.start.max(range.start)..node_range.end.min(range.end);
            if node_range.start > node_range.end {
                node_range.end = node_range.start;
            }
            // The tree can be stale while a background reparse is pending
            // (sync-parse timeout, or the large-text `edit_tree` path), so
            // node offsets may fall inside multi-byte characters of the
            // current text. Snap to char boundaries — text shaping panics on
            // a mid-char style boundary.
            node_range = self.text.clip_offset(node_range.start, Bias::Left)
                ..self.text.clip_offset(node_range.end, Bias::Right);
            if node_range.is_empty() {
                continue;
            }

            styles.push((node_range, theme.style(name.as_ref()).unwrap_or_default()));
        }

        // If the matched styles is empty, return a default range.
        if styles.len() == 0 {
            return vec![(start_offset..range.end, HighlightStyle::default())];
        }

        let styles = unique_styles(&range, styles);

        // NOTE: DO NOT remove this comment, it is used for debugging.
        // for style in &styles {
        //     println!("---- style: {:?} - {:?}", style.0, style.1.color);
        // }
        // println!("--------------------------------");

        styles
    }
}

/// Move `tree` by `steps`, the changes `edit` covers one by one, where
/// there are any, else by `edit` (`SyntaxHighlighter::move_trees`).
fn edit_by_steps(tree: &mut Tree, steps: &[InputEdit], edit: &InputEdit) {
    if steps.is_empty() {
        tree.edit(edit);
    }
    for step in steps {
        tree.edit(step);
    }
}

/// How many bytes `a` and `b` begin with alike, at a boundary of a
/// character: read a chunk at a time, and only as far as they are alike.
fn common_prefix(a: &Rope, b: &Rope) -> usize {
    let (mut ca, mut cb) = (a.chunks(), b.chunks());
    let (mut x, mut y): (&[u8], &[u8]) = (&[], &[]);
    let mut same = 0;
    loop {
        if x.is_empty() {
            match ca.next() {
                Some(chunk) => x = chunk.as_bytes(),
                None => break,
            }
            continue;
        }
        if y.is_empty() {
            match cb.next() {
                Some(chunk) => y = chunk.as_bytes(),
                None => break,
            }
            continue;
        }
        let n = x.len().min(y.len());
        let alike = if x[..n] == y[..n] {
            n
        } else {
            x[..n].iter().zip(&y[..n]).take_while(|(p, q)| p == q).count()
        };
        same += alike;
        if alike < n {
            break;
        }
        (x, y) = (&x[n..], &y[n..]);
    }
    a.clip_offset(same, Bias::Left)
}

/// How many bytes `a` and `b` end with alike, at most `most`, at a
/// boundary of a character: read back from their ends a chunk at a time.
fn common_suffix(a: &Rope, b: &Rope, most: usize) -> usize {
    let (mut at_a, mut at_b) = (a.len(), b.len());
    let mut same = 0;
    while same < most {
        let (chunk_a, start_a) = a.chunk(at_a - 1);
        let (chunk_b, start_b) = b.chunk(at_b - 1);
        let x = &chunk_a.as_bytes()[..at_a - start_a];
        let y = &chunk_b.as_bytes()[..at_b - start_b];
        let n = x.len().min(y.len()).min(most - same);
        let (x, y) = (&x[x.len() - n..], &y[y.len() - n..]);
        let alike = if x == y {
            n
        } else {
            x.iter().rev().zip(y.iter().rev()).take_while(|(p, q)| p == q).count()
        };
        same += alike;
        if alike < n {
            break;
        }
        at_a -= n;
        at_b -= n;
    }
    a.len() - a.clip_offset(a.len() - same, Bias::Right)
}

/// To merge intersection ranges, let the subsequent range cover
/// the previous overlapping range and split the previous range.
///
/// From:
///
/// AA
///   BBB
///    CCCCC
///      DD
///         EEEE
///
/// To:
///
/// AABCCDDCEEEE
pub(crate) fn unique_styles(
    total_range: &Range<usize>,
    styles: Vec<(Range<usize>, HighlightStyle)>,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let styles: Vec<_> = styles
        .into_iter()
        .filter(|(range, _)| !range.is_empty())
        .collect();

    if styles.is_empty() {
        return styles;
    }

    // Create intervals: (position, is_start, style_index)
    let mut intervals: Vec<(usize, bool, usize)> = Vec::with_capacity(styles.len() * 2 + 2);
    for (i, (range, _)) in styles.iter().enumerate() {
        intervals.push((range.start, true, i));
        intervals.push((range.end, false, i));
    }

    intervals.push((total_range.start, true, usize::MAX));
    intervals.push((total_range.end, false, usize::MAX));

    // Sort by position, with ends before starts at same position
    // This ensures we close ranges before opening new ones at the same position
    intervals.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    // Track significant intervals (where style ranges end) for merging decisions
    let mut significant_intervals: BTreeSet<usize> = BTreeSet::new();
    for (range, _) in &styles {
        significant_intervals.insert(range.end);
    }

    let mut result: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut active_styles: Vec<usize> = Vec::new();
    let mut last_pos = total_range.start;

    for (pos, is_start, style_idx) in intervals {
        // Skip total_range boundaries in active set management
        let is_boundary = style_idx == usize::MAX;

        if pos > last_pos {
            let interval = last_pos..pos;
            let combined_style = if active_styles.is_empty() {
                HighlightStyle::default()
            } else {
                let mut combined = HighlightStyle::default();
                for &idx in &active_styles {
                    merge_highlight_style(&mut combined, &styles[idx].1);
                }
                combined
            };
            result.push((interval, combined_style));
        }

        if !is_boundary {
            if is_start {
                active_styles.push(style_idx);
            } else {
                active_styles.retain(|&i| i != style_idx);
            }
        }

        last_pos = pos;
    }

    // Merge adjacent ranges with the same style, but not across significant boundaries
    let mut merged: Vec<(Range<usize>, HighlightStyle)> = Vec::with_capacity(result.len());
    for (range, style) in result {
        if let Some((last_range, last_style)) = merged.last_mut() {
            if last_range.end == range.start
                && *last_style == style
                && !significant_intervals.contains(&range.start)
            {
                // Merge adjacent ranges with same style, but not across significant boundaries
                last_range.end = range.end;
                continue;
            }
        }
        merged.push((range, style));
    }

    merged
}

/// Walk the tree and collect nodes suitable for querying, skipping subtrees
/// that fall entirely outside the byte range. Nodes much larger than the
/// query range are recursed into so that `QueryCursor` only visits the
/// relevant portion of the tree.
///
/// Returns the nodes to query whole, and the nodes gone into, outermost
/// first. A pattern on one of those, `(string_literal) @string` on a string
/// longer than the view or `(function_item (identifier) @function)` on a
/// long function, is found by no query of its children, and they are to be
/// queried at themselves alone.
///
/// The nodes still to look at are kept on a stack of their own, in the
/// order a call a level took them: tree-sitter's error recovery nests the
/// tree of a 1.4 MB paste of Rust with a key typed before it some 7,700
/// levels deep, each far larger than the view.
fn collect_query_nodes<'a>(
    root: tree_sitter::Node<'a>,
    range: &Range<usize>,
) -> (Vec<tree_sitter::Node<'a>>, Vec<tree_sitter::Node<'a>>) {
    let mut nodes = Vec::new();
    let mut gone_into = Vec::new();
    let mut to_look_at = vec![root];
    while let Some(node) = to_look_at.pop() {
        // Skip nodes entirely outside the range.
        if node.end_byte() <= range.start || node.start_byte() >= range.end {
            continue;
        }

        let node_span = node.end_byte() - node.start_byte();
        let range_span = range.end - range.start;
        if node_span <= range_span + LARGE_NODE_THRESHOLD || node.child_count() == 0 {
            nodes.push(node);
            continue;
        }

        // Use `goto_first_child_for_byte` to seek directly to the first
        // overlapping child instead of iterating all children from the start.
        gone_into.push(node);
        let first = to_look_at.len();
        let mut cursor = node.walk();
        if cursor.goto_first_child_for_byte(range.start).is_some() {
            loop {
                let child = cursor.node();
                if child.start_byte() >= range.end {
                    break;
                }
                to_look_at.push(child);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        to_look_at[first..].reverse();
    }
    if nodes.is_empty() {
        // the root, whole, has every pattern the nodes gone into have
        return (vec![root], Vec::new());
    }
    (nodes, gone_into)
}

/// Merge other style (Other on top)
fn merge_highlight_style(style: &mut HighlightStyle, other: &HighlightStyle) {
    if let Some(color) = other.color {
        style.color = Some(color);
    }
    if let Some(font_weight) = other.font_weight {
        style.font_weight = Some(font_weight);
    }
    if let Some(font_style) = other.font_style {
        style.font_style = Some(font_style);
    }
    if let Some(background_color) = other.background_color {
        style.background_color = Some(background_color);
    }
    if let Some(underline) = other.underline {
        style.underline = Some(underline);
    }
    if let Some(strikethrough) = other.strikethrough {
        style.strikethrough = Some(strikethrough);
    }
    if let Some(fade_out) = other.fade_out {
        style.fade_out = Some(fade_out);
    }
}

#[cfg(test)]
pub(super) mod tests {
    use gpui::Hsla;

    use super::*;
    use crate::Colorize as _;

    fn color_style(color: Hsla) -> HighlightStyle {
        let mut style = HighlightStyle::default();
        style.color = Some(color);
        style
    }

    /// The spans of target `gpui::perf` a subscriber is handed, as
    /// `name field=value…` once each has closed, in the order they closed.
    struct PerfSpans {
        next: std::sync::atomic::AtomicU64,
        open: std::sync::Mutex<HashMap<u64, String>>,
        closed: Arc<std::sync::Mutex<Vec<String>>>,
    }

    struct Said<'a>(&'a mut String);

    impl tracing::field::Visit for Said<'_> {
        fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
            self.0.push_str(&format!(" {}={v}", f.name()));
        }

        fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
            self.0.push_str(&format!(" {}={v:?}", f.name()));
        }
    }

    impl tracing::Subscriber for PerfSpans {
        fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
            m.target() == "gpui::perf"
        }

        fn new_span(&self, a: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            let mut said = a.metadata().name().to_string();
            a.record(&mut Said(&mut said));
            let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
            self.open.lock().unwrap().insert(id, said);
            tracing::span::Id::from_u64(id)
        }

        fn record(&self, id: &tracing::span::Id, v: &tracing::span::Record<'_>) {
            if let Some(said) = self.open.lock().unwrap().get_mut(&id.into_u64()) {
                v.record(&mut Said(said));
            }
        }

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}

        fn try_close(&self, id: tracing::span::Id) -> bool {
            if let Some(said) = self.open.lock().unwrap().remove(&id.into_u64()) {
                self.closed.lock().unwrap().push(said);
            }
            true
        }
    }

    /// What `PerfSpans` is handed, as long as it lives.
    pub(crate) struct WatchedPerfSpans {
        _default: tracing::subscriber::DefaultGuard,
        _other: tracing::Dispatch,
    }

    /// A subscriber of this thread's that is handed every span of target
    /// `gpui::perf`, and the spans it was handed once each has closed. A
    /// second dispatcher, so that a span's site another test's thread
    /// meets first is asked of this one too: with a scoped one alone,
    /// tracing asks the thread that meets it, and keeps its never.
    pub(crate) fn watch_perf_spans() -> (Arc<std::sync::Mutex<Vec<String>>>, WatchedPerfSpans) {
        let closed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let _other = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let _default = tracing::subscriber::set_default(PerfSpans {
            next: Default::default(),
            open: Default::default(),
            closed: closed.clone(),
        });
        (closed, WatchedPerfSpans { _default, _other })
    }

    #[test]
    fn test_plain_text_never_parses() {
        // "text" has no grammar, the highlighter shouldn't parse.
        let mut highlighter = SyntaxHighlighter::new("text");
        let rope = Rope::from("hello {\"a\": 1}\nworld");
        assert!(highlighter.update(None, &rope, None));
        assert!(highlighter.tree().is_none());
        assert_eq!(highlighter.text().to_string(), rope.to_string());

        let theme = HighlightTheme::default_dark();
        let styles = highlighter.styles(&(0..rope.len()), theme.as_ref());
        assert_eq!(styles, vec![(0..rope.len(), HighlightStyle::default())]);

        // Unregistered languages fall back to plain text.
        let mut highlighter = SyntaxHighlighter::new("no-such-language");
        assert!(highlighter.update(None, &rope, None));
        assert!(highlighter.tree().is_none());
    }

    /// While a background reparse is pending (sync-parse timeout, or the
    /// large-text `edit_tree` path), `styles()` serves ranges from a stale
    /// tree. Those must still land on char boundaries of the current text,
    /// or text shaping panics on multi-byte characters.
    #[cfg(feature = "tree-sitter-languages")]
    #[test]
    fn test_stale_tree_styles_snap_to_char_boundaries() {
        let mut highlighter = SyntaxHighlighter::new("markdown");
        let old = Rope::from("# hello world\n*emphasis* and `code` here\n");
        assert!(highlighter.update(None, &old, None));
        assert!(highlighter.tree().is_some());

        // Swap the text without reparsing: the tree is now stale and its node
        // offsets point into the middle of the new text's CJK characters.
        let new = Rope::from("# 你好，世界\n你好，*世界* 与 `代码`\n");
        highlighter.edit_tree(None, &new);

        let theme = HighlightTheme::default_dark();
        let styles = highlighter.styles(&(0..new.len()), theme.as_ref());
        for (range, _) in &styles {
            assert!(
                new.is_char_boundary(range.start) && new.is_char_boundary(range.end),
                "style range {range:?} is not on char boundaries of the current text"
            );
        }
    }

    #[cfg(feature = "tree-sitter-languages")]
    fn has_highlight_covering(
        highlights: &[HighlightItem],
        source: &str,
        text: &str,
        highlight_name: &str,
    ) -> bool {
        let start = source.find(text).expect("text should exist in source");
        let end = start + text.len();
        highlights.iter().any(|item| {
            item.name.as_ref() == highlight_name
                && item.range.start <= start
                && item.range.end >= end
        })
    }

    /// Replace `range` of `text` with `new_text`, as the editor does on a
    /// keystroke: the edit it gives the highlighter, and the text after it.
    #[cfg(feature = "tree-sitter-languages")]
    fn replace(text: &str, range: Range<usize>, new_text: &str) -> (InputEdit, String) {
        fn point(text: &str, offset: usize) -> Point {
            let before = &text[..offset];
            let column = offset - before.rfind('\n').map_or(0, |newline| newline + 1);
            Point::new(before.matches('\n').count(), column)
        }

        let edited = format!("{}{new_text}{}", &text[..range.start], &text[range.end..]);
        let new_end = range.start + new_text.len();
        let edit = InputEdit {
            start_byte: range.start,
            old_end_byte: range.end,
            new_end_byte: new_end,
            start_position: point(text, range.start),
            old_end_position: point(text, range.end),
            new_end_position: point(&edited, new_end),
        };
        (edit, edited)
    }

    #[track_caller]
    fn assert_unique_styles(
        range: Range<usize>,
        left: Vec<(Range<usize>, HighlightStyle)>,
        right: Vec<(Range<usize>, HighlightStyle)>,
    ) {
        fn color_name(c: Option<Hsla>) -> String {
            match c {
                Some(c) => {
                    if c == gpui::red() {
                        "red".to_string()
                    } else if c == gpui::green() {
                        "green".to_string()
                    } else if c == gpui::blue() {
                        "blue".to_string()
                    } else {
                        c.to_hex()
                    }
                }
                None => "clean".to_string(),
            }
        }

        let left = unique_styles(&range, left);
        if left.len() != right.len() {
            println!("\n---------------------------------------------");
            for (range, style) in left.iter() {
                println!("({:?}, {})", range, color_name(style.color));
            }
            println!("---------------------------------------------");
            panic!("left {} styles, right {} styles", left.len(), right.len());
        }
        for (left, right) in left.into_iter().zip(right) {
            if left.1.color != right.1.color || left.0 != right.0 {
                panic!(
                    "\n left: ({:?}, {})\nright: ({:?}, {})\n",
                    left.0,
                    color_name(left.1.color),
                    right.0,
                    color_name(right.1.color)
                );
            }
        }
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_take_parse_takes_every_tree() {
        let html = "<style>\n.card { color: #336699; }\n</style>\n<p>hi</p>\n";
        let rope = Rope::from_str(html);
        let mut highlighter = SyntaxHighlighter::new("html");
        assert!(highlighter.update(None, &rope, None));
        assert!(highlighter.tree().is_some());
        let (parse, bytes) = highlighter.take_parse();
        assert_eq!(bytes, html.len());
        assert!(parse.tree.is_some());
        assert_eq!(parse.injection_layers.len(), 1, "the style's CSS");
        assert!(highlighter.tree().is_none());
        assert!(
            highlighter.take_parse().0.is_empty(),
            "nothing left to take"
        );
        fn is_send<T: Send>(_: &T) {}
        is_send(&parse);
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_html_style_injects_css_highlights() {
        let html = r#"<style>
.card { color: #336699; }
</style>
"#;

        let rope = Rope::from_str(html);
        let mut highlighter = SyntaxHighlighter::new("html");
        highlighter.update(None, &rope, None);

        let highlights = highlighter.match_styles(0..html.len());

        assert!(
            has_highlight_covering(&highlights, html, "color", "property"),
            "CSS property names inside style elements should be highlighted"
        );
        assert!(
            has_highlight_covering(&highlights, html, "#336699", "string.special"),
            "CSS color values inside style elements should be highlighted"
        );
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_html_script_injects_javascript_highlights() {
        let html = r#"<script>
const answer = 42;
console.log(answer);
</script>
"#;

        let rope = Rope::from_str(html);
        let mut highlighter = SyntaxHighlighter::new("html");
        highlighter.update(None, &rope, None);

        let highlights = highlighter.match_styles(0..html.len());

        assert!(
            has_highlight_covering(&highlights, html, "const", "keyword"),
            "JavaScript keywords inside script elements should be highlighted"
        );
        assert!(
            has_highlight_covering(&highlights, html, "answer", "variable"),
            "JavaScript identifiers inside script elements should be highlighted"
        );
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_injection_overtyped_with_the_same_length_is_highlighted_again() {
        // Overtyping a character leaves every injected range where it was, so
        // the span's old tree is reused, and it must have been told of the
        // edit or it describes the text before it.
        for (language, source) in [
            (
                "html",
                "<script>\nconst answer = 42;\nlet total = 21;\n</script>\n",
            ),
            (
                "markdown",
                "```js\nconst answer = 42;\nlet total = 21;\n```\n",
            ),
        ] {
            let mut highlighter = SyntaxHighlighter::new(language);
            assert!(highlighter.update(None, &Rope::from_str(source), None));
            let highlights = highlighter.match_styles(0..source.len());
            assert!(has_highlight_covering(&highlights, source, "42", "number"));
            assert!(has_highlight_covering(
                &highlights,
                source,
                "let",
                "keyword"
            ));

            let at = source.find("42").unwrap();
            let (edit, source) = replace(source, at..at + 1, "x");
            assert!(highlighter.update(Some(edit), &Rope::from_str(&source), None));
            let at = source.find("let").unwrap();
            let (edit, source) = replace(&source, at..at + 1, "m");
            assert!(highlighter.update(Some(edit), &Rope::from_str(&source), None));

            let highlights = highlighter.match_styles(0..source.len());
            assert!(
                has_highlight_covering(&highlights, &source, "x2", "variable")
                    && !has_highlight_covering(&highlights, &source, "x2", "number"),
                "{language}: `42` overtyped to `x2` should be a variable, not a number"
            );
            assert!(
                !has_highlight_covering(&highlights, &source, "met", "keyword"),
                "{language}: `let` overtyped to `met` should not be a keyword"
            );
        }
    }

    /// A text made anew keeps the old text's colours until it is parsed,
    /// as VS Code and IntelliJ keep them, where it stood plain until the
    /// parse after the paste (elundqvist/kvist#401): what the two texts
    /// share at their start and their end coloured as it was, where it is
    /// now, what lies between plain; where that is too much to move the
    /// trees over, only what they share at their start is coloured, the
    /// old colours over shifted text giving plain words a keyword's. The
    /// trees are no start for a parse of the new text, and say nothing of
    /// its folds, and the parse that follows colours it as a fresh one
    /// does, and is of the text, its folds found and a key's parse
    /// starting from it.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_text_made_anew_keeps_the_old_colours_until_its_parse() {
        let function = |i: usize, v: &str| format!("fn f{i}() {{\n    let {v} = \"s{i}\";\n}}\n");
        let old: String = (0..3000).map(|i| function(i, "v")).collect();
        let named = |highlighter: &SyntaxHighlighter, text: &str, word: &str, name: &str| {
            has_highlight_covering(&highlighter.match_styles(0..text.len()), text, word, name)
        };
        let parsed = |source: &str| {
            let mut highlighter = SyntaxHighlighter::new("rust");
            assert!(highlighter.update(None, &Rope::from_str(source), None));
            highlighter
        };
        // a function in the middle renamed and given a line more
        let middle = function(1000, "v");
        let new = old.replacen(&middle, "fn changed() {\n    let w = 1;\n    let x = 2;\n}\n", 1);
        let mut highlighter = parsed(&old);
        highlighter.keep_colours_through(&Rope::from_str(&new));
        assert!(highlighter.tree_of_text().is_none(), "the old tree is not of the text");
        assert!(named(&highlighter, &new, "\"s3\"", "string"), "the start keeps its colours");
        assert!(named(&highlighter, &new, "\"s2999\"", "string"), "the end too, where it is now");
        assert!(!named(&highlighter, &new, "changed", "function"), "what went in is plain");
        let styles = highlighter.match_styles(0..new.len());
        let at = new.find("let x").unwrap();
        assert!(
            !styles.iter().any(|item| item.range.start < at + 5 && at < item.range.end),
            "what lies between is plain"
        );
        assert!(highlighter.injection_parse_data().is_none_or(|data| data.old_layers.is_empty()));
        // a key whose parse, from nothing, runs out of its time moves the
        // kept trees with it
        let mut keyed = parsed(&old);
        keyed.keep_colours_through(&Rope::from_str(&new));
        let (edit, typed) = replace(&new, 0..0, "// a comment\n");
        assert!(!keyed.update(Some(edit), &Rope::from_str(&typed), Some(Duration::ZERO)));
        assert!(keyed.tree_of_text().is_none());
        assert!(named(&keyed, &typed, "\"s3\"", "string"), "where it is now");
        assert!(named(&keyed, &typed, "\"s2999\"", "string"));
        // a parse of it starts afresh, and colours it as a fresh one does
        assert!(highlighter.update(None, &Rope::from_str(&new), None));
        assert!(highlighter.tree_of_text().is_some());
        assert_eq!(
            highlighter.tree().unwrap().root_node().to_sexp(),
            parsed(&new).tree().unwrap().root_node().to_sexp()
        );
        assert!(named(&highlighter, &new, "changed", "function"));

        // a word made longer in every function from the 100th on, a
        // difference of more than the trees are moved over: what the two
        // share at their start keeps its colours, and the rest, where the
        // old tokens would fall on other text, is plain
        let other: String = (0..3000).map(|i| function(i, if i < 100 { "v" } else { "value" })).collect();
        let head = old.find(&function(100, "v")).unwrap() + "fn f100() {\n    let v".len();
        assert!(other.len() - head - " = \"s2999\";\n}\n".len() > KEEP_ANEW_EDIT_BYTES);
        let mut highlighter = parsed(&old);
        highlighter.keep_colours_through(&Rope::from_str(&other));
        assert!(named(&highlighter, &other, "\"s3\"", "string"), "the start keeps its colours");
        let styles = highlighter.match_styles(0..other.len());
        assert!(styles.iter().all(|item| item.range.end <= head), "nothing past what the two share");
        let theme = HighlightTheme::default_dark();
        let coloured_past = |highlighter: &SyntaxHighlighter, text: &str, at: usize| {
            highlighter
                .styles(&(0..text.len()), theme.as_ref())
                .iter()
                .any(|(range, style)| range.end > at && style.color.is_some())
        };
        assert!(!coloured_past(&highlighter, &other, head));
        // a key typed before it moves where the colours end, and one after
        // it leaves it
        let (edit, typed) = replace(&other, 0..0, "x");
        highlighter.edit_tree(Some(edit), &Rope::from_str(&typed));
        assert!(named(&highlighter, &typed, "\"s50\"", "string"));
        assert!(!coloured_past(&highlighter, &typed, head + 1));
        let at = typed.find("\"s2000\"").unwrap();
        let (edit, typed) = replace(&typed, at..at, "y");
        highlighter.edit_tree(Some(edit), &Rope::from_str(&typed));
        assert!(named(&highlighter, &typed, "\"s50\"", "string"));
        assert!(!coloured_past(&highlighter, &typed, head + 1));
        // the parse off the UI thread lands: the trees are of the text, and
        // the old ones are handed back to be freed elsewhere
        let fresh = parsed(&typed);
        let rope = Rope::from_str(&typed);
        let replaced = highlighter.apply_background_tree(fresh.tree().unwrap().clone(), &rope, Vec::new());
        assert!(replaced.is_some_and(|parse| !parse.is_empty()), "the old text's trees");
        assert!(highlighter.tree_of_text().is_some(), "the trees are of the text");
        assert_eq!(highlight_names(&highlighter, &typed), highlight_names(&fresh, &typed));
        let (edit, typed) = replace(&typed, 0..0, "z");
        assert!(highlighter.update(Some(edit), &Rope::from_str(&typed), None));
        assert!(
            highlighter.apply_background_tree(highlighter.tree().unwrap().clone(), &Rope::from_str(&typed), Vec::new()).is_none(),
            "trees of a key are freed where they are"
        );

        // and a shorter one: the trees reach past its end, and nothing
        // past it is coloured
        let shorter = &other[..other.len() / 3];
        let mut highlighter = parsed(&old);
        highlighter.keep_colours_through(&Rope::from_str(shorter));
        let styles = highlighter.styles(&(0..shorter.len()), theme.as_ref());
        assert!(styles.iter().all(|(range, _)| range.end <= shorter.len()));
        assert!(styles.iter().any(|(_, style)| style.color.is_some()));
        // and one of the same length, which differs all over: the old
        // tokens stand where the two differ, and are not coloured there
        let same_length = old.replace("let v", "let w");
        let mut highlighter = parsed(&old);
        highlighter.keep_colours_through(&Rope::from_str(&same_length));
        let head = old.find("let v").unwrap() + "let ".len();
        assert!(!coloured_past(&highlighter, &same_length, head));
    }

    /// The steps of a parse of `text` from `old`, edited by a letter put
    /// in at the middle line's start: the parser's calls to its progress
    /// callback, made every so many steps of a parse.
    #[cfg(feature = "tree-sitter-languages")]
    pub(crate) fn key_steps(parser: &mut Parser, old: &Tree, text: &str) -> usize {
        let row = text.lines().count() / 2;
        let at = text.split_inclusive('\n').take(row).map(str::len).sum::<usize>();
        let mut old = old.clone();
        let point = Point::new(row, 0);
        old.edit(&InputEdit {
            start_byte: at,
            old_end_byte: at,
            new_end_byte: at + 1,
            start_position: point,
            old_end_position: point,
            new_end_position: Point::new(row, 1),
        });
        let typed = format!("{}x{}", &text[..at], &text[at..]);
        let mut calls = 0usize;
        let mut count = |_: &tree_sitter::ParseState| {
            calls += 1;
            ControlFlow::Continue(())
        };
        let options = ParseOptions::new().progress_callback(&mut count);
        parser
            .parse_with_options(&mut |at, _| &typed.as_bytes()[at.min(typed.len())..], Some(&old), Some(options))
            .unwrap();
        calls
    }

    /// A README's shape: front matter, then `parts` parts of ten
    /// paragraphs under a heading, each closed by a fence.
    #[cfg(feature = "tree-sitter-languages")]
    pub(crate) fn readme(parts: usize) -> String {
        let mut markdown = String::from("---\ntitle: A README\ntags: [editor, markdown]\n---\n\n# A README\n\n");
        for part in 0..parts {
            markdown += &format!("## Part {part}\n\n");
            for i in 0..10 {
                markdown += &format!("Paragraph {i} says *something* about [the design](a.md#p-{i}),\nover two lines.\n\n");
            }
            markdown += &format!("```rust\nfn f{part}() -> u32 {{\n    {part}\n}}\n```\n\n");
        }
        markdown
    }

    /// A Markdown tree made from nothing is not reused by the parse after
    /// an edit, which builds all of it again; parsed again from itself,
    /// edited at its start (`edited_at_its_start`), it is, and that parse
    /// does a small part of the work, as a Rust tree's does without it.
    /// The work is counted in the parser's calls to its progress callback,
    /// made every so many steps of a parse. The first key after a README
    /// opened ran out of its 2 ms, the whole of it parsed again
    /// (elundqvist/kvist#376), and so did the first key in a README with
    /// no front matter, which a parse again from itself unedited reused
    /// whole and left as it was: 405 steps where the keys after it take 13
    /// (elundqvist/kvist#593).
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_markdown_tree_made_from_nothing_is_parsed_again_from_itself() {
        let markdown = readme(30);
        let no_front_matter = without_front_matter(&markdown);
        let rust: String = (0..300)
            .map(|i| format!("fn f{i}(x: u32) -> u32 {{\n    x * {i}\n}}\n\n"))
            .collect();
        let steps = key_steps;
        for (language, text, again) in [
            ("markdown", &markdown, true),
            ("markdown", &no_front_matter, true),
            ("rust", &rust, false),
        ] {
            let config = LanguageRegistry::singleton().language(language).unwrap();
            let grammar = config.language.as_ref().unwrap();
            assert_eq!(parsed_again_from_itself(grammar), again, "{language}");
            let mut parser = Parser::new();
            parser.set_language(grammar).unwrap();
            let fresh = parser.parse(text, None).unwrap();
            let from_fresh = steps(&mut parser, &fresh, text);
            let again_tree = parser
                .parse(text, Some(&edited_at_its_start(&fresh)))
                .unwrap();
            assert_eq!(
                again_tree.root_node().to_sexp(),
                fresh.root_node().to_sexp(),
                "{language}"
            );
            let from_again = steps(&mut parser, &again_tree, text);
            if again {
                assert!(
                    from_again * 10 < from_fresh,
                    "{language}: {from_again} steps from the tree parsed again, {from_fresh} from the fresh one"
                );
            } else {
                assert!(
                    from_fresh <= from_again + 2,
                    "{language}: {from_fresh} steps from the fresh tree, {from_again} from one parsed again"
                );
            }
        }
        // and the highlighter's own whole parse leaves such a tree, of a
        // README with front matter and of one without
        let config = LanguageRegistry::singleton().language("markdown").unwrap();
        let mut parser = Parser::new();
        parser
            .set_language(config.language.as_ref().unwrap())
            .unwrap();
        for text in [&markdown, &no_front_matter] {
            let mut highlighter = SyntaxHighlighter::new("markdown");
            assert!(highlighter.update(None, &Rope::from_str(text), None));
            let fresh = parser.parse(text, None).unwrap();
            let kept = highlighter.tree().unwrap().clone();
            let (from_kept, from_fresh) = (
                steps(&mut parser, &kept, text),
                steps(&mut parser, &fresh, text),
            );
            assert!(
                from_kept * 10 < from_fresh,
                "{from_kept} steps from the highlighter's tree, {from_fresh} from a fresh one"
            );
        }
    }

    /// `readme`'s text from its first heading on, with no front matter.
    #[cfg(feature = "tree-sitter-languages")]
    pub(crate) fn without_front_matter(readme: &str) -> String {
        readme
            .split_once("---\n\n")
            .map_or(readme, |(_, rest)| rest)
            .to_string()
    }

    /// A language of a name of its own, `name`, that is Rust's grammar and
    /// highlights, and that no other test has met: nothing has compiled its
    /// queries.
    #[cfg(feature = "tree-sitter-languages")]
    pub(crate) fn register_rust_as(name: &str) {
        let rust = LanguageRegistry::singleton().language("rust").unwrap();
        let config = LanguageConfig::new(
            name,
            rust.language.clone().unwrap(),
            vec![],
            &rust.highlights,
            "",
            "",
        );
        LanguageRegistry::singleton().register(name, &config);
    }

    /// Wait for the thread that compiles `name`'s highlight query as an
    /// injected language (`queries::compile_in_background`).
    #[cfg(feature = "tree-sitter-languages")]
    pub(crate) fn wait_until_compiled(name: &str) {
        let config = LanguageRegistry::singleton().language(name).unwrap();
        let began = Instant::now();
        while queries::injected_if_compiled(&config).is_none() {
            assert!(began.elapsed() < Duration::from_secs(60), "{name} was not compiled");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[cfg(feature = "tree-sitter-languages")]
    fn layer_languages(highlighter: &SyntaxHighlighter) -> Vec<String> {
        highlighter
            .injection_layers
            .iter()
            .map(|layer| layer.language_name.to_string())
            .collect()
    }

    /// A fence of a language no one has compiled the highlight query of is
    /// left to the parse after the pause by an update with a time limit,
    /// whose thread compiles the query: compiled in the update, a Rust
    /// fence took 12 ms of the drawing thread and a Kotlin one 22, the
    /// open of a Markdown file with three fences 18 and the first key into
    /// a new Rust fence 12 to 15 (elundqvist/kvist#593). The paragraphs'
    /// language, every document's, is not left; and once compiled, the
    /// query is every highlighter's, the fence parsed in the first update
    /// of one that is made after.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_fence_of_a_language_no_one_has_compiled_is_left_to_the_parse_after_the_pause() {
        let name = "kvist-test-cold-fence";
        register_rust_as(name);
        let text = format!(
            "# Title\n\nA paragraph with *emphasis*.\n\n```{name}\nfn f() {{}}\n```\n"
        );
        let rope = Rope::from_str(&text);
        let limit = Some(Duration::from_secs(60));
        let mut highlighter = SyntaxHighlighter::new("markdown");
        highlighter.defer_cold_queries();
        assert!(!highlighter.update(None, &rope, limit));
        assert!(!highlighter.layers_whole, "the fence is not parsed");
        let layers = layer_languages(&highlighter);
        assert!(layers.iter().any(|name| name == "markdown_inline"), "{layers:?}");
        assert!(!layers.iter().any(|layer| layer == name), "{layers:?}");

        // the parse after the pause, off the drawing thread, has it
        // compiled by then or compiles it itself
        let data = InjectionParseData {
            parse_timeout: None,
            ..highlighter.injection_parse_data().unwrap()
        };
        let tree = highlighter.tree().unwrap().clone();
        let (found, finished) = SyntaxHighlighter::compute_injection_layers(data, &tree, &rope);
        assert!(finished);
        assert!(found.iter().any(|layer| layer.language_name == name));

        // and every highlighter after it finds it
        wait_until_compiled(name);
        let mut next = SyntaxHighlighter::new("markdown");
        next.defer_cold_queries();
        next.update(None, &rope, limit);
        assert!(layer_languages(&next).iter().any(|layer| layer == name));
        assert!(next.layers_whole);
    }

    /// A caller with no parse after the pause, and the parse that is one,
    /// compile the queries they need where they are.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_parse_with_no_pause_after_it_compiles_the_queries_it_needs() {
        let name = "kvist-test-compiled-here";
        register_rust_as(name);
        let text = format!("# Title\n\n```{name}\nfn f() {{}}\n```\n");
        let mut highlighter = SyntaxHighlighter::new("markdown");
        highlighter.update(None, &Rope::from_str(&text), Some(Duration::from_secs(60)));
        assert!(layer_languages(&highlighter).iter().any(|layer| layer == name));
        assert!(highlighter.layers_whole);
    }

    /// The queries of a language are compiled once for all its
    /// highlighters, and again where the language is registered again
    /// under its name with other sources.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_the_queries_of_a_language_are_made_once_for_every_highlighter() {
        let name = "kvist-test-queries";
        let rust = LanguageRegistry::singleton().language("rust").unwrap();
        let grammar = rust.language.clone().unwrap();
        let register = |highlights: &str| {
            LanguageRegistry::singleton()
                .register(name, &LanguageConfig::new(name, grammar.clone(), vec![], highlights, "", ""));
        };
        register("(identifier) @variable");
        let (first, second) = (SyntaxHighlighter::new(name), SyntaxHighlighter::new(name));
        let (first, second) = (first.query.unwrap(), second.query.unwrap());
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.pattern_count(), 1);
        register("(identifier) @variable\n(type_identifier) @type");
        let third = SyntaxHighlighter::new(name).query.unwrap();
        assert!(!Arc::ptr_eq(&first, &third));
        assert_eq!(third.pattern_count(), 2);
        // as one registered under another name is not
        let again = SyntaxHighlighter::new("rust").query.unwrap();
        assert!(!Arc::ptr_eq(&again, &third));
    }

    /// A text handed again while its parse is the pause's, a parse that ran
    /// out of its time or a tree to be parsed again, is the pause's still:
    /// said done, it cancelled the parse after the pause, and the text
    /// stood without a tree, or with one the first key would build again.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_text_handed_again_while_its_parse_is_the_pauses_is_not_done() {
        let rope = Rope::from_str(&readme(60));
        let mut highlighter = SyntaxHighlighter::new("markdown");
        assert!(!highlighter.update(None, &rope, Some(Duration::ZERO)), "out of its time");
        assert!(highlighter.tree().is_none());
        assert!(!highlighter.update(None, &rope, Some(Duration::ZERO)), "the pause's still");

        let mut highlighter = SyntaxHighlighter::new("markdown");
        assert!(!highlighter.update(None, &rope, Some(Duration::from_secs(60))));
        assert!(highlighter.again_pending());
        assert!(!highlighter.update(None, &rope, Some(Duration::from_secs(60))), "the pause's still");

        // parsed, it is done
        let mut highlighter = SyntaxHighlighter::new("markdown");
        assert!(highlighter.update(None, &rope, None));
        assert!(highlighter.update(None, &rope, Some(Duration::ZERO)));
    }

    /// A Markdown tree made from nothing at the key, where the parse again
    /// from itself would rebuild every node of the text on the drawing
    /// thread, is left to the parse after the pause, which makes the tree
    /// the next key's parse reuses: parsed again on the drawing thread it
    /// ran out of its time, and where it did, was never parsed again
    /// (elundqvist/kvist#593).
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_markdown_tree_made_at_the_key_is_parsed_again_by_the_parse_after_the_pause() {
        let text = without_front_matter(&readme(30));
        let rope = Rope::from_str(&text);
        let config = LanguageRegistry::singleton().language("markdown").unwrap();
        let mut parser = Parser::new();
        parser.set_language(config.language.as_ref().unwrap()).unwrap();
        let fresh = parser.parse(&text, None).unwrap();
        let from_fresh = key_steps(&mut parser, &fresh, &text);
        let limit = Some(Duration::from_secs(60));

        let mut highlighter = SyntaxHighlighter::new("markdown");
        assert!(!highlighter.update(None, &rope, limit), "left to the parse after the pause");
        assert!(highlighter.again_pending());
        let kept = highlighter.tree().unwrap().clone();
        let from_kept = key_steps(&mut parser, &kept, &text);
        assert!(from_kept * 2 > from_fresh, "{from_kept} steps from the tree, {from_fresh} from a fresh one");

        // the parse after the pause, as the editor's makes it
        let again = parser
            .parse(&text, Some(&edited_at_its_start(highlighter.tree_of_text().unwrap())))
            .unwrap();
        assert!(highlighter.apply_background_tree(again, &rope, Vec::new()).is_none());
        assert!(!highlighter.again_pending());
        let again = highlighter.tree().unwrap().clone();
        let from_again = key_steps(&mut parser, &again, &text);
        assert!(from_again * 10 < from_fresh, "{from_again} steps, {from_fresh} from a fresh tree");

        // a key's parse before the pause parses from it as it is, and
        // what it makes is a tree the next key reuses
        let mut highlighter = SyntaxHighlighter::new("markdown");
        highlighter.update(None, &rope, limit);
        let at = text.find("Paragraph 3 says").unwrap();
        let mut typed = text.clone();
        typed.insert(at, 'x');
        let point = |text: &str, at: usize| {
            let row = text[..at].matches('\n').count();
            Point::new(row, at - text[..at].rfind('\n').map_or(0, |n| n + 1))
        };
        let edit = InputEdit {
            start_byte: at,
            old_end_byte: at,
            new_end_byte: at + 1,
            start_position: point(&text, at),
            old_end_position: point(&text, at),
            new_end_position: point(&typed, at + 1),
        };
        highlighter.update(Some(edit), &Rope::from_str(&typed), limit);
        assert!(!highlighter.again_pending());
        let after_a_key = highlighter.tree().unwrap().clone();
        assert!(key_steps(&mut parser, &after_a_key, &typed) * 10 < from_fresh);

        // with no time limit, there is no parse after the pause
        let mut highlighter = SyntaxHighlighter::new("markdown");
        assert!(highlighter.update(None, &rope, None));
        assert!(!highlighter.again_pending());
    }

    /// A key typed after a whole parse that ran out of its time, before
    /// the parse off the UI thread lands, finds no tree, and is parsed
    /// from nothing as the whole parse was: the tree of the empty text,
    /// edited by the key alone as if it were of the text before the key,
    /// was taken as a start, and the parse from it finished at once with a
    /// tree of next to nothing, which every key after it built on. A README
    /// typed into within a moment of opening stayed plain until it was
    /// opened again (elundqvist/kvist#376).
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_key_after_a_whole_parse_out_of_time_is_parsed_from_nothing() {
        let rust: String = (0..2000).map(|i| format!("fn f{i}(x: u32) -> u32 {{\n    x * {i}\n}}\n\n")).collect();
        let empty = InputEdit {
            start_byte: 0,
            old_end_byte: 0,
            new_end_byte: 0,
            start_position: Point::new(0, 0),
            old_end_position: Point::new(0, 0),
            new_end_position: Point::new(0, 0),
        };
        for (language, text, at) in [("markdown", readme(30), "Paragraph 3 says"), ("rust", rust, "fn f30(")] {
            let at = text.find(at).unwrap();
            let (edit, typed) = replace(&text, at..at, "x");
            let fresh = fresh_highlighter(language, &typed);
            // opened: the editor hands the text with an empty edit at its
            // start, or with none, and the parse runs out of its time
            for open in [Some(empty), None] {
                let mut highlighter = SyntaxHighlighter::new(language);
                assert!(!highlighter.update(open, &Rope::from_str(&text), Some(Duration::ZERO)), "{language}");
                assert!(highlighter.tree().is_none(), "{language}");
                // the key with all the time it needs, as the parse after
                // the pause has it, its spans too (`fresh_highlighter`)
                highlighter.injection_parse_timeout = Duration::MAX;
                assert!(highlighter.update(Some(edit), &Rope::from_str(&typed), None));
                assert_eq!(
                    highlighter.tree().unwrap().root_node().to_sexp(),
                    fresh.tree().unwrap().root_node().to_sexp(),
                    "{language}, opened with {open:?}"
                );
                assert_eq!(highlight_names(&highlighter, &typed), highlight_names(&fresh, &typed), "{language}");
            }
        }
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_injection_layers_move_with_an_edit_before_it_is_parsed() {
        // While a parse is pending (text too large to parse on the keystroke,
        // or a parse that ran out of time) styles come from the last layers,
        // which must follow the text rather than stay at their old offsets.
        let source = "<h1>Title</h1>\n<script>\nconst answer = 42;\n</script>\n";
        let mut highlighter = SyntaxHighlighter::new("html");
        assert!(highlighter.update(None, &Rope::from_str(source), None));

        let at = source.find("Title").unwrap();
        let (edit, source) = replace(source, at..at, "A longer ");
        highlighter.edit_tree(Some(edit), &Rope::from_str(&source));

        let highlights = highlighter.match_styles(0..source.len());
        assert!(has_highlight_covering(
            &highlights,
            &source,
            "const",
            "keyword"
        ));
        assert!(has_highlight_covering(
            &highlights,
            &source,
            "answer",
            "variable"
        ));
        assert!(has_highlight_covering(&highlights, &source, "42", "number"));
    }

    #[cfg(feature = "tree-sitter-languages")]
    fn statements(range: Range<usize>) -> String {
        range.map(|i| format!("let n{i} = {i};\n")).collect()
    }

    /// Apply `edit` as a keystroke that lands in a span too large to parse
    /// again in the time it has: the main tree has all the time it needs,
    /// and the injected spans have none. Returns what `update` does.
    #[cfg(feature = "tree-sitter-languages")]
    fn update_out_of_time(
        highlighter: &mut SyntaxHighlighter,
        edit: InputEdit,
        source: &str,
    ) -> bool {
        highlighter.injection_parse_timeout = Duration::ZERO;
        let text = Rope::from_str(source);
        let finished = highlighter.update(Some(edit), &text, Some(Duration::MAX));
        highlighter.injection_parse_timeout = INJECTION_PARSE_TIMEOUT;
        finished
    }

    /// Parse the injected spans again with as long as they take, as the
    /// editor's background parse does. Returns whether every one finished.
    #[cfg(feature = "tree-sitter-languages")]
    fn parse_injections_with_time(highlighter: &mut SyntaxHighlighter) -> bool {
        let tree = highlighter.tree().unwrap().clone();
        let text = highlighter.text().clone();
        let data = InjectionParseData {
            parse_timeout: None,
            ..highlighter.injection_parse_data().unwrap()
        };
        let (layers, finished) = SyntaxHighlighter::compute_injection_layers(data, &tree, &text);
        highlighter.apply_background_tree(tree, &text, layers);
        finished
    }

    /// A highlighter with `source` parsed whole, its injected spans with as
    /// long as they take, as the editor has it once its background parse is
    /// in. The spans had the 20 ms a keystroke gives them, and on a loaded
    /// machine a fence of 3,000 lines was not parsed in that: the test that
    /// edits it found no fence to keep its colour (elundqvist/kvist#201).
    #[cfg(feature = "tree-sitter-languages")]
    pub(crate) fn fresh_highlighter(language: &str, source: &str) -> SyntaxHighlighter {
        let mut highlighter = SyntaxHighlighter::new(language);
        highlighter.injection_parse_timeout = Duration::MAX;
        assert!(highlighter.update(None, &Rope::from_str(source), None));
        highlighter.injection_parse_timeout = INJECTION_PARSE_TIMEOUT;
        highlighter
    }

    #[cfg(feature = "tree-sitter-languages")]
    fn highlight_names(
        highlighter: &SyntaxHighlighter,
        source: &str,
    ) -> Vec<(Range<usize>, SharedString)> {
        highlighter
            .match_styles(0..source.len())
            .into_iter()
            .map(|item| (item.range, item.name))
            .collect()
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_injection_layer_out_of_time_keeps_its_last_highlights() {
        let source = format!(
            "<p>hi</p>\n<script>\nconst first = 1;\n{}const last = 2;\n</script>\n",
            statements(0..50)
        );

        // A hundred statements pasted into the middle of the script, and at
        // its very start, where the span's new ranges match none of its old
        // ones exactly. Either is far more than the parser gets through before
        // it first asks whether time is up, and with no time at all it is.
        for at in [
            source.find("const last").unwrap(),
            source.find("\nconst first").unwrap(),
        ] {
            let mut highlighter = SyntaxHighlighter::new("html");
            assert!(highlighter.update(None, &Rope::from_str(&source), None));

            let (edit, source) = replace(&source, at..at, &statements(50..150));
            assert!(
                !update_out_of_time(&mut highlighter, edit, &source),
                "a span out of time should ask for another parse"
            );
            assert!(
                highlighter
                    .injection_layers
                    .iter()
                    .any(|layer| layer.language_name.as_ref() == "javascript"),
                "a script whose parse runs out of time should keep its layer"
            );

            let highlights = highlighter.match_styles(0..source.len());
            for name in ["first", "last"] {
                assert!(
                    has_highlight_covering(&highlights, &source, name, "variable"),
                    "{name:?} should keep its highlight, moved with the text"
                );
            }
        }
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_injection_layer_out_of_time_leaves_what_an_edit_touched_plain() {
        // An edit grows the token it lands in, or the one it follows, over
        // everything it inserts. A layer out of time must not colour a
        // hundred pasted lines as the one string or comment they went into.
        let source = format!(
            "<p>hi</p>\n<script>\nconst first = 1;\nconst label = \"a string\";\n// a comment\n{}const last = 2;\n</script>\n",
            statements(0..50)
        );
        for at in [
            source.find("a string").unwrap() + 1,
            source.find("// a comment").unwrap() + "// a comment".len(),
        ] {
            let mut highlighter = SyntaxHighlighter::new("html");
            assert!(highlighter.update(None, &Rope::from_str(&source), None));
            let (edit, source) = replace(&source, at..at, &statements(50..150));
            assert!(!update_out_of_time(&mut highlighter, edit, &source));

            let highlights = highlighter.match_styles(0..source.len());
            for name in ["string", "comment"] {
                assert!(
                    !has_highlight_covering(&highlights, &source, "let n99 = 99;", name),
                    "pasted lines should not be read as one {name}"
                );
            }
            for name in ["first", "last"] {
                assert!(has_highlight_covering(
                    &highlights,
                    &source,
                    name,
                    "variable"
                ));
            }

            assert!(parse_injections_with_time(&mut highlighter));
            assert_eq!(
                highlight_names(&highlighter, &source),
                highlight_names(&fresh_highlighter("html", &source), &source)
            );
        }
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_injection_layer_out_of_time_is_finished_by_a_parse_with_time() {
        // `/*` typed at the top of a script makes all of it a comment up to
        // the `*/` further down; deleting it again means reading all of it
        // again, which a large script has no time for on a keystroke.
        let source = format!(
            "<script>\nconst first = 1;\n{}/* note */\nconst last = 2;\n</script>\n",
            statements(0..100)
        );
        let mut highlighter = SyntaxHighlighter::new("html");
        assert!(highlighter.update(None, &Rope::from_str(&source), None));
        let at = source.find("const first").unwrap();
        let (edit, opened) = replace(&source, at..at, "/*");
        assert!(highlighter.update(Some(edit), &Rope::from_str(&opened), None));
        let highlights = highlighter.match_styles(0..opened.len());
        assert!(has_highlight_covering(
            &highlights,
            &opened,
            "n50",
            "comment"
        ));

        let (edit, closed) = replace(&opened, at..at + 2, "");
        assert!(!update_out_of_time(&mut highlighter, edit, &closed));
        let highlights = highlighter.match_styles(0..closed.len());
        assert!(
            !has_highlight_covering(&highlights, &closed, "n50", "comment"),
            "a comment closed again should not still be read as one"
        );

        // A parse that is cancelled stops, however long it may take.
        let tree = highlighter.tree().unwrap().clone();
        let data = InjectionParseData {
            parse_timeout: None,
            cancel: Some(Arc::new(AtomicBool::new(true))),
            ..highlighter.injection_parse_data().unwrap()
        };
        let (_, finished) =
            SyntaxHighlighter::compute_injection_layers(data, &tree, highlighter.text());
        assert!(!finished);

        assert!(parse_injections_with_time(&mut highlighter));
        let highlights = highlighter.match_styles(0..closed.len());
        assert!(has_highlight_covering(
            &highlights,
            &closed,
            "n50",
            "variable"
        ));
        assert_eq!(
            highlight_names(&highlighter, &closed),
            highlight_names(&fresh_highlighter("html", &closed), &closed)
        );
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_injection_span_closed_early_out_of_time_colours_nothing_after_it() {
        for (language, source, close) in [
            (
                "html",
                format!(
                    "<script>\nconst first = 1;\n{}let tail_word = 1;\n{}</script>\n",
                    statements(0..50),
                    statements(50..100)
                ),
                "</script>\n",
            ),
            (
                "markdown",
                format!(
                    "```js\nconst first = 1;\n{}let tail_word = 1;\n{}```\n",
                    statements(0..50),
                    statements(50..100)
                ),
                "```\n\n",
            ),
        ] {
            let mut highlighter = SyntaxHighlighter::new(language);
            assert!(highlighter.update(None, &Rope::from_str(&source), None));
            let at = source.find("let n40").unwrap();
            let (edit, source) = replace(&source, at..at, close);
            assert!(
                !update_out_of_time(&mut highlighter, edit, &source),
                "{language}: a span with no old tree to keep should ask for another parse"
            );

            let highlights = highlighter.match_styles(0..source.len());
            assert!(
                !has_highlight_covering(&highlights, &source, "tail_word", "variable"),
                "{language}: text after a span closed early should not be coloured as the span"
            );
            for layer in highlighter.injection_layers.iter() {
                // moved to where its text is, as a parse would take it
                let mut layer = layer.clone();
                layer.catch_up();
                let root = layer.tree.root_node();
                assert!(
                    layer.byte_range.start <= root.start_byte()
                        && root.end_byte() <= layer.byte_range.end,
                    "{language}: a layer's tree should lie within its span"
                );
            }
        }
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_injection_layers_after_an_edit_match_a_fresh_parse() {
        let script = format!("<script>\n{}</script>\n<p>after</p>\n", statements(0..20));
        let fence = format!("```js\n{}```\n\nafter\n", statements(0..20));
        let at_script = script.find("let n10").unwrap();
        let at_fence = fence.find("let n10").unwrap();
        for (language, source, range, new_text) in [
            // A paragraph that joins the combined inline layer.
            ("markdown", "d` e\ng*\n\n".to_string(), 9..9, "`"),
            // A fence that becomes a paragraph.
            (
                "markdown",
                "```python\nx = 1\n```\n".to_string(),
                3..9,
                "py```\nthon",
            ),
            // A span closed early, and one split in two.
            ("html", script.clone(), at_script..at_script, "</script>\n"),
            (
                "html",
                script.clone(),
                at_script..at_script,
                "</script>\n<script>\n",
            ),
            ("markdown", fence.clone(), at_fence..at_fence, "```\n\n"),
            (
                "markdown",
                fence.clone(),
                at_fence..at_fence,
                "```\n\n```js\n",
            ),
            // Text typed at a span's very start, and deleted there.
            ("html", script.clone(), 8..8, "\nlet typed = 1;"),
            ("html", script.clone(), 9..13, ""),
            ("markdown", fence.clone(), 6..6, "let typed = 1;\n"),
        ] {
            let mut highlighter = fresh_highlighter(language, &source);
            let (edit, source) = replace(&source, range, new_text);
            assert!(highlighter.update(Some(edit), &Rope::from_str(&source), None));
            let fresh = fresh_highlighter(language, &source);
            assert_eq!(
                highlighter.tree().unwrap().root_node().to_sexp(),
                fresh.tree().unwrap().root_node().to_sexp(),
                "{source:?}"
            );
            assert_eq!(
                highlight_names(&highlighter, &source),
                highlight_names(&fresh, &source),
                "{source:?}"
            );
        }
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_markdown_fenced_code_injects_captured_language() {
        let markdown = "```rs\nfn first() {}\n```\n\n```rust\nfn second() {}\n```\n";
        let rope = Rope::from_str(markdown);
        let mut highlighter = SyntaxHighlighter::new("markdown");

        assert!(highlighter.update(None, &rope, None));

        let rust_layers = highlighter
            .injection_layers
            .iter()
            .filter(|layer| layer.language_name.as_ref() == "rust")
            .collect::<Vec<_>>();
        assert_eq!(rust_layers.len(), 2);
        assert!(
            Arc::ptr_eq(
                &rust_layers[0].highlight_query,
                &rust_layers[1].highlight_query
            ),
            "fences using the same canonical language should share one highlight query"
        );

        let highlights = highlighter.match_styles(0..markdown.len());
        for function in ["first", "second"] {
            assert!(
                has_highlight_covering(&highlights, markdown, function, "function"),
                "Rust function {function:?} should be highlighted inside its fence"
            );
        }

        let theme = HighlightTheme::default_dark();
        let styles = highlighter.styles(&(0..markdown.len()), theme.as_ref());
        let keyword_start = markdown.find("fn first").unwrap();
        let keyword_color = theme.style("keyword").and_then(|style| style.color);
        assert!(styles.iter().any(|(range, style)| {
            range.start <= keyword_start
                && range.end >= keyword_start + 2
                && style.color == keyword_color
        }));
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_markdown_unknown_fence_language_does_not_allocate_layer() {
        let markdown = "```not-a-registered-language\nplain content\n```\n";
        let rope = Rope::from_str(markdown);
        let mut highlighter = SyntaxHighlighter::new("markdown");

        assert!(highlighter.update(None, &rope, None));
        assert!(
            highlighter
                .injection_layers
                .iter()
                .all(|layer| { layer.language_name.as_ref() != "not-a-registered-language" })
        );
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_markdown_fenced_code_highlights_blocks_beyond_previous_limit() {
        const FENCE_COUNT: usize = 384;
        const _: () = assert!(FENCE_COUNT <= MAX_NON_COMBINED_INJECTION_PARSES);
        let markdown = (0..FENCE_COUNT)
            .map(|i| format!("```rust\nfn function_{i}() {{}}\n```\n"))
            .collect::<String>();
        let rope = Rope::from_str(&markdown);
        let mut highlighter = SyntaxHighlighter::new("markdown");

        assert!(highlighter.update(None, &rope, None));
        assert_eq!(highlighter.injection_layers.len(), FENCE_COUNT);
        assert!(
            highlighter
                .injection_layers
                .iter()
                .all(|layer| layer.language_name.as_ref() == "rust")
        );

        let highlights = highlighter.match_styles(0..markdown.len());
        assert!(has_highlight_covering(
            &highlights,
            &markdown,
            "function_383",
            "function"
        ));
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_markdown_fenced_code_injection_layers_are_bounded() {
        const FENCE_COUNT: usize = MAX_NON_COMBINED_INJECTION_PARSES + 128;
        // The paragraph trails the fences so the combined match is only reached
        // once the non-combined budget is already exhausted.
        let markdown = format!(
            "{}\nparagraph *inline*\n",
            (0..FENCE_COUNT)
                .map(|i| format!("```rust\nfn function_{i}() {{}}\n```\n"))
                .collect::<String>()
        );
        let rope = Rope::from_str(&markdown);
        let mut highlighter = SyntaxHighlighter::new("markdown");

        assert!(highlighter.update(None, &rope, None));
        assert_eq!(
            highlighter
                .injection_layers
                .iter()
                .filter(|layer| layer.language_name.as_ref() == "rust")
                .count(),
            MAX_NON_COMBINED_INJECTION_PARSES
        );
        assert!(
            highlighter
                .injection_layers
                .iter()
                .any(|layer| layer.language_name.as_ref() == "markdown_inline"),
            "the non-combined budget should not starve combined injection layers"
        );

        let highlights = highlighter.match_styles(0..markdown.len());
        assert!(has_highlight_covering(
            &highlights,
            &markdown,
            "function_0",
            "function"
        ));
        assert!(
            !has_highlight_covering(
                &highlights,
                &markdown,
                &format!("function_{}", FENCE_COUNT - 1),
                "function"
            ),
            "fences past the budget keep host highlighting but get no injected tokens"
        );
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_php_combined_injection_closing_tags() {
        let php_code = r#"<?php
$x = 1;
?>
<html>
<body>
  <h1><?php echo "Hello"; ?></h1>
  <ul>
    <?php foreach ($items as $item): ?>
      <li><?php echo $item; ?></li>
    <?php endforeach; ?>
  </ul>
</body>
</html>
"#;

        let rope = Rope::from_str(php_code);
        let mut highlighter = SyntaxHighlighter::new("php");
        highlighter.update(None, &rope, None);

        let full_range = 0..php_code.len();
        let highlights = highlighter.match_styles(full_range);

        // Verify all closing HTML tags are highlighted
        let closing_tags = ["</h1>", "</li>", "</ul>", "</body>", "</html>"];
        for tag in closing_tags {
            let pos = php_code.find(tag).unwrap();
            let tag_name_start = pos + 2; // after "</"
            let tag_name_end = tag_name_start + tag.len() - 3; // before ">"

            let has_highlight = highlights
                .iter()
                .any(|item| item.range.start <= tag_name_start && item.range.end >= tag_name_end);

            assert!(
                has_highlight,
                "closing tag {} at byte {} should be highlighted",
                tag, pos
            );
        }
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_markdown_inline_injection_layers_are_bounded() {
        let markdown = (0..(MAX_INJECTION_RANGES + 1024))
            .map(|i| format!("paragraph {i} *x*\n\n"))
            .collect::<String>();
        let rope = Rope::from_str(markdown.as_str());
        let mut highlighter = SyntaxHighlighter::new("markdown");

        assert!(highlighter.update(None, &rope, None));
        assert!(
            highlighter.injection_layers.len() <= 1,
            "markdown_inline should be combined instead of one layer per inline node"
        );

        if let Some(layer) = highlighter
            .injection_layers
            .iter()
            .find(|layer| layer.language_name.as_ref() == "markdown_inline")
        {
            assert!(layer.ranges.len() <= MAX_INJECTION_RANGES);
            assert!(injection_ranges_byte_count(&layer.ranges) <= MAX_INJECTION_BYTES);
        }

        let plain_markdown = (0..1024)
            .map(|i| format!("paragraph {i} plain\n\n"))
            .collect::<String>();
        let plain_rope = Rope::from_str(plain_markdown.as_str());
        let mut plain_highlighter = SyntaxHighlighter::new("markdown");

        assert!(plain_highlighter.update(None, &plain_rope, None));
        assert!(
            plain_highlighter
                .injection_layers
                .iter()
                .all(|layer| layer.language_name.as_ref() != "markdown_inline"),
            "plain inline ranges should not create markdown_inline injection layers"
        );
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_markdown_inline_code_spans_in_list_items() {
        let markdown = "- `one`\n- `two`\n- `three`\n\nLater `four`\n";
        let rope = Rope::from_str(markdown);
        let mut highlighter = SyntaxHighlighter::new("markdown");
        highlighter.update(None, &rope, None);

        let highlights = highlighter.match_styles(0..markdown.len());
        for text in ["one", "two", "three", "four"] {
            assert!(
                has_highlight_covering(&highlights, markdown, text, "text.code.span"),
                "{text:?} should be highlighted as a code span"
            );
        }

        let prose_start = markdown.find("Later").unwrap();
        let prose_end = prose_start + "Later".len();
        assert!(
            !highlights.iter().any(|item| {
                item.name.as_ref() == "text.code.span"
                    && item.range.start <= prose_start
                    && item.range.end >= prose_end
            }),
            "plain prose after the list should not be highlighted as a code span"
        );
    }

    /// Whether a highlight of the name begins where `text` begins in the
    /// source: a keyword covers its word and no more.
    #[cfg(feature = "tree-sitter-languages")]
    fn has_highlight_at(
        highlights: &[HighlightItem],
        source: &str,
        text: &str,
        highlight_name: &str,
    ) -> bool {
        let start = source.find(text).expect("text should exist in source");
        highlights
            .iter()
            .any(|item| item.name.as_ref() == highlight_name && item.range.start == start)
    }

    /// Above the size the editor parses on a keystroke, an edit reaches the
    /// tree and every layer without a parse: what it touched is plain until
    /// the background parse, and the rest keeps its colours. A paste into a
    /// string grows the string's node over the paste; a keyword typed
    /// beside a string leaves the string as it was.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_an_edit_without_a_parse_leaves_what_it_touched_plain() {
        let html = format!(
            "<p>hi</p>\n<script>\nconst greeting = \"hello\";\n{}const last = 2;\n</script>\n",
            statements(0..20)
        );
        let pasted = statements(100..4100);
        let script_at = html.find("hello").unwrap() + "hello".len();
        let fence = format!(
            "# Title\n\n```js\nconst greeting = \"hello\";\n{}```\n\n*after*\n",
            statements(0..20)
        );
        let fence_at = fence.find("hello").unwrap() + "hello".len();
        for (language, source, at) in [("html", html, script_at), ("markdown", fence, fence_at)] {
            let mut highlighter = fresh_highlighter(language, &source);
            let (edit, source) = replace(&source, at..at, &pasted);
            highlighter.edit_tree(Some(edit), &Rope::from_str(&source));
            let highlights = highlighter.match_styles(0..source.len());
            // the string the paste landed in stretches over it, and is plain
            assert!(
                !has_highlight_covering(&highlights, &source, "let n100", "string"),
                "{language}: the paste is not a string"
            );
            // a fence keeps its literal colour under its layer's tokens; a
            // script has nothing of its own over the paste
            assert!(
                !highlights.iter().any(|h| h.range.start <= at
                    && h.range.end >= at + pasted.len()
                    && !matches!(h.name.as_ref(), "text.literal" | "none")),
                "{language}: nothing colours the whole paste"
            );
            // the tokens before and after it keep their colours, and so does
            // the host around the script or the fence
            assert!(
                has_highlight_covering(&highlights, &source, "const", "keyword"),
                "{language}: the keyword before the paste"
            );
            assert!(
                has_highlight_at(&highlights, &source, "let n19 = 19", "keyword"),
                "{language}: a keyword after the paste"
            );
            if language == "html" {
                assert!(
                    has_highlight_at(&highlights, &source, "p>hi", "tag"),
                    "the host keeps its colours"
                );
            } else {
                assert!(
                    has_highlight_covering(&highlights, &source, "Title", "title"),
                    "the host keeps its colours"
                );
            }
            // a keystroke beside the string leaves the string as it was
            let (edit, source) = replace(
                &source,
                at + pasted.len() + 2..at + pasted.len() + 2,
                "\nconst typed = 1;",
            );
            highlighter.edit_tree(Some(edit), &Rope::from_str(&source));
            let highlights = highlighter.match_styles(0..source.len());
            assert!(
                has_highlight_covering(&highlights, &source, "const", "keyword"),
                "{language}: still the keyword before"
            );
            assert!(
                !has_highlight_covering(&highlights, &source, "let n100", "string"),
                "{language}: the paste is still not a string"
            );
            // the background parse then colours the paste as the grammar
            // makes of a string broken open by four thousand lines, and no
            // longer as one green block
            let mut parser = Parser::new();
            parser
                .set_language(
                    LanguageRegistry::singleton()
                        .language(language)
                        .unwrap()
                        .language
                        .as_ref()
                        .unwrap(),
                )
                .unwrap();
            let tree = parser.parse(&source, highlighter.tree()).unwrap();
            let text = Rope::from_str(&source);
            let data = InjectionParseData {
                parse_timeout: None,
                ..highlighter.injection_parse_data().unwrap()
            };
            let (layers, finished) =
                SyntaxHighlighter::compute_injection_layers(data, &tree, &text);
            assert!(finished);
            highlighter.apply_background_tree(tree, &text, layers);
            let highlights = highlighter.match_styles(0..source.len());
            assert!(
                !highlighter.stale && highlighter.edited.is_empty(),
                "{language}: served whole again"
            );
            assert!(
                !has_highlight_covering(&highlights, &source, "let n100", "string"),
                "{language}: the paste is not a string once parsed"
            );
        }
    }

    /// Text typed beside a token leaves the token as it was: a space after
    /// a semicolon, a character before a word. The word typed into goes
    /// plain, and so does a token beside a deletion, which may have taken
    /// its edge.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_an_edit_beside_a_token_leaves_it_its_colour() {
        let source = "let abc = \"xyz\";\nfoo(1);\nconst last = 2;\n";
        let cases: [(&str, &str, &[(&str, &str)], &[(&str, &str)]); 4] = [
            // a space after the semicolon
            (
                ";\nfoo",
                " ",
                &[("let", "keyword"), ("foo", "function")],
                &[],
            ),
            // the newline before const deleted: const, beside it, is plain
            (
                "\nconst",
                "",
                &[("foo", "function"), ("let", "keyword")],
                &[("const", "keyword")],
            ),
            // a character before a word
            ("abc", "x", &[("let", "keyword")], &[]),
            // a character typed at the end of a word: the word grows
            (
                " = \"xyz",
                "d",
                &[("let", "keyword"), ("\"xyz\"", "string")],
                &[("abc", "variable")],
            ),
        ];
        for (at, typed, kept, plain) in cases {
            let mut highlighter = fresh_highlighter("javascript", source);
            let p = source.find(at).unwrap();
            let end = if typed.is_empty() { p + 1 } else { p };
            let (edit, edited) = replace(source, p..end, typed);
            highlighter.edit_tree(Some(edit), &Rope::from_str(&edited));
            let highlights = highlighter.match_styles(0..edited.len());
            for (text, name) in kept {
                assert!(
                    has_highlight_at(&highlights, &edited, text, name),
                    "{at:?} + {typed:?}: {text} keeps {name}"
                );
            }
            for (text, name) in plain {
                assert!(
                    !has_highlight_at(&highlights, &edited, text, name),
                    "{at:?} + {typed:?}: {text} is plain"
                );
            }
        }
        // a keystroke inside a fence leaves the fence its colour, and the
        // token typed into plain; the parse after it matches a fresh one
        let fence = format!("# Title\n\n```js\n{}```\n", statements(0..3000));
        let mut highlighter = fresh_highlighter("markdown", &fence);
        let p = fence.find("let n1500").unwrap() + 2;
        let (edit, edited) = replace(&fence, p..p, "xy");
        highlighter.edit_tree(Some(edit), &Rope::from_str(&edited));
        let highlights = highlighter.match_styles(0..edited.len());
        assert!(
            has_highlight_covering(&highlights, &edited, "let n1499", "text.literal"),
            "the fence keeps its colour"
        );
        assert!(
            !has_highlight_at(&highlights, &edited, "lexyt", "keyword"),
            "the word typed into is plain"
        );
        assert!(has_highlight_at(
            &highlights,
            &edited,
            "let n1501",
            "keyword"
        ));
        let mut parser = Parser::new();
        parser
            .set_language(
                LanguageRegistry::singleton()
                    .language("markdown")
                    .unwrap()
                    .language
                    .as_ref()
                    .unwrap(),
            )
            .unwrap();
        let tree = parser.parse(&edited, highlighter.tree()).unwrap();
        let text = Rope::from_str(&edited);
        let data = InjectionParseData {
            parse_timeout: None,
            ..highlighter.injection_parse_data().unwrap()
        };
        let (layers, _) = SyntaxHighlighter::compute_injection_layers(data, &tree, &text);
        highlighter.apply_background_tree(tree, &text, layers);
        assert_eq!(
            highlight_names(&highlighter, &edited),
            highlight_names(&fresh_highlighter("markdown", &edited), &edited)
        );

        // `/*` deleted and a space typed where it was: the comment it
        // opened is still gone
        let commented = format!("/*{}*/\nlet z = 1;\n", statements(0..20));
        let mut highlighter = fresh_highlighter("javascript", &commented);
        let (edit, opened) = replace(&commented, 0..2, "");
        highlighter.edit_tree(Some(edit), &Rope::from_str(&opened));
        let (edit, spaced) = replace(&opened, 0..0, " ");
        highlighter.edit_tree(Some(edit), &Rope::from_str(&spaced));
        let highlights = highlighter.match_styles(0..spaced.len());
        assert!(
            !has_highlight_covering(&highlights, &spaced, "let n10", "comment"),
            "the comment stays gone"
        );
        assert!(has_highlight_at(&highlights, &spaced, "let z", "keyword"));

        // the whole text replaced at once is plain until parsed
        let mut highlighter = fresh_highlighter("javascript", source);
        highlighter.edit_tree(None, &Rope::from_str("const other = 1;\n"));
        assert!(highlighter.match_styles(0..17).is_empty());
    }

    /// Characters deleted one after another with no parse between them, as
    /// a held Backspace or an undo of typing does in a text too large to
    /// parse at each key, are one point where the last went, kept once.
    /// A point was kept again for every point at its place, so the points
    /// doubled at each deletion: 28 of them were 268 million, which took
    /// seconds and some 6 GB (elundqvist/kvist#368).
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_deletions_before_a_parse_keep_one_point_each() {
        let mut text = format!("let typed = \"{}\";\n{}", "x".repeat(40), statements(0..50));
        let mut highlighter = fresh_highlighter("javascript", &text);
        let end = text.find("\";").unwrap();
        // a held Backspace, from the end of the string back
        for at in (end - 24..end).rev() {
            let (edit, edited) = replace(&text, at..at + 1, "");
            highlighter.edit_tree(Some(edit), &Rope::from_str(&edited));
            text = edited;
        }
        let at = end - 24;
        assert_eq!(highlighter.edited.len(), 1, "one point");
        assert_eq!(highlighter.edited, vec![at..at], "where the last went");
        // a held Delete there, then a character typed: still one point a place
        for _ in 0..24 {
            let (edit, edited) = replace(&text, at..at + 1, "");
            highlighter.edit_tree(Some(edit), &Rope::from_str(&edited));
            text = edited;
        }
        let (edit, edited) = replace(&text, at..at, "y");
        highlighter.edit_tree(Some(edit), &Rope::from_str(&edited));
        assert_eq!(
            highlighter.edited,
            vec![at..at, at..at + 1, at + 1..at + 1],
            "the point, what was typed at it, and the point moved past it"
        );
    }

    /// A keystroke whose parse of the host runs out of time keeps the host's
    /// tree, edited, the same way: the token typed into is plain, the rest
    /// keeps its colours, and the tree is served whole again by the next
    /// parse that finishes.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_parse_out_of_time_leaves_what_the_edit_touched_plain() {
        let source = format!("const greeting = \"hello\";\n{}", statements(0..3000));
        let mut highlighter = fresh_highlighter("javascript", &source);
        let at = source.find("hello").unwrap() + "hello".len();
        let (edit, source) = replace(&source, at..at, &statements(5000..5100));
        assert!(!highlighter.update(Some(edit), &Rope::from_str(&source), Some(Duration::ZERO)));
        let highlights = highlighter.match_styles(0..source.len());
        assert!(!has_highlight_covering(
            &highlights,
            &source,
            "let n5000",
            "string"
        ));
        assert!(has_highlight_covering(
            &highlights,
            &source,
            "const",
            "keyword"
        ));
        assert!(has_highlight_at(
            &highlights,
            &source,
            "let n2999 = 2999",
            "keyword"
        ));
        let source = format!("{source} ");
        assert!(highlighter.update(None, &Rope::from_str(&source), None));
        assert_eq!(
            highlight_names(&highlighter, &source),
            highlight_names(&fresh_highlighter("javascript", &source), &source)
        );
    }

    /// A parse a time limit halted is not taken up again by the next call.
    /// tree-sitter goes on with a halted parse at the parser's next call,
    /// from the stack and the old tree it was halted with, over whatever
    /// text that call hands it, until the parser is reset; and `unwrap_or`
    /// made a tree of the empty text at every call, before anything else,
    /// which went on with the halted parse instead, over the empty text and
    /// with no limit.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_halted_parse_is_not_resumed_on_the_next_edit() {
        let source = format!("const greeting = \"hello\";\n{}", statements(0..3000));
        let mut highlighter = fresh_highlighter("javascript", &source);
        let resumed = Arc::new(AtomicBool::new(false));
        let seen = resumed.clone();
        highlighter.parser.set_logger(Some(Box::new(move |_, message| {
            if message.starts_with("resume_parsing") {
                seen.store(true, Ordering::Relaxed);
            }
        })));
        let at = source.find("hello").unwrap();
        let (edit, source) = replace(&source, at..at, "x");
        assert!(!highlighter.update(Some(edit), &Rope::from_str(&source), Some(Duration::ZERO)));
        let (edit, source) = replace(&source, at..at, "y");
        assert!(!highlighter.update(Some(edit), &Rope::from_str(&source), Some(Duration::ZERO)));
        let (edit, source) = replace(&source, at..at, "z");
        assert!(highlighter.update(Some(edit), &Rope::from_str(&source), None));
        assert!(
            !resumed.load(Ordering::Relaxed),
            "a parse a time limit halted was taken up again"
        );
        assert_eq!(
            highlight_names(&highlighter, &source),
            highlight_names(&fresh_highlighter("javascript", &source), &source)
        );
    }

    /// Markdown with injections for fences, HTML blocks and front matter
    /// and none for inline syntax, as Kvist has it: the inline layer is one
    /// layer over every paragraph, combined, and a query with a combined
    /// pattern is always made over the whole text.
    #[cfg(feature = "tree-sitter-languages")]
    fn markdown_blocks() -> &'static str {
        const INJECTIONS: &str = r#"
(fenced_code_block
  (info_string
    (language) @injection.language)
  (code_fence_content) @injection.content)

((html_block) @injection.content (#set! injection.language "html"))

((minus_metadata) @injection.content (#set! injection.language "yaml"))
"#;
        let registry = LanguageRegistry::singleton();
        if registry.language("markdown-blocks").is_none() {
            let markdown = registry.language("markdown").unwrap();
            registry.register(
                "markdown-blocks",
                &crate::highlighter::LanguageConfig::new(
                    "markdown-blocks",
                    markdown.language.clone().unwrap(),
                    vec![],
                    &markdown.highlights,
                    INJECTIONS,
                    "",
                ),
            );
        }
        "markdown-blocks"
    }

    /// Each layer's language and ranges.
    #[cfg(feature = "tree-sitter-languages")]
    fn layer_ranges(highlighter: &SyntaxHighlighter) -> Vec<(String, Vec<Range<usize>>)> {
        highlighter
            .injection_layers
            .iter()
            .map(|layer| {
                let ranges = layer.ranges.iter().map(|r| r.start_byte..r.end_byte);
                (layer.language_name.to_string(), ranges.collect())
            })
            .collect()
    }

    /// A key typed in a line of a fence, past the line's first character,
    /// moves the host's tree and parses the fence's layer alone, the host's
    /// parse left to the pause: tree-sitter-md parses the whole of a fence
    /// again for a key in it, every punctuation mark in it a node, and a key
    /// in a fence of 10 KB took 4.2 ms of the highlighter, 2 of them the
    /// host's parse, against 1 for a key in a paragraph
    /// (elundqvist/kvist#376). What it leaves is coloured as a fresh parse
    /// colours it, in a fence at the top, in a list item and in a quote.
    /// A key at a line's first character, which may close the fence or
    /// end what holds it, one in a line that begins as a fence does, a line
    /// break and a key in a fence of no language are parsed as any other.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_key_in_a_fences_line_parses_the_fence_alone() {
        let language = markdown_blocks();
        let body: String = (0..200)
            .map(|i| format!("fn function_{i}(value: u32) -> u32 {{\n    value * {i}\n}}\n"))
            .collect();
        let source = format!(
            "# Notes\n\nProse before.\n\n```rust\n{body}```\n\n- an item\n\n  ```rust\n  let item = 1;\n  ```\n\n> ```rust\n> let quoted = 2;\n> ```\n\n```\nplain text here\n```\n\n```rust\nlet a = 1;\n``x`\nlet b = 2;\n```\n\nProse after.\n"
        );
        let parses = || LAYER_PARSES.with(|parses| parses.get());
        // (where, how far into it, how much is taken out, what is put in,
        // and whether the host's tree is moved and the fence alone parsed)
        let keys: &[(&str, usize, usize, &str, bool)] = &[
            ("    value * 100", 9, 0, "x", true),
            ("    value * 100", 10, 1, "", true),
            ("    value * 100", 10, 0, "`", true),
            ("  let item = 1;", 6, 0, "x", true),
            ("> let quoted = 2;", 6, 0, "y", true),
            // the line's first character, and before it
            ("    value * 100", 4, 0, "x", false),
            ("    value * 100", 2, 0, "x", false),
            // a line break, put in and taken out
            ("    value * 100", 9, 0, "\n", false),
            ("    value * 100\n}", 15, 1, "", false),
            // a fence of no language, which has no layer
            ("plain text here", 6, 0, "x", false),
            // a line that begins as a fence does: taken out, the `x` leaves
            // the line that closes the fence
            ("``x`", 2, 1, "", false),
            // the fence's opening line
            ("```rust\nfn function_0", 5, 0, "x", false),
        ];
        for &(find, into, out, put, moved) in keys {
            let mut highlighter = fresh_highlighter(language, &source);
            let at = source.find(find).unwrap_or_else(|| panic!("{find:?}")) + into;
            let (edit, edited) = replace(&source, at..at + out, put);
            let before = parses();
            let parsed = highlighter.update(Some(edit), &Rope::from_str(&edited), Some(Duration::MAX));
            assert_eq!(parsed, !moved, "{put:?} at {find:?}");
            assert_eq!(highlighter.tree().unwrap().root_node().has_changes(), moved, "{put:?} at {find:?}");
            if moved {
                assert_eq!(parses() - before, 1, "{put:?} at {find:?}: the fence's layer alone");
            }
            let fresh = fresh_highlighter(language, &edited);
            assert_eq!(layer_ranges(&highlighter), layer_ranges(&fresh), "{put:?} at {find:?}");
            assert_eq!(
                highlight_names(&highlighter, &edited),
                highlight_names(&fresh, &edited),
                "{put:?} at {find:?}"
            );
        }

        // keys one after another, then the parse after the pause, and a
        // key in prose that parses the host from the tree they moved
        let mut highlighter = fresh_highlighter(language, &source);
        let mut text = source.clone();
        let at = text.find("    value * 150").unwrap() + "    value".len();
        for (i, c) in " + 1".chars().enumerate() {
            let (edit, edited) = replace(&text, at + i..at + i, &c.to_string());
            text = edited;
            assert!(!highlighter.update(Some(edit), &Rope::from_str(&text), Some(Duration::MAX)));
            assert_eq!(highlight_names(&highlighter, &text), highlight_names(&fresh_highlighter(language, &text), &text));
        }
        let mut paused = fresh_highlighter(language, &source);
        paused.update(None, &Rope::from_str(&text), None);
        assert!(parse_in_background(&mut highlighter));
        assert!(!highlighter.tree().unwrap().root_node().has_changes());
        assert_eq!(highlight_names(&highlighter, &text), highlight_names(&paused, &text));
        let at = text.find("Prose after").unwrap();
        let (edit, edited) = replace(&text, at..at, "x");
        assert!(highlighter.update(Some(edit), &Rope::from_str(&edited), Some(Duration::MAX)));
        assert_eq!(highlight_names(&highlighter, &edited), highlight_names(&fresh_highlighter(language, &edited), &edited));
        // and without the pause between
        let mut highlighter = fresh_highlighter(language, &source);
        let at = source.find("    value * 150").unwrap() + "    value".len();
        let (edit, mut text) = replace(&source, at..at, "x");
        assert!(!highlighter.update(Some(edit), &Rope::from_str(&text), Some(Duration::MAX)));
        let at = text.find("Prose after").unwrap();
        let (edit, edited) = replace(&text, at..at, "x");
        text = edited;
        assert!(highlighter.update(Some(edit), &Rope::from_str(&text), Some(Duration::MAX)));
        assert!(!highlighter.tree().unwrap().root_node().has_changes());
        assert_eq!(highlight_names(&highlighter, &text), highlight_names(&fresh_highlighter(language, &text), &text));
    }

    /// An edit moves the trees of the layers it reaches and no others: a
    /// layer after it keeps its tree where it was until the tree is used
    /// for a parse, and its colours are read where its text is now; one
    /// before it is as it was. Each tree moved, a key in a CHANGELOG of
    /// 3,000 items moved 3,000 trees, and the layers were copied for the
    /// parse after the pause, some 1 ms of every key (elundqvist/kvist#376):
    /// they are shared with it now, and copied only where an edit comes
    /// while it holds them. A tree moved late, by the edits before it put
    /// together, is the tree moved by each, line breaks among them.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_an_edit_moves_only_the_layer_trees_it_reaches() {
        const ITEMS: usize = 300;
        let language = markdown_by_paragraph();
        let items: String = (0..ITEMS)
            .map(|i| format!("- fix `foo_{i}` in [#{i}](http://x/{i}) and *note*\n"))
            .collect();
        let source = format!("```rust\nfn first() {{}}\n```\n\n# Changelog\n\n{items}\n```rust\nfn last() {{\n    let x = 1;\n}}\n```\n");
        let mut highlighter = fresh_highlighter(language, &source);
        assert_eq!(highlighter.injection_layers.len(), ITEMS + 2);
        let fence = |highlighter: &SyntaxHighlighter| {
            highlighter.injection_layers.iter().find(|l| l.language_name.as_ref() == "rust" && l.byte_range.start > 100).unwrap().clone()
        };
        let mut by_each = fence(&highlighter).tree;
        let edits = || LAYER_TREE_EDITS.with(|edits| edits.get());

        // keys in item 150, a line put in between two items, which reaches
        // neither, and a word taken out of item 10, and how many trees each
        // moves
        let mut text = source.clone();
        let changes: [(&str, usize, usize, &str, usize); 4] = [
            ("foo_150`", 7, 0, "x", 1),
            ("foo_150x`", 8, 0, "yz", 1),
            ("- fix `foo_200`", 0, 0, "- a new item\n", 0),
            ("- fix `foo_10`", 2, 4, "", 1),
        ];
        for (find, into, out, put, moved) in changes {
            let at = text.find(find).unwrap() + into;
            let (edit, edited) = replace(&text, at..at + out, put);
            text = edited;
            by_each.edit(&edit);
            let before = edits();
            highlighter.edit_tree(Some(edit), &Rope::from_str(&text));
            assert_eq!(edits() - before, moved, "{put:?} at {find:?}: the item's tree alone");
        }
        let last = fence(&highlighter);
        assert!(last.moved_from.is_some(), "after every edit, and not moved");
        assert!(highlighter.injection_layers[0].moved_from.is_none(), "before every edit");
        // read where its text is now, as a fresh parse colours it
        let fresh = fresh_highlighter(language, &text);
        let from = text.find("fn last").unwrap();
        let names = |h: &SyntaxHighlighter| {
            highlight_names(h, &text).into_iter().filter(|(r, _)| r.start >= from).collect::<Vec<_>>()
        };
        assert_eq!(names(&highlighter), names(&fresh));
        // moved by the edits put together as by each in turn, every node
        let mut caught_up = last.clone();
        caught_up.catch_up();
        fn nodes(tree: &Tree) -> Vec<(tree_sitter::Range, String)> {
            let mut out = Vec::new();
            let mut cursor = tree.walk();
            'walk: loop {
                out.push((cursor.node().range(), cursor.node().kind().to_string()));
                if cursor.goto_first_child() || cursor.goto_next_sibling() {
                    continue;
                }
                while cursor.goto_parent() {
                    if cursor.goto_next_sibling() {
                        continue 'walk;
                    }
                }
                break;
            }
            out
        }
        assert_eq!(nodes(&caught_up.tree), nodes(&by_each));

        // shared with the parse after the pause, not copied; an edit while
        // it holds them copies them, and leaves the parse's as they were
        let source_data = highlighter.injection_parse_source().unwrap();
        assert!(Arc::ptr_eq(&source_data.layers, &highlighter.injection_layers));
        let at = text.find("foo_20`").unwrap();
        let (edit, edited) = replace(&text, at..at, "w");
        highlighter.edit_tree(Some(edit), &Rope::from_str(&edited));
        assert!(!Arc::ptr_eq(&source_data.layers, &highlighter.injection_layers));
        let old_starts: Vec<usize> = source_data.layers.iter().map(|l| l.byte_range.start).collect();
        let new_starts: Vec<usize> = highlighter.injection_layers.iter().map(|l| l.byte_range.start).collect();
        assert_ne!(old_starts, new_starts);
        drop(source_data);
        let layers = Arc::as_ptr(&highlighter.injection_layers);
        let at = edited.find("foo_30`").unwrap();
        let (edit, edited) = replace(&edited, at..at, "v");
        highlighter.edit_tree(Some(edit), &Rope::from_str(&edited));
        assert_eq!(Arc::as_ptr(&highlighter.injection_layers), layers, "held by none, moved in place");

        // the parse after the pause, which reuses the trees moved late,
        // colours the text as a fresh parse does, and so does a key after it
        assert!(parse_in_background(&mut highlighter));
        let fresh = fresh_highlighter(language, &edited);
        assert_eq!(highlight_names(&highlighter, &edited), highlight_names(&fresh, &edited));
        assert_eq!(layer_ranges(&highlighter), layer_ranges(&fresh));
        let at = edited.find("let x").unwrap();
        let (edit, edited) = replace(&edited, at..at + 3, "const");
        assert!(highlighter.update(Some(edit), &Rope::from_str(&edited), None));
        let fresh = fresh_highlighter(language, &edited);
        assert_eq!(highlight_names(&highlighter, &edited), highlight_names(&fresh, &edited));
    }

    /// A layer's tree that the edits before it have not moved yet
    /// (`InjectionLayer::moved_from`) is read where its text is now, and so
    /// is the text its highlight query's predicates test: read where the
    /// tree has it, `#match?` and `#eq?` looked as many bytes off as the
    /// edits put in or took out, so `Vec`, `MAX`, `Foo`, `console` and
    /// `require` lost their colours or gave them to the text beside them,
    /// and a deletion before a fence or a script at the end of the text had
    /// them read past its end, which panicked on the drawing thread
    /// (elundqvist/kvist#376). Through an edit not parsed, as a text too
    /// large to parse at a key has it, and through a key's parse, which
    /// keeps the layers its edit does not reach as they are.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_layer_not_moved_yet_is_read_where_its_text_is() {
        let prose = "Prose, a sentence that is taken out, and more of it, longer than all that follows it.";
        let markdown = format!(
            "# Notes\n\n{prose}\n\n```rust\nlet v = Vec::new();\nconst MAX: u32 = Foo::BAR;\n```\n"
        );
        let html = format!("<p>{prose}</p>\n<script>console.log(Foo, MAX); require(x);</script>\n");
        let cases = [
            (markdown_blocks(), markdown, "let v", ["Vec", "MAX", "Foo"]),
            ("html", html, "console", ["console", "Foo", "require"]),
        ];
        for (language, source, layer_start, coloured) in cases {
            let fresh = fresh_highlighter(language, &source);
            let names = highlight_names(&fresh, &source);
            for word in coloured {
                let at = source.find(word).unwrap();
                assert!(
                    names.iter().any(|(r, _)| *r == (at..at + word.len())),
                    "{word:?} is coloured by a predicate in {language}: {names:?}"
                );
            }
            let start = source.find("a sentence").unwrap();
            let end = source.find("follows it").unwrap() + "follows it".len();
            assert!(end - start > source.len() - source.find(layer_start).unwrap());
            for (range, put) in [(start..start, "word, "), (start..end, "")] {
                let (edit, edited) = replace(&source, range.clone(), put);
                let fresh = fresh_highlighter(language, &edited);
                let from = edited.find(layer_start).unwrap();
                let after = |highlighter: &SyntaxHighlighter| {
                    highlight_names(highlighter, &edited)
                        .into_iter()
                        .filter(|(r, _)| r.start >= from)
                        .collect::<Vec<_>>()
                };
                let not_moved = |highlighter: &SyntaxHighlighter| {
                    highlighter.injection_layers.iter().any(|layer| layer.moved_from.is_some())
                };

                // moved, not parsed: the host's tree is stale where the edit
                // lies, and its colours there are left out
                let mut highlighter = fresh_highlighter(language, &source);
                highlighter.edit_tree(Some(edit), &Rope::from_str(&edited));
                assert!(not_moved(&highlighter), "{language}, {range:?}");
                assert_eq!(after(&highlighter), after(&fresh), "{language}, {range:?}, not parsed");

                // a key's parse that keeps the layer as it is
                let mut highlighter = fresh_highlighter(language, &source);
                assert!(highlighter.update(Some(edit), &Rope::from_str(&edited), Some(Duration::MAX)));
                assert!(not_moved(&highlighter), "{language}, {range:?}");
                assert_eq!(
                    highlight_names(&highlighter, &edited),
                    highlight_names(&fresh, &edited),
                    "{language}, {range:?}, parsed at the key"
                );
            }
        }
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_pattern_root_kinds_are_read_from_the_query() {
        let markdown = LanguageRegistry::singleton().language("markdown").unwrap();
        let query = Query::new(markdown.language.as_ref().unwrap(), &markdown.injections).unwrap();
        let roots = pattern_root_kinds(&query, &markdown.injections);
        let roots = roots.iter().map(|r| r.as_deref()).collect::<Vec<_>>();
        assert_eq!(
            roots,
            [
                Some("fenced_code_block"),
                Some("html_block"),
                Some("document"),
                Some("minus_metadata"),
                Some("plus_metadata"),
                Some("inline"),
            ]
        );
        let html = LanguageRegistry::singleton().language("html").unwrap();
        let source = concat!(
            "[(script_element) (style_element)] @injection.content\n",
            "((_) @injection.content)\n",
            // two nodes side by side: the match hangs on the element, which
            // holds no capture, as much as on the comment
            "((comment) @injection.content . (element))\n",
            "((comment) @injection.content)\n",
        );
        let query = Query::new(html.language.as_ref().unwrap(), source).unwrap();
        assert_eq!(
            pattern_root_kinds(&query, source),
            [None, None, None, Some(SharedString::from("comment"))]
        );
    }

    /// A keystroke looks for injections again only where the edits since
    /// the last finished parse reach, and keeps the layers beyond, their
    /// trees moved with the text and not parsed again: looked for over the
    /// whole text at every key, a README of 190 KB spent 9 ms of each
    /// keystroke's 10 on the query (elundqvist/kvist#43).
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_keystroke_keeps_the_layers_its_edit_does_not_reach() {
        let language = markdown_blocks();
        let source: String = (0..6)
            .map(|i| format!("```rust\nfn function_{i}() {{}}\n```\n\nProse about function {i}.\n\n"))
            .collect();
        let mut highlighter = fresh_highlighter(language, &source);
        assert_eq!(highlighter.injection_layers.len(), 6);
        let parses = || LAYER_PARSES.with(|parses| parses.get());

        let at = source.find("about function 2").unwrap();
        let (edit, source) = replace(&source, at..at, "x");
        let before = parses();
        assert!(highlighter.update(Some(edit), &Rope::from_str(&source), Some(Duration::MAX)));
        assert_eq!(
            parses() - before,
            0,
            "a layer the edit does not reach was looked for and parsed again"
        );
        assert_eq!(highlighter.injection_layers.len(), 6);
        assert_eq!(
            highlight_names(&highlighter, &source),
            highlight_names(&fresh_highlighter(language, &source), &source)
        );

        // an edit in a fence parses that fence again, and it alone, the
        // host's tree moved and its parse left to the pause
        // (`in_a_fence_line`)
        let at = source.find("function_4").unwrap();
        let (edit, source) = replace(&source, at..at, "x");
        let before = parses();
        assert!(!highlighter.update(Some(edit), &Rope::from_str(&source), Some(Duration::MAX)));
        assert_eq!(parses() - before, 1);
        assert_eq!(highlighter.injection_layers.len(), 6);
        assert_eq!(
            highlight_names(&highlighter, &source),
            highlight_names(&fresh_highlighter(language, &source), &source)
        );
    }

    /// A keystroke whose parse ran out of time leaves its edit to the next
    /// one that finishes, which looks for the layers as far as both reach:
    /// the edit in a fence a halted keystroke made is parsed into its layer,
    /// with the prose typed after it far away, and no layer is left stale;
    /// and the other way round, a halted keystroke in prose, then one in a
    /// fence.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_keystroke_after_one_out_of_time_looks_where_both_reach() {
        let language = markdown_blocks();
        let prose: String = (0..200)
            .map(|i| format!("Paragraph {i} says *something* about a [link](#{i}).\n\n"))
            .collect();
        let source: String = (0..6)
            .map(|i| format!("```rust\nfn function_{i}() {{}}\n```\n\nProse about function {i}.\n\n{prose}"))
            .collect();
        let parses = || LAYER_PARSES.with(|parses| parses.get());
        // (where the halted keystroke types, what, then where the one with
        // time types, what, and how many fences lie between the two, both
        // included): a number typed in a fence is a number only in the layer
        // parsed again. The halted key types at the start of a fence's line:
        // one typed further on in the line does not parse the host
        // (`in_a_fence_line`)
        let fence_1 = format!("about function 0.\n\n{prose}```rust\n");
        let orders: [(&str, &str, &str, &str, usize); 2] = [
            (&fence_1, "0x7f; ", "about function 4", "x", 4),
            ("about function 1", "x", "fn function_4() {", " 0x7f ", 3),
        ];
        for (halted_at, halted, timed_at, timed, fences) in orders {
            let mut highlighter = fresh_highlighter(language, &source);
            assert_eq!(highlighter.injection_layers.len(), 6);

            let at = source.find(halted_at).unwrap() + halted_at.len();
            let (edit, edited) = replace(&source, at..at, halted);
            assert!(
                !highlighter.update(Some(edit), &Rope::from_str(&edited), Some(Duration::ZERO)),
                "the host's parse was to run out of time"
            );
            assert!(highlighter.injection_layers.iter().all(|layer| layer.stale));

            let at = edited.find(timed_at).unwrap() + timed_at.len();
            let (edit, edited) = replace(&edited, at..at, timed);
            let before = parses();
            assert!(highlighter.update(Some(edit), &Rope::from_str(&edited), Some(Duration::MAX)));
            // the fences from the first edit to the second, and no others
            assert_eq!(parses() - before, fences, "after {halted:?} at {halted_at:?}");
            assert!(highlighter.injection_layers.iter().all(|layer| !layer.stale));
            let fresh = fresh_highlighter(language, &edited);
            let names = highlight_names(&highlighter, &edited);
            assert_eq!(names, highlight_names(&fresh, &edited), "after {halted:?} at {halted_at:?}");
            let number = edited.find("0x7f").unwrap();
            assert!(
                names.iter().any(|(range, _)| *range == (number..number + 4)),
                "the number typed in a fence is not coloured: {names:?}"
            );
        }
    }

    /// The layers after each edit, which a keystroke looks for only where
    /// its edits reach, are the ones a query over the whole text finds:
    /// an edit of a fence's language, one that takes the language away or
    /// opens or shuts a fence, front matter, an HTML block, a script's type.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_layers_looked_for_where_an_edit_reaches_match_a_whole_query() {
        fn layers(highlighter: &SyntaxHighlighter) -> Vec<(String, Vec<Range<usize>>)> {
            highlighter
                .injection_layers
                .iter()
                .map(|layer| {
                    let ranges = layer.ranges.iter().map(|r| r.start_byte..r.end_byte);
                    (layer.language_name.to_string(), ranges.collect())
                })
                .collect()
        }
        let markdown = concat!(
            "---\ntitle: notes\n---\n\n",
            "Prose before.\n\n",
            "```rust\nfn first() {}\n```\n\n",
            "<div class=\"note\">\nhi\n</div>\n\n",
            "More prose.\n\n",
            "```js\nconst second = 2;\n```\n\n",
            "Prose after.\n",
        );
        // (what to find, where in it, what to put there: an empty find is
        // the text's end)
        let markdown_edits: &[(&str, usize, usize, &str)] = &[
            ("```rust", 3, 7, "python"),
            ("```python", 3, 9, ""),
            ("```\nfn first", 3, 3, "rust"),
            ("title", 0, 0, "x"),
            ("Prose before.", 0, 0, "```json\n"),
            ("```json\n", 8, 8, "{\"a\": 1}\n```\n\n"),
            ("<div class", 1, 4, "span"),
            ("hi\n", 0, 2, "<b>bold</b>"),
            ("More prose.", 0, 0, "```"),
            ("```More", 0, 3, ""),
            ("const second", 6, 12, "third"),
            ("```\n\nProse after", 0, 3, ""),
            ("", 0, 0, "\n```\n"),
            ("Prose after", 0, 5, "Text"),
        ];
        let html = concat!(
            "<p>hi</p>\n",
            "<script>\nconst first = 1;\n</script>\n",
            "<style>\n.a { color: red; }\n</style>\n",
            "<script>\nconst last = 2;\n</script>\n",
        );
        let html_edits: &[(&str, usize, usize, &str)] = &[
            ("<p>hi", 3, 5, "hello"),
            ("<style>", 1, 6, "script"),
            ("</style>", 2, 7, "script"),
            ("<script>\nconst last", 7, 7, " "),
            ("const first", 0, 5, "let"),
            ("</script>\n<script>", 0, 0, "<p>between</p>"),
        ];
        for (language, source, edits) in [
            (markdown_blocks(), markdown, markdown_edits),
            ("html", html, html_edits),
        ] {
            let mut source = source.to_string();
            let mut highlighter = fresh_highlighter(language, &source);
            for (find, from, to, put) in edits {
                let at = if find.is_empty() {
                    source.len()
                } else {
                    source.find(find).unwrap_or_else(|| panic!("{find:?} in {source:?}"))
                };
                let (edit, edited) = replace(&source, at + from..at + to, put);
                source = edited;
                highlighter.injection_parse_timeout = Duration::MAX;
                // a key in a fence's line leaves the host's parse to the
                // pause (`in_a_fence_line`), and finds what a whole query does
                let in_a_fence = *find == "const second";
                assert_eq!(
                    highlighter.update(Some(edit), &Rope::from_str(&source), Some(Duration::MAX)),
                    !in_a_fence,
                    "{language}, after {put:?} at {find:?}"
                );
                let fresh = fresh_highlighter(language, &source);
                assert_eq!(layers(&highlighter), layers(&fresh), "{language}, after {put:?} at {find:?}: {source:?}");
                assert_eq!(
                    highlight_names(&highlighter, &source),
                    highlight_names(&fresh, &source),
                    "{language}, after {put:?} at {find:?}: {source:?}"
                );
            }
        }
    }

    /// The inline layer after an edit is what a fresh parse of the text
    /// gives: tree-sitter-markdown's inline grammar, parsed again from its
    /// old tree, reads some emphasis otherwise than from scratch, so the
    /// layer is never parsed from its old tree.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_markdown_inline_layer_after_an_edit_matches_a_fresh_parse() {
        // Found by the #34 fuzz: the first `)` replaced loses the emphasis.
        let source = "\n`\n\\\\**[a](b)[a](b)\n\n";
        let at = source.find(')').unwrap();
        let mut highlighter = fresh_highlighter("markdown", source);
        let (edit, source) = replace(source, at..at + 1, "[a](b)*");
        assert!(highlighter.update(Some(edit), &Rope::from_str(&source), None));
        let fresh = fresh_highlighter("markdown", &source);
        assert_eq!(
            highlight_names(&highlighter, &source),
            highlight_names(&fresh, &source),
            "{source:?}"
        );
        assert!(
            highlight_names(&fresh, &source)
                .iter()
                .any(|(_, name)| name.as_ref() == "emphasis"),
            "the edited text holds an emphasis: {source:?}"
        );

        // Random edits of random inline text, as the fuzz did, and as a user
        // does: closing and opening delimiters, and typing in one paragraph
        // while another holds emphasis.
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut next = |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        const PIECES: &[&str] = &[
            "a", "b", " ", "\n", "*", "**", "_", "`", "[", "]", "(", ")", "\\", "!", "<", ">", "~",
            "\n\n", "- ",
        ];
        let text = |len: usize, next: &mut dyn FnMut(usize) -> usize| -> String {
            (0..len).map(|_| PIECES[next(PIECES.len())]).collect()
        };
        for _ in 0..300 {
            let len = 3 + next(12);
            let source = text(len, &mut next);
            let start = next(source.len() + 1);
            let end = start + next(source.len() - start + 1);
            let len = next(4);
            let typed = text(len, &mut next);
            let mut highlighter = fresh_highlighter("markdown", &source);
            let (edit, edited) = replace(&source, start..end, &typed);
            assert!(highlighter.update(Some(edit), &Rope::from_str(&edited), None));
            let fresh = fresh_highlighter("markdown", &edited);
            assert_eq!(
                highlight_names(&highlighter, &edited),
                highlight_names(&fresh, &edited),
                "{source:?} with {start}..{end} replaced by {typed:?}"
            );
        }
    }

    /// The inline layer still keeps its old tree, edited to the text, when
    /// its parse runs out of time: what an edit did not touch keeps its
    /// colours until the background parse finishes.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_markdown_inline_layer_out_of_time_keeps_its_last_highlights() {
        let source = format!(
            "*one* and `two`\n\n{}three *four*\n",
            (0..200)
                .map(|i| format!("*a{i}* b `c{i}`\n\n"))
                .collect::<String>()
        );
        let mut highlighter = fresh_highlighter("markdown", &source);
        let at = source.find("three ").unwrap() + "three ".len();
        let (edit, source) = replace(&source, at..at, "typed ");
        assert!(!update_out_of_time(&mut highlighter, edit, &source));
        let highlights = highlighter.match_styles(0..source.len());
        assert!(has_highlight_covering(
            &highlights,
            &source,
            "one",
            "emphasis"
        ));
        assert!(has_highlight_covering(
            &highlights,
            &source,
            "two",
            "text.code.span"
        ));
        assert!(parse_injections_with_time(&mut highlighter));
        assert_eq!(
            highlight_names(&highlighter, &source),
            highlight_names(&fresh_highlighter("markdown", &source), &source)
        );
    }

    /// Markdown as Kvist registers it (elundqvist/kvist#54): the blocks of
    /// `markdown_blocks`, and the inline syntax of every paragraph, heading
    /// and table cell a layer of its own, as tree-sitter-md's own query has
    /// it, not one combined layer over them all.
    #[cfg(feature = "tree-sitter-languages")]
    fn markdown_by_paragraph() -> &'static str {
        let registry = LanguageRegistry::singleton();
        if registry.language("markdown-by-paragraph").is_none() {
            let blocks = registry.language(markdown_blocks()).unwrap();
            let injections = format!(
                "{}{}",
                blocks.injections,
                r#"
((inline) @injection.content
  (#set! injection.language "markdown_inline")
  (#set! injection.include-children))

((pipe_table_cell) @injection.content
  (#set! injection.language "markdown_inline")
  (#set! injection.include-children))
"#
            );
            registry.register(
                "markdown-by-paragraph",
                &crate::highlighter::LanguageConfig::new(
                    "markdown-by-paragraph",
                    blocks.language.clone().unwrap(),
                    vec![],
                    &blocks.highlights,
                    &injections,
                    "",
                ),
            );
        }
        "markdown-by-paragraph"
    }

    /// What the editor's background parse does with the edits a keystroke
    /// left it: the host parsed again from its edited tree, then the
    /// layers, each with all the time it needs.
    #[cfg(feature = "tree-sitter-languages")]
    fn parse_in_background(highlighter: &mut SyntaxHighlighter) -> bool {
        let text = highlighter.text().clone();
        let config = LanguageRegistry::singleton()
            .language(highlighter.language())
            .unwrap();
        let mut parser = Parser::new();
        parser.set_language(config.language.as_ref().unwrap()).unwrap();
        let tree = parser
            .parse(text.to_string(), highlighter.tree())
            .unwrap();
        let data = InjectionParseData {
            parse_timeout: None,
            ..highlighter.injection_parse_data().unwrap()
        };
        let (layers, finished) = SyntaxHighlighter::compute_injection_layers(data, &tree, &text);
        highlighter.apply_background_tree(tree, &text, layers);
        finished
    }

    /// A paragraph's inline syntax is read alone: a backtick or a `*` left
    /// open in one pairs with nothing in the next, list items next to each
    /// other are read apart, and a table's cells too. Combined into one
    /// layer, the stray backtick took everything up to the next one in the
    /// next paragraph for a code span, and the code spans after it were
    /// read inside out (elundqvist/kvist#54).
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_paragraph_of_markdown_is_read_alone() {
        let source = concat!(
            "Press the ` key.\n\nThen run `foo` and `bar`, *one star.\n\nThen *em* here.\n\n",
            "- `one`\n- `two`\n\nLater `four`\n\n",
            "| `--jobs` | [x](http://y) |\n|---|---|\n| `8` | **z** |\n",
        );
        let highlighter = fresh_highlighter(markdown_by_paragraph(), source);
        let highlights = highlighter.match_styles(0..source.len());
        for (text, name) in [
            ("`foo`", "text.code.span"),
            ("`bar`", "text.code.span"),
            ("*em*", "emphasis"),
            ("`one`", "text.code.span"),
            ("`two`", "text.code.span"),
            ("`four`", "text.code.span"),
            ("`--jobs`", "text.code.span"),
            ("`8`", "text.code.span"),
            ("x", "link_text"),
            ("http://y", "link_uri"),
            ("**z**", "emphasis.strong"),
        ] {
            assert!(
                has_highlight_covering(&highlights, source, text, name),
                "{text:?} is no {name}"
            );
        }
        for text in ["key.", "Then run", ", *one", "Later"] {
            let start = source.find(text).unwrap();
            assert!(
                !highlights.iter().any(|item| {
                    item.range.start < start + text.len()
                        && start < item.range.end
                        && ["text.code.span", "emphasis"].contains(&item.name.as_ref())
                }),
                "{text:?} is read as part of a span or an emphasis"
            );
        }
    }

    /// Markdown's `~~gone~~` is named `strikethrough`, and a theme's style
    /// of that name strikes it through, the default themes' too, beside an
    /// emphasis that stays unstruck (elundqvist/kvist#382).
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_markdown_strikethrough_is_struck_through() {
        let source = "Some *em* and ~~gone~~ text.\n";
        let highlighter = fresh_highlighter(markdown_by_paragraph(), source);
        let highlights = highlighter.match_styles(0..source.len());
        assert!(has_highlight_covering(&highlights, source, "~~gone~~", "strikethrough"));
        let at = |styles: &[(Range<usize>, HighlightStyle)], text: &str| {
            let byte = source.find(text).unwrap();
            styles
                .iter()
                .find(|(range, _)| range.contains(&byte))
                .map(|(_, style)| *style)
                .unwrap_or_default()
        };
        for theme in [HighlightTheme::default_dark(), HighlightTheme::default_light()] {
            let styles = highlighter.styles(&(0..source.len()), theme.as_ref());
            let struck = at(&styles, "gone").strikethrough.expect("gone is struck through");
            assert_eq!((struck.thickness, struck.color), (gpui::px(1.), None));
            assert_eq!(at(&styles, "em").strikethrough, None);
            assert_eq!(at(&styles, "text").strikethrough, None);
        }
        let mut theme = HighlightTheme::default_dark().as_ref().clone();
        theme.style.syntax.strikethrough = None;
        let styles = highlighter.styles(&(0..source.len()), &theme);
        assert_eq!(at(&styles, "gone").strikethrough, None, "a theme without the style");
    }

    /// Paragraphs are not counted against the cap on spans parsed: a
    /// CHANGELOG's thousand items each have their layer, and so has the
    /// fence after them, where the cap left all past the 512th plain. A
    /// keystroke in one item looks only where it reaches and parses that
    /// item's layer alone, and asks for no background parse.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_paragraphs_are_not_counted_against_the_cap() {
        const ITEMS: usize = MAX_NON_COMBINED_INJECTION_PARSES + 200;
        let language = markdown_by_paragraph();
        let source = format!(
            "# Changelog\n\n{}\n```rust\nfn last() {{}}\n```\n",
            (0..ITEMS)
                .map(|i| format!("- fix `foo_{i}` in [#{i}](http://x/{i}) and *note*\n"))
                .collect::<String>()
        );
        let mut highlighter = fresh_highlighter(language, &source);
        assert_eq!(highlighter.injection_layers.len(), ITEMS + 1);
        let highlights = highlighter.match_styles(0..source.len());
        let last = format!("`foo_{}`", ITEMS - 1);
        assert!(has_highlight_covering(&highlights, &source, &last, "text.code.span"));
        assert!(has_highlight_covering(&highlights, &source, "last", "function"));

        let parses = || LAYER_PARSES.with(|parses| parses.get());
        let at = source.find("foo_300`").unwrap();
        let (edit, source) = replace(&source, at..at, "x");
        let before = parses();
        assert!(highlighter.update(Some(edit), &Rope::from_str(&source), Some(Duration::MAX)));
        assert_eq!(parses() - before, 1, "the item typed into is parsed, and it alone");
        assert_eq!(
            highlight_names(&highlighter, &source),
            highlight_names(&fresh_highlighter(language, &source), &source)
        );
    }

    /// On an update with a time limit the paragraphs share one span's time
    /// between them, and once it is spent the rest are not begun: each keeps
    /// its old tree, stale, its untouched colours with it, and the update
    /// asks for the background parse. That parse parses only what an edit
    /// reached and keeps every other paragraph's tree as it is: parsed from
    /// scratch, all of them, it took a README's background parse 60 ms more.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_paragraphs_share_a_keystrokes_time() {
        let language = markdown_by_paragraph();
        let source: String = (0..30)
            .map(|i| format!("Paragraph {i} with *em{i}* and `c{i}`.\n\n"))
            .collect();
        let mut highlighter = fresh_highlighter(language, &source);
        let parses = || LAYER_PARSES.with(|parses| parses.get());

        // the time spent: the paragraph typed into keeps its old tree
        let at = source.find("Paragraph 5 ").unwrap() + "Paragraph 5 ".len();
        let (edit, source) = replace(&source, at..at, "x");
        let before = parses();
        assert!(!update_out_of_time(&mut highlighter, edit, &source));
        assert_eq!(parses() - before, 0, "a paragraph was begun with no time left");
        assert_eq!(highlighter.injection_layers.len(), 30);
        let highlights = highlighter.match_styles(0..source.len());
        for text in ["em4", "c5", "em6"] {
            assert!(
                highlights
                    .iter()
                    .any(|item| item.range.start <= source.find(text).unwrap()
                        && source.find(text).unwrap() < item.range.end),
                "{text:?} lost its colour"
            );
        }

        // the background parse parses the one an edit reached
        let before = parses();
        assert!(parse_in_background(&mut highlighter));
        assert_eq!(parses() - before, 1);
        assert_eq!(
            highlight_names(&highlighter, &source),
            highlight_names(&fresh_highlighter(language, &source), &source)
        );

        // prose that was a fence: none has a layer to keep, and the
        // background parse makes them all
        let fenced = format!("~~~\n{source}~~~\n");
        let mut highlighter = fresh_highlighter(language, &fenced);
        assert_eq!(highlighter.injection_layers.len(), 0);
        let (edit, source) = replace(&fenced, 0..4, "");
        assert!(!update_out_of_time(&mut highlighter, edit, &source));
        assert_eq!(highlighter.injection_layers.len(), 0);
        assert!(parse_in_background(&mut highlighter));
        assert_eq!(highlighter.injection_layers.len(), 30);
        assert_eq!(
            highlight_names(&highlighter, &source),
            highlight_names(&fresh_highlighter(language, &source), &source)
        );
    }

    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_highlight_allow_overlap_property_combines_nested_captures() {
        let markdown = "This has **_bold and italic_** and **bold _with_ italic** text.";
        let rope = Rope::from_str(markdown);
        let mut highlighter = SyntaxHighlighter::new("markdown");
        highlighter.update(None, &rope, None);

        let theme = HighlightTheme::default_dark();
        let styles = highlighter.styles(&(0..markdown.len()), theme.as_ref());
        for text in ["bold and italic", "with"] {
            let start = markdown.find(text).unwrap();
            let end = start + text.len();

            assert!(
                styles.iter().any(|(range, style)| {
                    range.start <= start
                        && range.end >= end
                        && style.font_weight == Some(gpui::FontWeight::BOLD)
                        && style.font_style == Some(gpui::FontStyle::Italic)
                }),
                "{text:?} should combine bold and italic styles"
            );
        }

        let highlights = highlighter.match_styles(0..markdown.len());
        let delimiter_start = markdown.find("_with_").unwrap();
        let delimiter_end = delimiter_start + "_".len();

        assert!(
            highlights.iter().any(|item| {
                item.name.as_ref() == "punctuation.delimiter"
                    && item.range.start <= delimiter_start
                    && item.range.end >= delimiter_end
            }),
            "overlap-enabled captures should not hide nested delimiter highlights"
        );
    }

    /// A node far larger than the range in view keeps its own colour. The
    /// query goes into such a node's children, to leave what is out of view
    /// alone, and had lost what matched on the node itself: a string of
    /// 4,000 lines pasted into, with no quote in it, was plain for good,
    /// where #45 has it plain only until parsed; and so was the name of a
    /// function far longer than the view, a pattern on the function.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_a_node_larger_than_the_view_keeps_its_own_colour() {
        let words = "word ".repeat(4_000);
        let source = format!("fn main() {{\n    let s = \"{words}\";\n}}\nfn after() {{}}\n");
        let highlighter = fresh_highlighter("rust", &source);
        let middle = source.find(&words).unwrap() + words.len() / 2;
        let view = middle..middle + 200;
        let theme = HighlightTheme::default_dark();
        assert_eq!(
            highlighter.styles(&view, theme.as_ref()),
            vec![(view.clone(), theme.style("string").unwrap())],
            "the middle of the string is the string's colour"
        );
        // the string's end in view, and what comes after it
        let end = source.find("\";\n}").unwrap();
        let highlights = highlighter.match_styles(end - 100..source.len());
        assert!(has_highlight_covering(
            &highlights,
            &source,
            "word \"",
            "string"
        ));
        assert!(has_highlight_at(&highlights, &source, "after", "function"));

        let body = statements(0..2_000);
        assert!(body.len() > 2 * LARGE_NODE_THRESHOLD);
        let source = format!("fn big() {{\n{body}}}\n");
        let highlighter = fresh_highlighter("rust", &source);
        let highlights = highlighter.match_styles(0..200);
        assert!(
            has_highlight_at(&highlights, &source, "big", "function"),
            "{highlights:?}"
        );
        assert!(has_highlight_at(&highlights, &source, "let n0", "keyword"));

        // a stale string an edit reached is still plain until it is parsed
        let before = "fn main() { let s = \"x\"; }\n";
        let mut highlighter = fresh_highlighter("rust", before);
        let at = before.find("x\"").unwrap() + 1;
        let (edit, source) = replace(before, at..at, &words);
        highlighter.edit_tree(Some(edit), &Rope::from_str(&source));
        let middle = at + words.len() / 2;
        let highlights = highlighter.match_styles(middle..middle + 200);
        assert!(
            !highlights.iter().any(|h| h.name.as_ref() == "string"),
            "{highlights:?}"
        );
    }

    /// The nodes in view of a tree nested thousands of levels deep are
    /// found on a thread's stack as those of a flat one are: tree-sitter's
    /// error recovery nests the tree of a 1.4 MB paste of Rust with a key
    /// typed before it some 7,700 levels deep, each level far larger than
    /// the view, and a walk a call a level runs out of a thread's stack.
    /// Parentheses nested a line each nest a tree as deep.
    #[test]
    #[cfg(feature = "tree-sitter-languages")]
    fn test_the_nodes_in_view_of_a_tree_nested_deep_are_found_on_a_small_stack() {
        const DEPTH: usize = 20_000;
        let source = format!(
            "fn deep() {{\n    let v = {}1{};\n}}\n",
            "(\n".repeat(DEPTH),
            "\n)".repeat(DEPTH)
        );
        let config = LanguageRegistry::singleton().language("rust").unwrap();
        let mut parser = Parser::new();
        parser.set_language(config.language.as_ref().unwrap()).unwrap();
        let tree = parser.parse(&source, None).unwrap();
        let middle = source.find("1\n").unwrap();
        let range = middle..middle + 1;
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let (nodes, gone_into) = collect_query_nodes(tree.root_node(), &range);
                // gone into, each within the one before it and far larger
                // than the view, down to the parenthesis queried whole
                assert!(gone_into.len() > DEPTH / 2, "{}", gone_into.len());
                let child_of = |child: tree_sitter::Node, parent: tree_sitter::Node| {
                    let mut cursor = parent.walk();
                    parent.children(&mut cursor).any(|c| c == child)
                };
                assert_eq!(gone_into[0], tree.root_node());
                for pair in gone_into.windows(2) {
                    assert!(child_of(pair[1], pair[0]));
                }
                assert!(gone_into.iter().all(|node| node.byte_range().len() > 1 + LARGE_NODE_THRESHOLD));
                let [node] = nodes.as_slice() else {
                    panic!("one node queried whole: {nodes:?}");
                };
                assert!(child_of(*node, *gone_into.last().unwrap()));
                assert_eq!(node.kind(), "parenthesized_expression");
                assert!(node.byte_range().len() <= 1 + LARGE_NODE_THRESHOLD);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// A parse after an edit and a highlight query are spans for a
    /// subscriber that takes trace-level spans, as gpui's frames are, with
    /// the language and the bytes parsed or looked at; a text with no
    /// grammar, which is never parsed, is none (elundqvist/kvist#589).
    #[cfg(feature = "tree-sitter-languages")]
    #[test]
    fn test_a_parse_and_a_highlight_query_are_perf_spans() {
        let (closed, _watching) = watch_perf_spans();
        let source = "fn main() {}\n";
        let mut highlighter = SyntaxHighlighter::new("rust");
        assert!(highlighter.update(None, &Rope::from_str(source), None));
        let (edit, typed) = replace(source, 11..11, " 1 ");
        assert!(highlighter.update(Some(edit), &Rope::from_str(&typed), Some(Duration::MAX)));
        let theme = HighlightTheme::default_dark();
        highlighter.styles(&(3..7), theme.as_ref());
        let mut plain = SyntaxHighlighter::new("text");
        assert!(plain.update(None, &Rope::from_str(source), None));
        assert_eq!(
            *closed.lock().unwrap(),
            [
                "parse language=rust bytes=13",
                "parse language=rust bytes=16",
                "highlight language=rust bytes=4"
            ]
        );
    }

    /// The trees moved with an edit and not parsed, a key's in a text too
    /// large to parse at the key and a paste's over all of one, are a
    /// span, `move`, of the text's size: what such a key costs the
    /// drawing thread was in no span (elundqvist/kvist#594).
    #[cfg(feature = "tree-sitter-languages")]
    #[test]
    fn test_trees_moved_without_a_parse_are_a_perf_span() {
        let (closed, _watching) = watch_perf_spans();
        let source = "fn main() {}\n";
        let mut highlighter = SyntaxHighlighter::new("rust");
        assert!(highlighter.update(None, &Rope::from_str(source), None));
        closed.lock().unwrap().clear();
        let (edit, typed) = replace(source, 11..11, " 1 ");
        highlighter.edit_tree(Some(edit), &Rope::from_str(&typed));
        highlighter.keep_colours_through(&Rope::from_str("fn other() {}\n"));
        assert_eq!(
            *closed.lock().unwrap(),
            ["move language=rust bytes=16", "move language=rust bytes=14"]
        );
    }

    /// The parse after the pause is a span, and one that a key typed
    /// meanwhile cancelled says so as it closes, where one that ran to its
    /// end says nothing of how it ended (elundqvist/kvist#589).
    #[test]
    fn test_a_parse_after_the_pause_says_whether_a_key_cancelled_it() {
        let (closed, _watching) = watch_perf_spans();
        let cancel = Arc::new(AtomicBool::new(false));
        drop(PauseParse::enter("rust", 90_000, &cancel));
        let cancel = Arc::new(AtomicBool::new(false));
        let cut_short = PauseParse::enter("rust", 91_000, &cancel);
        cancel.store(true, Ordering::Relaxed);
        drop(cut_short);
        assert_eq!(
            *closed.lock().unwrap(),
            [
                "parse language=rust bytes=90000",
                "parse language=rust bytes=91000 outcome=cancelled"
            ]
        );
    }

    #[test]
    fn test_unique_styles() {
        let red = color_style(gpui::red());
        let green = color_style(gpui::green());
        let blue = color_style(gpui::blue());
        let clean = HighlightStyle::default();

        assert_unique_styles(
            0..65,
            vec![
                (2..10, clean),
                (2..10, clean),
                (5..11, red),
                (2..6, clean),
                (10..15, green),
                (15..30, clean),
                (29..35, blue),
                (35..40, green),
                (45..60, blue),
            ],
            vec![
                (0..5, clean),
                (5..6, red),
                (6..10, red),
                (10..11, green),
                (11..15, green),
                (15..29, clean),
                (29..30, blue),
                (30..35, blue),
                (35..40, green),
                (40..45, clean),
                (45..60, blue),
                (60..65, clean),
            ],
        );

        assert_unique_styles(
            0..10,
            vec![(2..2, red), (4..6, green)],
            vec![(0..4, clean), (4..6, green), (6..10, clean)],
        );
    }
}
