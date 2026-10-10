use crate::input::InputModeKind;
use aho_corasick::AhoCorasick;
use gpui::{Context, Window};
use ropey::Rope;
use std::{ops::Range, rc::Rc};

use super::{InputBaseState, Replace, RopeExt as _, Search, movement::MoveDirection};

/// Stateful, presentation-independent search engine used by text inputs.
#[derive(Debug, Clone)]
pub struct SearchMatcher {
    text: Rope,
    pub query: Option<AhoCorasick>,
    matched_ranges: Rc<Vec<Range<usize>>>,
    current_match_ix: usize,
    replacing: bool,
}

#[derive(Debug, Clone)]
pub struct SearchSession {
    pub open: bool,
    pub replace_mode: bool,
    pub case_insensitive: bool,
    pub query: String,
    pub replacement: String,
    /// Where the search begins, set as the bar opens
    /// (`InputBaseState::search_start`): a query's first match is the first
    /// that begins here or after.
    pub anchor_offset: Option<usize>,
    /// The bar has opened on a query, remembered or the selection, and
    /// nothing has gone to its current match yet: Enter goes to that one
    /// where it is out of sight, and on to the next only where it is in
    /// view.
    pub(crate) unvisited: bool,
    pub matcher: SearchMatcher,
}

impl Default for SearchSession {
    fn default() -> Self {
        Self {
            open: false,
            replace_mode: false,
            case_insensitive: true,
            query: String::new(),
            replacement: String::new(),
            anchor_offset: None,
            unvisited: false,
            matcher: SearchMatcher::new(),
        }
    }
}

impl SearchSession {
    pub(crate) fn open(&mut self, replace_mode: bool, replaceable: bool) {
        self.open = true;
        self.replace_mode = replace_mode && replaceable;
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
    }

    pub(crate) fn update_query(&mut self, query: impl Into<String>, case_insensitive: bool) {
        self.query = query.into();
        self.case_insensitive = case_insensitive;
        self.matcher.update_query(&self.query, case_insensitive);
    }
}

impl<M: InputModeKind> InputBaseState<M> {
    /// Open the find bar. Its query is the selection, or else the one it
    /// last held, and its current match the first from where the search
    /// begins ([`Self::search_start`]); the view stays where it is, as VS
    /// Code's does until something is typed or Enter is pressed.
    pub fn open_search(&mut self, replace_mode: bool, cx: &mut Context<Self>) {
        if !self.searchable {
            return;
        }
        self.search_session
            .open(replace_mode, self.is_replaceable());
        let selected = self.selected_text().to_string();
        let seeded = !selected.is_empty();
        if seeded {
            self.search_session.query = selected;
        }
        let start = self.search_start(seeded);
        self.search_session.anchor_offset = Some(start);
        self.search_session.matcher.update_query(
            &self.search_session.query,
            self.search_session.case_insensitive,
        );
        self.search_session.matcher.update(&self.text);
        self.search_session.unvisited = self.search_session.matcher.select_from(start).is_some();
        cx.notify();
    }

    /// Where a search begins: the start of the selection, which is the
    /// caret where nothing is selected, as VS Code's and IntelliJ's begin
    /// there, so a selection the bar opens on is itself the current match
    /// and Enter goes to the one after it. With nothing selected and the
    /// caret out of sight, scrolled away from, it begins at the top of the
    /// view, with what is in sight. It began at the top of the view
    /// always: Ctrl+F on a selection further down made a match above it
    /// current, and Enter went back up to an earlier one
    /// (elundqvist/kvist#208).
    fn search_start(&self, seeded: bool) -> usize {
        let caret = self.selected_range.start.min(self.selected_range.end);
        let Some(layout) = self.last_layout.as_ref() else {
            return caret;
        };
        let view = &layout.visible_range_offset;
        if seeded || (view.start <= caret && caret <= view.end) {
            caret
        } else {
            view.start
        }
    }

    pub fn search_session(&self) -> &SearchSession {
        &self.search_session
    }

    #[doc(hidden)]
    pub fn set_search_replace_mode(&mut self, replace_mode: bool, cx: &mut Context<Self>) {
        self.search_session.replace_mode = replace_mode && self.is_replaceable();
        cx.notify();
    }

    /// Returns true if the search panel can replace the matches.
    ///
    /// This is false when the input is not `replaceable`, or when it is
    /// `disabled` or `readonly`.
    pub fn is_replaceable(&self) -> bool {
        self.replaceable && self.is_editable()
    }

    pub fn set_search_query(
        &mut self,
        query: impl Into<String>,
        case_insensitive: bool,
        cx: &mut Context<Self>,
    ) {
        self.search_session.update_query(query, case_insensitive);
        self.search_session.matcher.update(&self.text);
        self.follow_search_query(cx);
        cx.notify();
    }

    /// The match the query finds first from where the search began
    /// ([`Self::search_start`]), the first in the text where none is after
    /// it, is the current one and the view goes to it: as each character
    /// is typed, as VS Code's find widget and IntelliJ's search do, so
    /// Enter goes on to the one after it. The query's matches were counted
    /// from the first in the text, out of sight, and nothing moved until
    /// Enter, which went to the second (elundqvist/kvist#208).
    fn follow_search_query(&mut self, cx: &mut Context<Self>) {
        self.search_session.unvisited = false;
        let from = self
            .search_session
            .anchor_offset
            .unwrap_or_else(|| self.selected_range.start.min(self.selected_range.end));
        let Some(range) = self.search_session.matcher.select_from(from) else {
            return;
        };
        self.show_search_match(range.end, cx);
    }

    /// Bring the match at `offset` into view, up or down by where the view
    /// is now. [`Self::scroll_to`] keeps the view from going the other way
    /// than it is told, and the way was told by the matches' order alone:
    /// with the view below a match that came later in that order, as it is
    /// after Enter has gone down the text and the query is typed again,
    /// the bar named a match above the view and nothing moved
    /// (elundqvist/kvist#208).
    ///
    /// A fold that hides the match's line opens first, every one of them
    /// where they nest, as VS Code and IntelliJ open the fold the match
    /// gone to is in: folded, the bar counted to a match nobody could see,
    /// and neither what was typed nor Enter showed it.
    fn show_search_match(&mut self, offset: usize, cx: &mut Context<Self>) {
        let row = self.text.offset_to_point(offset).row;
        // a fold hides the lines between its first and its last
        let hiding: Vec<usize> = self
            .display_map
            .folded_ranges()
            .iter()
            .filter(|fold| row > fold.start_line && row < fold.end_line)
            .map(|fold| fold.start_line)
            .collect();
        for start_line in hiding {
            self.display_map.set_folded(start_line, false);
        }
        let direction = self.last_layout.as_ref().map(|layout| {
            let top = layout.line_height * self.display_map.buffer_line_to_display_row(row);
            if top < -self.scroll_handle.offset().y {
                MoveDirection::Up
            } else {
                MoveDirection::Down
            }
        });
        self.scroll_to(offset, direction, cx);
    }

    /// Whether the line `range` begins on lies whole inside the view, as
    /// last laid out and scrolled.
    fn search_match_in_view(&self, range: &Range<usize>) -> bool {
        let Some(layout) = self.last_layout.as_ref() else {
            return false;
        };
        let row = self.text.offset_to_point(range.start).row;
        if self.display_map.is_buffer_line_hidden(row) {
            return false;
        }
        let top = layout.line_height * self.display_map.buffer_line_to_display_row(row);
        let view_top = -self.scroll_handle.offset().y;
        top >= view_top && top + layout.line_height <= view_top + self.input_bounds.size.height
    }

    pub fn close_search(&mut self, cx: &mut Context<Self>) {
        self.search_session.close();
        cx.notify();
    }

    pub fn next_search_match(&mut self, cx: &mut Context<Self>) -> Option<Range<usize>> {
        // the bar opened on this query and has gone nowhere yet: its
        // current match, the first from where the search began, is gone to
        // and not past where it is out of sight, as VS Code's first Enter
        // goes to the first match after the caret
        let current = std::mem::take(&mut self.search_session.unvisited)
            .then(|| {
                let matcher = &self.search_session.matcher;
                matcher
                    .matched_ranges()
                    .get(matcher.current_match_index())
                    .cloned()
            })
            .flatten()
            .filter(|range| !self.search_match_in_view(range));
        let range = match current {
            Some(range) => range,
            None => self.search_session.matcher.next()?,
        };
        self.show_search_match(range.end, cx);
        Some(range)
    }

    pub fn previous_search_match(&mut self, cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.search_session.unvisited = false;
        let range = self.search_session.matcher.next_back()?;
        self.show_search_match(range.start, cx);
        Some(range)
    }

    pub fn replace_current_search_match(
        &mut self,
        replacement: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.is_replaceable() {
            return false;
        }
        let matcher = &mut self.search_session.matcher;
        let Some(range) = matcher
            .matched_ranges()
            .get(matcher.current_match_index())
            .cloned()
        else {
            return false;
        };
        let next = matcher.peek().unwrap_or_else(|| range.clone());
        let direction = matcher
            .has_next_without_wrap()
            .then_some(MoveDirection::Down);
        if direction.is_none() {
            matcher.set_current_match_index(0);
        }
        matcher.begin_replacement();
        let range_utf16 = self.range_to_utf16(&range);
        self.scroll_to(next.end, direction, cx);
        self.replace_text_in_range_silent(Some(range_utf16), replacement, window, cx);
        true
    }

    pub fn replace_all_search_matches(
        &mut self,
        replacement: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        if !self.is_replaceable() {
            return 0;
        }
        let ranges = self.search_session.matcher.matched_ranges();
        if ranges.is_empty() {
            return 0;
        }
        let mut text = self.text.clone();
        for range in ranges.iter().rev() {
            text.replace(range.clone(), replacement);
        }
        self.search_session.matcher.begin_replacement();
        let count = ranges.len();
        self.replace_text_in_range_silent(Some(0..self.text.len()), &text.to_string(), window, cx);
        self.scroll_to(0, Some(MoveDirection::Down), cx);
        count
    }

    pub(super) fn update_search(&mut self, _cx: &mut gpui::App) {
        self.search_session.matcher.update(&self.text);
    }

    pub(super) fn on_action_search(&mut self, _: &Search, _: &mut Window, cx: &mut Context<Self>) {
        if !self.searchable {
            return;
        }
        self.open_search(false, cx);
    }

    pub(super) fn on_action_replace(
        &mut self,
        _: &Replace,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.searchable {
            return;
        }
        self.open_search(true, cx);
    }
}

impl Default for SearchMatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchMatcher {
    pub fn new() -> Self {
        Self {
            text: "".into(),
            query: None,
            matched_ranges: Rc::new(Vec::new()),
            current_match_ix: 0,
            replacing: false,
        }
    }

    /// Update the source text and recompute matches.
    pub fn update(&mut self, text: &Rope) {
        if self.text.eq(text) {
            self.replacing = false;
            return;
        }
        self.text = text.clone();
        self.update_matches();
    }

    pub fn update_query(&mut self, query: &str, case_insensitive: bool) {
        self.query = (!query.is_empty()).then(|| {
            AhoCorasick::builder()
                .ascii_case_insensitive(case_insensitive)
                .build([query])
                .expect("failed to build input search query")
        });
        self.update_matches();
    }

    pub fn matched_ranges(&self) -> Rc<Vec<Range<usize>>> {
        self.matched_ranges.clone()
    }

    pub fn current_match_index(&self) -> usize {
        self.current_match_ix
    }

    pub fn len(&self) -> usize {
        self.matched_ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.matched_ranges.is_empty()
    }

    pub fn label(&self) -> String {
        if self.is_empty() {
            "0/0".into()
        } else {
            format!("{}/{}", self.current_match_ix + 1, self.len())
        }
    }

    fn peek(&self) -> Option<Range<usize>> {
        self.next_index()
            .and_then(|ix| self.matched_ranges.get(ix).cloned())
    }

    fn has_next_without_wrap(&self) -> bool {
        self.current_match_ix < self.matched_ranges.len().saturating_sub(1)
    }

    pub fn update_cursor_by_offset(&mut self, offset: usize) {
        for (ix, range) in self.matched_ranges.iter().enumerate() {
            self.current_match_ix = ix;
            if range.contains(&offset) || range.end >= offset {
                return;
            }
        }
    }

    /// Make the first match that begins at `offset` or after it the
    /// current one, the first in the text where none does, as a search
    /// that reaches the end goes on from the top, and hand it back.
    pub fn select_from(&mut self, offset: usize) -> Option<Range<usize>> {
        if self.matched_ranges.is_empty() {
            return None;
        }
        let ix = self
            .matched_ranges
            .partition_point(|range| range.start < offset);
        self.current_match_ix = if ix == self.matched_ranges.len() {
            0
        } else {
            ix
        };
        self.matched_ranges.get(self.current_match_ix).cloned()
    }

    /// Preserve the current logical match while a replacement mutates text.
    fn begin_replacement(&mut self) {
        self.replacing = true;
    }

    fn set_current_match_index(&mut self, index: usize) {
        self.current_match_ix = index.min(self.matched_ranges.len().saturating_sub(1));
    }

    fn next_index(&self) -> Option<usize> {
        if self.is_empty() {
            None
        } else if self.has_next_without_wrap() {
            Some(self.current_match_ix + 1)
        } else {
            Some(0)
        }
    }

    fn update_matches(&mut self) {
        let mut ranges = Vec::new();
        if let Some(query) = &self.query {
            let text = self.text.to_string();
            ranges.extend(
                query
                    .stream_find_iter(text.as_bytes())
                    .map(|result| result.expect("input search match").range()),
            );
        }
        self.matched_ranges = Rc::new(ranges);
        if !self.replacing || self.is_empty() {
            self.current_match_ix = 0;
        } else {
            self.current_match_ix = self.current_match_ix.min(self.len() - 1);
        }
        self.replacing = false;
    }
}

impl Iterator for SearchMatcher {
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        let ix = self.next_index()?;
        self.current_match_ix = ix;
        self.matched_ranges.get(ix).cloned()
    }
}

impl DoubleEndedIterator for SearchMatcher {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.is_empty() {
            return None;
        }
        if self.current_match_ix == 0 {
            self.current_match_ix = self.len();
        }
        self.current_match_ix -= 1;
        self.matched_ranges.get(self.current_match_ix).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_navigates_and_preserves_replacement_position() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("foo FOO foo"));
        matcher.update_query("foo", true);
        assert_eq!(&*matcher.matched_ranges(), &[0..3, 4..7, 8..11]);
        assert_eq!(matcher.next(), Some(4..7));
        assert_eq!(matcher.next_back(), Some(0..3));

        matcher.set_current_match_index(2);
        matcher.begin_replacement();
        matcher.update(&Rope::from("foo FOO bar"));
        assert_eq!(matcher.current_match_index(), 1);
    }

    #[test]
    fn next_wraps_to_start() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from(".....aaaaa.....aaaaa.....aaaaa"));
        matcher.update_query("aaaaa", false);
        matcher.set_current_match_index(2);
        assert_eq!(matcher.next(), Some(5..10));
    }

    #[test]
    fn replacement_keeps_current_match_index_on_next_match() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("foo foo foo"));
        matcher.update_query("foo", true);
        assert_eq!(matcher.label(), "1/3");

        assert!(matcher.has_next_without_wrap());
        matcher.begin_replacement();
        matcher.update(&Rope::from("bar foo foo"));
        assert_eq!(matcher.current_match_index(), 0);
        assert_eq!(matcher.matched_ranges()[0], 4..7);
        assert_eq!(matcher.label(), "1/2");

        matcher.set_current_match_index(1);
        assert!(!matcher.has_next_without_wrap());
        matcher.set_current_match_index(0);
        matcher.begin_replacement();
        matcher.update(&Rope::from("bar foo bar"));
        assert_eq!(matcher.current_match_index(), 0);
        assert_eq!(matcher.matched_ranges()[0], 4..7);
        assert_eq!(matcher.label(), "1/1");
    }

    #[test]
    fn update_matches_clamps_current_match_index_while_replacing() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("foo foo foo"));
        matcher.update_query("foo", true);
        matcher.set_current_match_index(2);
        matcher.begin_replacement();

        matcher.update(&Rope::from("foo xoo foo"));

        assert_eq!(matcher.len(), 2);
        assert_eq!(matcher.current_match_index(), 1);
        assert_eq!(matcher.label(), "2/2");
    }
}
