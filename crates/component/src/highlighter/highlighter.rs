#[cfg(test)]
use crate::highlighter::HighlightTheme;
use crate::highlighter::LanguageRegistry;

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

/// A syntax highlighter that supports incremental parsing, multiline text,
/// and caching of highlight results.
#[allow(unused)]
pub struct SyntaxHighlighter {
    language: SharedString,
    query: Option<Query>,
    /// The full injections query. This is used to build injection layers during parsing.
    injections_query: Option<Arc<Query>>,

    locals_pattern_index: usize,
    highlights_pattern_index: usize,
    // highlight_indices: Vec<Option<Highlight>>,
    non_local_variable_patterns: Vec<bool>,
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
    injection_layers: Vec<InjectionLayer>,
    /// How long one injected span may parse in update().
    injection_parse_timeout: Duration,
    /// `tree` was edited to the text but not parsed again, as it is while
    /// a large file waits for the background parse or a parse ran out of
    /// time: what the edits touched in it says nothing true about what is
    /// there now, and is left plain until a parse finishes.
    stale: bool,
    /// Where the edits since the last finished parse lie in the text. A
    /// node an edit lands in has changes, but so has the first node after
    /// an edit in the space before it, which the edit did not touch: only
    /// a node one of these reaches is left plain.
    edited: Vec<Range<usize>>,
}

/// A parsed injection layer.
/// Stores the parsed tree and the ranges it covers.
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
}

pub(crate) struct ReusableInjectionLayer {
    pub(crate) language_name: SharedString,
    highlight_query: Arc<Query>,
    pub(crate) ranges: Vec<tree_sitter::Range>,
    pub(crate) tree: Tree,
}

struct TextProvider<'a>(&'a Rope);
struct ByteChunks<'a> {
    cursor: ChunkCursor<'a>,
    node_start: usize,
    node_end: usize,
    at_first: bool,
}
impl<'a> tree_sitter::TextProvider<&'a [u8]> for TextProvider<'a> {
    type I = ByteChunks<'a>;

    fn text(&mut self, node: tree_sitter::Node) -> Self::I {
        let range = node.byte_range();
        let cursor = self.0.chunk_cursor_at(range.start);

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

    /// Build an inert highlighter that never parses and creates no styles,
    /// for languages without a grammar.
    fn build_inert(language: SharedString) -> Self {
        Self {
            language,
            query: None,
            injections_query: None,
            locals_pattern_index: 0,
            highlights_pattern_index: 0,
            non_local_variable_patterns: Vec::new(),
            injection_content_capture_index: None,
            injection_language_capture_index: None,
            local_scope_capture_index: None,
            local_def_capture_index: None,
            local_def_value_capture_index: None,
            local_ref_capture_index: None,
            text: Rope::new(),
            parser: Parser::new(),
            tree: None,
            injection_layers: Vec::new(),
            injection_parse_timeout: INJECTION_PARSE_TIMEOUT,
            stale: false,
            edited: Vec::new(),
        }
    }

    /// Build the highlighter for the given language.
    ///
    /// https://github.com/tree-sitter/tree-sitter/blob/v0.26.8/crates/highlight/src/highlight.rs#L339
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

        // Concatenate the query strings, keeping track of the start offset of each section.
        let mut query_source = String::new();
        query_source.push_str(&config.injections);
        let locals_query_offset = query_source.len();
        query_source.push_str(&config.locals);
        let highlights_query_offset = query_source.len();
        query_source.push_str(&config.highlights);

        // Construct a single query by concatenating the three query strings, but record the
        // range of pattern indices that belong to each individual string.
        let mut query = Query::new(grammar, &query_source).context("new query")?;

        let mut locals_pattern_index = 0;
        let mut highlights_pattern_index = 0;
        for i in 0..(query.pattern_count()) {
            let pattern_offset = query.start_byte_for_pattern(i);
            if pattern_offset < highlights_query_offset {
                if pattern_offset < highlights_query_offset {
                    highlights_pattern_index += 1;
                }
                if pattern_offset < locals_query_offset {
                    locals_pattern_index += 1;
                }
            }
        }

        let injections_query = if !config.injections.is_empty() {
            Query::new(grammar, &config.injections).ok().map(Arc::new)
        } else {
            None
        };

        // Injection layers are computed separately during parsing, so do not
        // emit injection captures from the main highlight query.
        for pattern_index in 0..locals_pattern_index {
            query.disable_pattern(pattern_index);
        }

        // Find all of the highlighting patterns that are disabled for nodes that
        // have been identified as local variables.
        let non_local_variable_patterns = (0..query.pattern_count())
            .map(|i| {
                query
                    .property_predicates(i)
                    .iter()
                    .any(|(prop, positive)| !*positive && prop.key.as_ref() == "local")
            })
            .collect();

        // Store the numeric ids for all of the special captures.
        let injection_content_capture_index = injections_query.as_ref().and_then(|q| {
            q.capture_names()
                .iter()
                .position(|name| *name == "injection.content")
                .map(|i| i as u32)
        });
        let injection_language_capture_index = injections_query.as_ref().and_then(|q| {
            q.capture_names()
                .iter()
                .position(|name| *name == "injection.language")
                .map(|i| i as u32)
        });
        let mut local_def_capture_index = None;
        let mut local_def_value_capture_index = None;
        let mut local_ref_capture_index = None;
        let mut local_scope_capture_index = None;
        for (i, name) in query.capture_names().iter().enumerate() {
            let i = Some(i as u32);
            match *name {
                "local.definition" => local_def_capture_index = i,
                "local.definition-value" => local_def_value_capture_index = i,
                "local.reference" => local_ref_capture_index = i,
                "local.scope" => local_scope_capture_index = i,
                _ => {}
            }
        }

        // let highlight_indices = vec![None; query.capture_names().len()];

        Ok(Self {
            language: config.name.clone(),
            query: Some(query),
            injections_query,

            locals_pattern_index,
            highlights_pattern_index,
            non_local_variable_patterns,
            injection_content_capture_index,
            injection_language_capture_index,
            local_scope_capture_index,
            local_def_capture_index,
            local_def_value_capture_index,
            local_ref_capture_index,
            text: Rope::new(),
            parser,
            tree: None,
            injection_layers: Vec::new(),
            injection_parse_timeout: INJECTION_PARSE_TIMEOUT,
            stale: false,
            edited: Vec::new(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.text.len() == 0
    }

    /// Get the parsed tree (if available)
    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    /// Apply only the structural `edit` to the existing tree and update the stored text,
    /// without re-parsing. The tree and every layer are then stale: what
    /// the edit touched in them is left plain until a parse finishes, since
    /// a token an edit lands in grows over all it inserts, and the rest
    /// keeps its colours, moved with the text.
    pub fn edit_tree(&mut self, edit: Option<InputEdit>, text: &Rope) {
        match edit {
            Some(edit) => {
                if let Some(tree) = self.tree.as_mut() {
                    tree.edit(&edit);
                }
                self.edit_injection_layers(&edit);
                self.note_edit(&edit);
            }
            // the whole text is new, and the tree knows none of it: it is
            // plain until the parse, which starts afresh, rather than
            // coloured as the old text was where the old tree's nodes
            // happen to fall, and parsed again from a tree never told of
            // the change
            None => {
                self.tree = None;
                self.injection_layers.clear();
                self.edited.clear();
            }
        }
        self.mark_stale();
        self.text = text.clone();
    }

    /// The tree and every layer were edited to the text and not parsed
    /// again.
    fn mark_stale(&mut self) {
        self.stale = true;
        for layer in &mut self.injection_layers {
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
            if r.is_empty() {
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
        let (start, end) = (node.start_byte(), node.end_byte());
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

    /// Apply `edit` to every injection layer as to the main tree: its tree is
    /// edited and its ranges move with the text. A layer is then found again
    /// at its new place and parsed again only as far as the edit reaches, and
    /// an edit that leaves a span's length as it was (an overtype) is parsed
    /// again rather than hidden by an old tree that never saw it.
    fn edit_injection_layers(&mut self, edit: &InputEdit) {
        for layer in &mut self.injection_layers {
            layer.tree.edit(edit);
            for range in &mut layer.ranges {
                edit.edit_range(range);
            }
            if let Some(byte_range) = bounding_byte_range(&layer.ranges) {
                layer.byte_range = byte_range;
            }
        }
    }

    /// Returns the language name for this highlighter.
    pub fn language(&self) -> &SharedString {
        &self.language
    }

    /// Returns a reference to the current text.
    pub fn text(&self) -> &Rope {
        &self.text
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
    /// When `timeout` is `None`, parsing runs to completion and always returns `true`.
    pub fn update(
        &mut self,
        edit: Option<InputEdit>,
        text: &Rope,
        timeout: Option<Duration>,
    ) -> bool {
        if self.text.eq(text) {
            return true;
        }

        // If there's no grammar for the language, just update the text.
        if self.parser.language().is_none() {
            self.text = text.clone();
            return true;
        }

        let edit = edit.unwrap_or(InputEdit {
            start_byte: 0,
            old_end_byte: 0,
            new_end_byte: text.len(),
            start_position: Point::new(0, 0),
            old_end_position: Point::new(0, 0),
            new_end_position: Point::new(0, 0),
        });

        let mut old_tree = self
            .tree
            .take()
            .unwrap_or(self.parser.parse("", None).unwrap());
        old_tree.edit(&edit);
        self.edit_injection_layers(&edit);
        self.note_edit(&edit);

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
            // Restore the old tree so highlighting continues with stale data.
            self.tree = Some(old_tree);
            self.text = text.clone();
            self.mark_stale();
            return false;
        }

        let new_tree = new_tree.unwrap();
        self.tree = Some(new_tree.clone());
        self.stale = false;
        self.text = text.clone();
        let injections_finished = self.parse_injection_layers(&new_tree);
        if injections_finished {
            self.edited.clear();
        }
        // Only a caller with a time limit parses again in the background.
        injections_finished || timeout.is_none()
    }

    /// Returns the data needed to compute injection layers on a background thread.
    /// Returns `None` if this language has no injections.
    pub(crate) fn injection_parse_data(&self) -> Option<InjectionParseData> {
        let query = self.injections_query.clone()?;
        Some(InjectionParseData {
            query,
            content_capture_index: self.injection_content_capture_index,
            language_capture_index: self.injection_language_capture_index,
            old_layers: self
                .injection_layers
                .iter()
                .map(|layer| ReusableInjectionLayer {
                    language_name: layer.language_name.clone(),
                    highlight_query: layer.highlight_query.clone(),
                    ranges: layer.ranges.clone(),
                    tree: layer.tree.clone(),
                })
                .collect(),
            parse_timeout: Some(self.injection_parse_timeout),
            cancel: None,
        })
    }

    /// Compute injection layers from a freshly-parsed main tree, and whether
    /// every span's parse finished in time.
    /// This is pure computation with no side effects and is safe to run on a
    /// background thread.
    pub(crate) fn compute_injection_layers(
        data: InjectionParseData,
        tree: &Tree,
        text: &Rope,
    ) -> (Vec<InjectionLayer>, bool) {
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

        fn resolve_language(
            language_name: &str,
            query_cache: &mut HashMap<SharedString, Arc<Query>>,
        ) -> Option<(SharedString, Arc<Query>)> {
            let config = LanguageRegistry::singleton().language(language_name)?;
            if let Some(query) = query_cache.get(&config.name) {
                return Some((config.name, query.clone()));
            }

            let query = match Query::new(config.language.as_ref()?, &config.highlights) {
                Ok(query) => Arc::new(query),
                Err(error) => {
                    tracing::error!(
                        "failed to build injection query for {:?}: {:?}",
                        config.name,
                        error
                    );
                    return None;
                }
            };
            query_cache.insert(config.name.clone(), query.clone());
            Some((config.name, query))
        }

        let root_node = tree.root_node();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&data.query, root_node, TextProvider(text));

        let mut combined_ranges: HashMap<SharedString, CombinedRanges> = HashMap::new();
        let old_layer_trees: HashMap<_, _> = data
            .old_layers
            .iter()
            .map(|layer| {
                (
                    (layer.language_name.clone(), ranges_cache_key(&layer.ranges)),
                    &layer.tree,
                )
            })
            .collect();
        let front_moved_old_layer_trees: HashMap<_, _> = data
            .old_layers
            .iter()
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
                    return exact.copied();
                }
                front_moved_old_layer_trees
                    .get(&(language_name.clone(), front_moved_key(ranges)))
                    .copied()
            };
        let mut finished = true;
        // Query objects are relatively expensive. Reuse one Arc per language
        // from the previous parse and compile only languages present in this
        // document, rather than eagerly retaining every registered grammar.
        let mut highlight_queries: HashMap<SharedString, Arc<Query>> = data
            .old_layers
            .iter()
            .map(|layer| (layer.language_name.clone(), layer.highlight_query.clone()))
            .collect();
        // Cache raw names as well as canonical queries. Otherwise every fence with the
        // same info string would lock the registry and clone its language configuration.
        let mut resolved_languages: HashMap<SharedString, Option<(SharedString, Arc<Query>)>> =
            HashMap::new();
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

            // Skip rather than break, so later combined ranges are still collected.
            if !combined && non_combined_parses >= MAX_NON_COMBINED_INJECTION_PARSES {
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
                    let resolved = resolve_language(&raw_language_name, &mut highlight_queries);
                    resolved_languages.insert(raw_language_name, resolved.clone());
                    resolved
                };
            let Some((language_name, highlight_query)) = resolved_language else {
                continue;
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

                non_combined_parses += 1;
                let old_tree = old_tree_for(&language_name, &ranges, false);
                if let Some(layer) = Self::parse_injection_layer(
                    &language_name,
                    highlight_query,
                    ranges,
                    old_tree,
                    text,
                    &data,
                    &mut finished,
                ) {
                    new_layers.push(layer);
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
            if let Some(layer) = Self::parse_injection_layer(
                &language_name,
                highlight_query,
                ranges,
                old_tree,
                text,
                &data,
                &mut finished,
            ) {
                new_layers.push(layer);
            }
        }
        new_layers.sort_by_key(|layer| layer.byte_range.start);
        (new_layers, finished)
    }

    /// Parse one injection layer over the given included ranges, from the
    /// span's old tree (edited to the text) when there is one and the
    /// language's incremental parse can be trusted (`injection_parses_from_scratch`).
    ///
    /// A parse that runs out of the data's `parse_timeout`, or is cancelled,
    /// clears `finished` and keeps the old tree as a stale layer, so the span
    /// keeps its last highlights where the edits did not touch it until a
    /// parse of it finishes, instead of going plain; a span with no old tree
    /// is left out.
    fn parse_injection_layer(
        language_name: &SharedString,
        highlight_query: Arc<Query>,
        ranges: Vec<tree_sitter::Range>,
        old_tree: Option<&Tree>,
        text: &Rope,
        data: &InjectionParseData,
        finished: &mut bool,
    ) -> Option<InjectionLayer> {
        let config = LanguageRegistry::singleton().language(language_name)?;
        let mut parser = Parser::new();
        parser.set_language(config.language.as_ref()?).ok()?;
        parser.set_included_ranges(&ranges).ok()?;
        let parse_start = Instant::now();
        let mut timed_out = false;
        let mut progress = |_: &tree_sitter::ParseState| -> ControlFlow<()> {
            if data
                .parse_timeout
                .is_some_and(|timeout| parse_start.elapsed() > timeout)
                || data
                    .cancel
                    .as_ref()
                    .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
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
            _ => {
                *finished = false;
                (old_tree?.clone(), true)
            }
        };

        let byte_range = bounding_byte_range(&ranges)?;
        Some(InjectionLayer {
            language_name: language_name.clone(),
            highlight_query,
            ranges,
            byte_range,
            tree,
            stale,
        })
    }

    /// Apply a tree that was parsed on a background thread.
    ///
    /// `injection_layers` must also be pre-computed in the background via
    /// [`compute_injection_layers`] to avoid blocking the main thread.
    pub(crate) fn apply_background_tree(
        &mut self,
        tree: Tree,
        text: &Rope,
        injection_layers: Vec<InjectionLayer>,
    ) {
        // Only apply if the text still matches what was parsed.
        if !self.text.eq(text) {
            return;
        }

        self.tree = Some(tree);
        self.stale = false;
        self.injection_layers = injection_layers;
        self.edited.clear();
    }

    /// Parse injection layers after the main tree is updated, and return
    /// whether every span's parse finished in time.
    /// pattern: parse once in update, query many times in render.
    fn parse_injection_layers(&mut self, tree: &Tree) -> bool {
        let Some(data) = self.injection_parse_data() else {
            self.injection_layers.clear();
            return true;
        };
        let (layers, finished) = Self::compute_injection_layers(data, tree, &self.text.clone());
        self.injection_layers = layers;
        finished
    }

    /// Match the visible ranges of nodes in the Tree for highlighting.
    fn match_styles(&self, range: Range<usize>) -> Vec<HighlightItem> {
        let mut highlights = vec![];
        let mut injection_highlights = vec![];
        let Some(tree) = &self.tree else {
            return highlights;
        };

        let Some(query) = &self.query else {
            return highlights;
        };

        let root_node = tree.root_node();
        let source = &self.text;

        // Query pre-parsed injection layers.
        let mut last_layer_start = 0;
        for layer in &self.injection_layers {
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

            // A layer's tree has nothing to say outside the layer's span.
            let mut query_cursor = QueryCursor::new();
            query_cursor.set_byte_range(
                range.start.max(layer.byte_range.start)..range.end.min(layer.byte_range.end),
            );

            let mut matches =
                query_cursor.matches(query, layer.tree.root_node(), TextProvider(&self.text));

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
                    if layer.stale && self.touched(&cap.node) {
                        continue;
                    }

                    let node_range = cap.node.start_byte()..cap.node.end_byte();

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

        let query_nodes = collect_query_nodes(root_node, &range);

        for query_node in &query_nodes {
            let mut query_cursor = QueryCursor::new();
            query_cursor.set_byte_range(range.clone());

            let mut matches = query_cursor.matches(&query, *query_node, TextProvider(&source));

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
fn collect_query_nodes<'a>(
    root: tree_sitter::Node<'a>,
    range: &Range<usize>,
) -> Vec<tree_sitter::Node<'a>> {
    let mut nodes = Vec::new();
    collect_query_nodes_inner(root, range, &mut nodes);
    if nodes.is_empty() {
        nodes.push(root);
    }
    nodes
}

fn collect_query_nodes_inner<'a>(
    node: tree_sitter::Node<'a>,
    range: &Range<usize>,
    out: &mut Vec<tree_sitter::Node<'a>>,
) {
    // Skip nodes entirely outside the range.
    if node.end_byte() <= range.start || node.start_byte() >= range.end {
        return;
    }

    let node_span = node.end_byte() - node.start_byte();
    let range_span = range.end - range.start;

    // Use `goto_first_child_for_byte` to seek directly to the first
    // overlapping child instead of iterating all children from the start.
    if node_span > range_span + LARGE_NODE_THRESHOLD && node.child_count() > 0 {
        let mut cursor = node.walk();
        if cursor.goto_first_child_for_byte(range.start).is_some() {
            loop {
                let child = cursor.node();
                if child.start_byte() >= range.end {
                    break;
                }
                collect_query_nodes_inner(child, range, out);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        return;
    }

    out.push(node);
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
mod tests {
    use gpui::Hsla;

    use super::*;
    use crate::Colorize as _;

    fn color_style(color: Hsla) -> HighlightStyle {
        let mut style = HighlightStyle::default();
        style.color = Some(color);
        style
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

    #[cfg(feature = "tree-sitter-languages")]
    fn fresh_highlighter(language: &str, source: &str) -> SyntaxHighlighter {
        let mut highlighter = SyntaxHighlighter::new(language);
        assert!(highlighter.update(None, &Rope::from_str(source), None));
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
            for layer in &highlighter.injection_layers {
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
        let mut text = |len: usize, next: &mut dyn FnMut(usize) -> usize| -> String {
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
