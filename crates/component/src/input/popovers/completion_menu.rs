use std::{cell::Cell, cmp::Reverse, collections::HashMap, ops::Range, rc::Rc};

use gpui::{
    Action, AnyElement, App, AppContext, AvailableSpace, Bounds, Context, DismissEvent,
    DispatchPhase, Element, ElementId, Empty, Entity, EventEmitter, FontId, FontWeight,
    GlobalElementId, Half as _, HighlightStyle, Hsla, InspectorElementId, InteractiveElement as _,
    IntoElement, LayoutId, MouseDownEvent, ParentElement, Pixels, Rems, Render, RenderOnce,
    ScrollHandle, ScrollStrategy, SharedString, StatefulInteractiveElement as _, Style, Styled,
    StyledText, Subscription, WeakEntity, Window, WindowTextSystem, deferred, div, point,
    prelude::FluentBuilder, px, relative, rems, size,
};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, Documentation, MarkupKind,
};

const MAX_MENU_HEIGHT: Pixels = px(240.);
const MIN_MENU_WIDTH: Pixels = px(120.);
/// The popover's padding around the list. It is `editor_popover`'s own,
/// set here as well because where the popover fits is worked out from it.
const MENU_PADDING: Rems = rems(0.25);
/// Between the list and its documentation, and between either and the line
/// the caret is on.
const POPOVER_GAP: Pixels = px(4.);
/// Kept clear between a popover and the window's edge.
const WINDOW_MARGIN: Pixels = px(4.);
/// How tall the documentation panel grows before it scrolls.
const MAX_DOCS_HEIGHT: Pixels = px(320.);
/// The narrowest the documentation panel is drawn beside the list. With
/// less room than this on either side it goes above or below the list.
const MIN_DOCS_WIDTH: Pixels = px(240.);
/// The least room the documentation panel takes past the list, away from
/// the caret, before it goes to the caret's other side instead; and the
/// least of the editor's height it is kept inside, else the window's.
const MIN_DOCS_HEIGHT: Pixels = px(80.);
/// The room right of the list that the documentation panel takes before
/// the left is tried, as IntelliJ and VS Code put it on the right: in a
/// window where it does not fit whole on either side, the left is mostly a
/// file tree.
const PREFERRED_DOCS_WIDTH: Pixels = px(320.);
/// The least room for the list inside the editor on the side of the
/// caret's line with more of it: a list taller than both sides is shrunk to
/// that, and scrolls, rather than run past the editor. With less, it goes
/// where the window has room.
const MIN_LIST_HEIGHT: Pixels = px(60.);
/// How many rows are laid out to find the widest: those [`RowWidths`] ranks
/// widest, so that a long list costs this many layouts and not one per row.
const MEASURED_ROWS: usize = 32;
/// A row's text size, and the gap between its parts. Shared by the row and
/// by [`RowWidths`], which must see the row as it is drawn.
const ROW_TEXT_SIZE: Rems = rems(0.75);
const ROW_GAP: Rems = rems(0.5);
/// A row's line, taller than its text so that what hangs below the
/// baseline, an underscore most of all, is inside it: a name and its
/// signature cut short at the menu's edge are clipped to it, and at a line
/// as tall as the text `clone_from` read `clone from`.
const ROW_LINE_HEIGHT: f32 = 1.25;
/// The kind column: a square as tall as the row's text, its letter smaller.
const KIND_SIZE: Rems = rems(0.875);
const KIND_TEXT_SIZE: Rems = rems(0.625);

use crate::{
    ActiveTheme, IndexPath, Selectable, actions, h_flex,
    input::{
        self, EditorState,
        popovers::{editor_popover, render_markdown},
    },
    label::Label,
    list::{List, ListDelegate, ListEvent, ListState},
    scroll::ScrollableElement as _,
};

struct ContextMenuDelegate {
    query: SharedString,
    menu: Entity<CompletionMenu>,
    items: Vec<Rc<CompletionItem>>,
    /// Where each item's label matches what was typed, as its provider
    /// said; `None` where it said nothing.
    matches: Vec<Option<Rc<[Range<usize>]>>>,
    show_kinds: bool,
    /// Kept clear at the right of each row for the scrollbar, drawn over
    /// the rows' ends, while the list scrolls.
    gutter: Pixels,
    selected_ix: usize,
}

/// How long a row's text is, which picks the list's sample row: the one it
/// takes its row height from. Characters, not bytes: `·` is two bytes and
/// `→` three, and neither is wider than a letter. The menu's width does not
/// come from this row; see [`CompletionMenu::measure_width`].
fn row_chars(item: &CompletionItem) -> usize {
    let chars = |s: Option<&str>| s.map_or(0, |s| s.chars().count());
    let signature = item
        .label_details
        .as_ref()
        .and_then(|d| d.detail.as_deref());
    item.label.chars().count() + chars(signature) + chars(right_column(item))
}

/// What a row shows at its right: the item's `labelDetails.description`, a
/// type or a path, else its `detail`. Without `labelDetails` the detail is
/// drawn straight after the label instead, as before the menu laid them out.
fn right_column(item: &CompletionItem) -> Option<&str> {
    match &item.label_details {
        Some(details) => details
            .description
            .as_deref()
            .filter(|d| !d.is_empty())
            .or(item.detail.as_deref()),
        None => item.detail.as_deref(),
    }
    .filter(|d| !d.is_empty())
}

/// Whether `a` and `b` are the same row, of a list and of one handed after
/// it: the same name, signature, type and kind, putting in the same text.
/// What resolving adds, the documentation and the detail, and what a
/// provider keeps in `data` to know an item by, may differ.
fn same_row(a: &CompletionItem, b: &CompletionItem) -> bool {
    let text = |item: &CompletionItem| match &item.text_edit {
        Some(CompletionTextEdit::Edit(edit)) => Some(edit.new_text.clone()),
        Some(CompletionTextEdit::InsertAndReplace(edit)) => Some(edit.new_text.clone()),
        None => item.insert_text.clone(),
    };
    a.label == b.label
        && a.label_details == b.label_details
        && a.kind == b.kind
        && text(a) == text(b)
}

/// The letter an item's kind is drawn as in the kind column, and which of
/// the theme's colours: callables magenta, values blue, types yellow,
/// interfaces green, modules and files cyan, snippets red, and keywords
/// and text muted. Letters repeat only across colours: a function and a
/// field are both `f`, as in IntelliJ.
fn kind_letter(kind: CompletionItemKind) -> Option<(&'static str, KindColor)> {
    use CompletionItemKind as K;
    use KindColor::*;
    Some(match kind {
        K::METHOD => ("m", Callable),
        K::FUNCTION => ("f", Callable),
        K::CONSTRUCTOR => ("c", Callable),
        K::EVENT => ("e", Callable),
        K::OPERATOR => ("o", Callable),
        K::FIELD => ("f", Value),
        K::VARIABLE => ("v", Value),
        K::PROPERTY => ("p", Value),
        K::CONSTANT => ("K", Value),
        K::ENUM_MEMBER => ("e", Value),
        K::REFERENCE => ("r", Value),
        K::CLASS => ("C", Type),
        K::STRUCT => ("S", Type),
        K::ENUM => ("E", Type),
        K::TYPE_PARAMETER => ("T", Type),
        K::INTERFACE => ("I", Interface),
        K::MODULE => ("M", Module),
        K::FILE => ("F", Module),
        K::FOLDER => ("D", Module),
        K::SNIPPET => ("s", Snippet),
        K::KEYWORD => ("k", Plain),
        K::TEXT => ("t", Plain),
        K::VALUE => ("v", Plain),
        K::UNIT => ("u", Plain),
        K::COLOR => ("#", Plain),
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KindColor {
    Callable,
    Value,
    Type,
    Interface,
    Module,
    Snippet,
    Plain,
}

fn kind_color(color: KindColor, cx: &App) -> Hsla {
    let theme = cx.theme();
    match color {
        KindColor::Callable => theme.magenta,
        KindColor::Value => theme.blue,
        KindColor::Type => theme.yellow,
        KindColor::Interface => theme.green,
        KindColor::Module => theme.cyan,
        KindColor::Snippet => theme.red,
        KindColor::Plain => theme.muted_foreground,
    }
}

/// The byte ranges of `label` painted as matched: those its provider gave,
/// else its first `prefix` bytes. Each is cut to the label and to character
/// boundaries, and one that overlaps the one before is left out.
fn label_highlights(
    label: &str,
    matches: Option<&[Range<usize>]>,
    prefix: usize,
) -> Vec<Range<usize>> {
    let floor = |mut b: usize| {
        b = b.min(label.len());
        while !label.is_char_boundary(b) {
            b -= 1;
        }
        b
    };
    let prefix = [0..prefix];
    let mut out: Vec<Range<usize>> = Vec::new();
    for r in matches.unwrap_or(&prefix) {
        let (start, end) = (floor(r.start), floor(r.end));
        if start < end && out.last().is_none_or(|last| last.end <= start) {
            out.push(start..end);
        }
    }
    out
}

/// What the documentation panel says of the highlighted row.
#[derive(Clone, Debug, PartialEq, Eq)]
enum DocsBody {
    Markdown(SharedString),
    /// Plain text, drawn as it is: `*` and `_` in it are characters.
    Plain(SharedString),
}

/// What the documentation panel shows for `item`: its `detail` when the
/// row shows something else (a `labelDetails.description`), in the row's
/// font, and its documentation. None when there is neither, so that an item
/// without documentation has no panel, not an empty one.
fn docs_of(item: &CompletionItem) -> Option<(Option<SharedString>, Option<DocsBody>)> {
    let header = item
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty() && Some(*d) != right_column(item).map(str::trim))
        .map(|d| SharedString::from(d.to_string()));
    let body = match &item.documentation {
        Some(Documentation::String(text)) => Some(DocsBody::Plain(text.clone().into())),
        Some(Documentation::MarkupContent(content)) => Some(match content.kind {
            MarkupKind::Markdown => DocsBody::Markdown(content.value.clone().into()),
            MarkupKind::PlainText => DocsBody::Plain(content.value.clone().into()),
        }),
        None => None,
    }
    .filter(|body| match body {
        DocsBody::Markdown(text) | DocsBody::Plain(text) => !text.trim().is_empty(),
    });
    (header.is_some() || body.is_some()).then_some((header, body))
}

/// How wide rows are drawn, told cheaply enough to ask of every row in a
/// long list: each character is laid out once, in the rows' own fonts, and
/// a row is the sum of its characters. A count of characters is not a
/// width: an ideograph is about two letters wide, and `i` less than one.
/// Kerning is left out, so this only ranks the rows; those it ranks widest
/// are then laid out whole. The kind column is as wide in every row, and
/// is left out.
struct RowWidths<'a> {
    text_system: &'a WindowTextSystem,
    font_size: Pixels,
    gap: Pixels,
    label_font: FontId,
    detail_font: FontId,
    chars: HashMap<(FontId, char), Pixels>,
}

impl<'a> RowWidths<'a> {
    /// The rows' fonts are the ones inherited where the menu renders, at the
    /// rows' own size; the detail after a label is italic.
    fn new(window: &'a Window) -> Self {
        let font = window.text_style().font();
        let text_system = window.text_system();
        Self {
            label_font: text_system.resolve_font(&font),
            detail_font: text_system.resolve_font(&font.italic()),
            font_size: ROW_TEXT_SIZE.to_pixels(window.rem_size()),
            gap: ROW_GAP.to_pixels(window.rem_size()),
            text_system,
            chars: HashMap::default(),
        }
    }

    fn text(&mut self, font_id: FontId, text: &str) -> Pixels {
        let Self {
            text_system,
            font_size,
            chars,
            ..
        } = self;
        text.chars().fold(px(0.), |width, c| {
            width
                + *chars
                    .entry((font_id, c))
                    .or_insert_with(|| text_system.layout_width(font_id, *font_size, c))
        })
    }

    fn row(&mut self, item: &CompletionItem) -> Pixels {
        let label = self.text(self.label_font, &item.label);
        match &item.label_details {
            Some(details) => {
                let signature = details
                    .detail
                    .as_deref()
                    .map_or(px(0.), |s| self.text(self.label_font, s));
                let right =
                    right_column(item).map_or(px(0.), |r| self.gap + self.text(self.label_font, r));
                label + signature + right
            }
            None => match item.detail.as_deref() {
                Some(detail) => label + self.gap + self.text(self.detail_font, detail),
                None => label,
            },
        }
    }
}

impl ContextMenuDelegate {
    fn set_items(&mut self, items: Vec<CompletionItem>, matches: Vec<Option<Rc<[Range<usize>]>>>) {
        self.items = items.into_iter().map(Rc::new).collect();
        self.matches = matches;
        self.selected_ix = 0;
    }

    /// The same list with some items replaced: the highlighted row stays.
    fn replace_items(
        &mut self,
        items: Vec<CompletionItem>,
        matches: Vec<Option<Rc<[Range<usize>]>>>,
    ) {
        let selected = self.selected_ix;
        self.set_items(items, matches);
        self.selected_ix = selected.min(self.items.len().saturating_sub(1));
    }

    fn selected_item(&self) -> Option<&Rc<CompletionItem>> {
        self.items.get(self.selected_ix)
    }

    fn row(&self, ix: usize) -> Option<CompletionMenuItem> {
        let item = self.items.get(ix)?;
        Some(
            CompletionMenuItem::new(ix, item.clone())
                .highlight_prefix(self.query.clone())
                .matches(self.matches.get(ix).cloned().flatten())
                .show_kinds(self.show_kinds)
                .gutter(self.gutter),
        )
    }
}

#[derive(IntoElement)]
struct CompletionMenuItem {
    ix: usize,
    item: Rc<CompletionItem>,
    children: Vec<AnyElement>,
    selected: bool,
    highlight_prefix: SharedString,
    matches: Option<Rc<[Range<usize>]>>,
    show_kinds: bool,
    gutter: Pixels,
}

impl CompletionMenuItem {
    fn new(ix: usize, item: Rc<CompletionItem>) -> Self {
        Self {
            ix,
            item,
            children: vec![],
            selected: false,
            highlight_prefix: "".into(),
            matches: None,
            show_kinds: false,
            gutter: px(0.),
        }
    }

    fn gutter(mut self, gutter: Pixels) -> Self {
        self.gutter = gutter;
        self
    }

    fn highlight_prefix(mut self, s: impl Into<SharedString>) -> Self {
        self.highlight_prefix = s.into();
        self
    }

    fn matches(mut self, matches: Option<Rc<[Range<usize>]>>) -> Self {
        self.matches = matches;
        self
    }

    fn show_kinds(mut self, show: bool) -> Self {
        self.show_kinds = show;
        self
    }
}

impl Selectable for CompletionMenuItem {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl ParentElement for CompletionMenuItem {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

/// The kind column of a row: its letter on a square of its colour, or the
/// square's room left blank for an item of no kind the menu knows.
fn kind_badge(kind: Option<CompletionItemKind>, cx: &App) -> impl IntoElement {
    let letter = kind.and_then(kind_letter);
    div()
        .flex_none()
        .size(KIND_SIZE)
        .mr(ROW_GAP)
        .flex()
        .items_center()
        .justify_center()
        .rounded(cx.theme().radius.half())
        .text_size(KIND_TEXT_SIZE)
        .font_weight(FontWeight::BOLD)
        .when_some(letter, |this, (letter, color)| {
            let color = kind_color(color, cx);
            this.bg(color.opacity(0.16)).text_color(color).child(letter)
        })
}

impl RenderOnce for CompletionMenuItem {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let item = self.item;

        let deprecated = item.deprecated.unwrap_or(false);
        let prefix = item
            .filter_text
            .as_ref()
            .map_or(self.highlight_prefix.len(), |s| s.len());
        let matched = HighlightStyle {
            color: Some(cx.theme().blue),
            ..Default::default()
        };
        let highlights = label_highlights(&item.label, self.matches.as_deref(), prefix)
            .into_iter()
            .map(|range| (range, matched))
            .collect::<Vec<_>>();
        // The name is whole: it gives way to nothing but the row's own end,
        // when it alone is wider than the row. A deprecated item's name is
        // struck through, and nothing else of its row, as IntelliJ and VS
        // Code do.
        let label = div()
            .flex_none()
            .whitespace_nowrap()
            .when(deprecated, |this| this.line_through())
            .debug_selector(|| "completion-label".to_string())
            .child(StyledText::new(item.label.clone()).with_highlights(highlights));
        let muted = cx.theme().muted_foreground;

        h_flex()
            .id(self.ix)
            .p_1()
            .mr(self.gutter)
            .overflow_hidden()
            .text_size(ROW_TEXT_SIZE)
            .line_height(relative(ROW_LINE_HEIGHT))
            .rounded(cx.theme().radius.half())
            .hover(|this| this.bg(cx.theme().accent.opacity(0.8)))
            .when(self.selected, |this| {
                this.bg(cx.theme().tokens.accent)
                    .text_color(cx.theme().accent_foreground)
            })
            .when(self.show_kinds, |this| {
                this.child(kind_badge(item.kind, cx))
            })
            .child(label)
            .map(|this| match &item.label_details {
                // The signature straight after the name and the type at the
                // right, as IntelliJ and VS Code lay them out. In a row too
                // wide for the menu the signature is cut first, and the type
                // only once the signature is gone and the type alone is
                // wider than what the name leaves: the type's box is at
                // most as wide as that and never shrinks, and the signature
                // takes what is left of it.
                Some(details) => {
                    let signature = details.detail.as_deref().filter(|s| !s.is_empty());
                    let right = right_column(&item);
                    this.child(
                        h_flex()
                            .flex_grow(1.)
                            .min_w(px(0.))
                            .overflow_hidden()
                            .when_some(signature, |this, signature| {
                                this.child(
                                    div()
                                        .min_w(px(0.))
                                        .truncate()
                                        .text_color(muted)
                                        .debug_selector(|| "completion-signature".to_string())
                                        .child(signature.to_string()),
                                )
                            })
                            .when_some(right, |this, right| {
                                this.child(
                                    h_flex()
                                        .flex_grow(1.)
                                        .flex_shrink_0()
                                        .max_w_full()
                                        .pl(ROW_GAP)
                                        .justify_end()
                                        .child(
                                            div()
                                                .min_w(px(0.))
                                                .truncate()
                                                .text_color(muted)
                                                .debug_selector(|| "completion-type".to_string())
                                                .child(right.to_string()),
                                        ),
                                )
                            }),
                    )
                }
                None => this.when(item.detail.is_some(), |this| {
                    this.child(
                        Label::new(item.detail.as_deref().unwrap_or("").to_string())
                            .ml(ROW_GAP)
                            .text_color(muted)
                            .italic(),
                    )
                }),
            })
            .children(self.children)
    }
}

impl EventEmitter<DismissEvent> for ContextMenuDelegate {}

impl ListDelegate for ContextMenuDelegate {
    type Item = CompletionMenuItem;

    fn items_count(&self, _: usize, _: &gpui::App) -> usize {
        self.items.len()
    }

    fn render_item(
        &mut self,
        ix: crate::IndexPath,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        self.row(ix.row)
    }

    fn set_selected_index(
        &mut self,
        ix: Option<crate::IndexPath>,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        self.selected_ix = ix.map(|i| i.row).unwrap_or(0);
        cx.notify();
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let Some(item) = self.selected_item() else {
            return;
        };

        self.menu.update(cx, |this, cx| {
            this.select_item(&item, window, cx);
        });
    }
}

/// Where the list and its documentation panel were drawn last, in window
/// coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Placed {
    pub(crate) list: Bounds<Pixels>,
    pub(crate) docs: Option<Bounds<Pixels>>,
    /// The list is above the caret's line, there being no room below it.
    pub(crate) above: bool,
}

/// A context menu for code completions and code actions.
pub struct CompletionMenu {
    offset: usize,
    editor: WeakEntity<EditorState>,
    list: Entity<ListState<ContextMenuDelegate>>,
    open: bool,

    /// The offset of the first character that triggered the completion.
    pub(crate) trigger_start_offset: Option<usize>,
    query: SharedString,
    /// How wide the list is drawn: its widest row, measured on the first
    /// render after the items change, and the list's height it was
    /// measured for, which says whether the rows scroll. `None` until then.
    width: Option<(Pixels, Pixels)>,
    /// Bumped as the row highlighted becomes another item: a new list's
    /// first, or one the keyboard moves to. It keys the documentation
    /// panel's scroll, so a new row's documentation is shown from its top.
    highlight: u64,
    /// The keyboard moved the highlight since the list was shown: a list
    /// handed again for the same caret keeps it on the same item.
    moved: bool,
    /// The row the provider was last told is highlighted, and which item
    /// it was (`CompletionProvider::completion_selected`).
    announced: Option<(usize, *const CompletionItem)>,
    /// Where the documentation panel is scrolled to, and the highlight it
    /// is for. The panel is built as it is placed, every frame, so its
    /// scroll is kept here.
    docs_scroll: (ScrollHandle, u64),
    placed: Rc<Cell<Option<Placed>>>,
    _subscriptions: Vec<Subscription>,
}

impl CompletionMenu {
    /// Creates a new `CompletionMenu` with the given offset and completion items.
    ///
    /// NOTE: This element should not call from EditorState::new, unless that will stack overflow.
    pub(crate) fn new(
        editor: Entity<EditorState>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let view = cx.entity();
            let menu = ContextMenuDelegate {
                query: SharedString::default(),
                menu: view,
                items: vec![],
                matches: vec![],
                show_kinds: false,
                gutter: px(0.),
                selected_ix: 0,
            };

            let list = cx.new(|cx| ListState::new(menu, window, cx));

            let _subscriptions =
                vec![
                    cx.subscribe(&list, |this: &mut Self, _, ev: &ListEvent, cx| {
                        match ev {
                            ListEvent::Confirm(_) => {
                                this.hide(cx);
                            }
                            _ => {}
                        }
                        cx.notify();
                    }),
                ];

            Self {
                offset: 0,
                editor: editor.downgrade(),
                list,
                open: false,
                trigger_start_offset: None,
                query: SharedString::default(),
                width: None,
                highlight: 0,
                moved: false,
                announced: None,
                docs_scroll: (ScrollHandle::new(), 0),
                placed: Rc::default(),
                _subscriptions,
            }
        })
    }

    fn select_item(&mut self, item: &CompletionItem, window: &mut Window, cx: &mut Context<Self>) {
        let item = item.clone();
        let range = self.trigger_start_offset.unwrap_or(self.offset)..self.offset;

        let editor = self.editor.clone();

        // The provider is offered the item first, with nothing borrowed, and
        // the editor inserts it only if the provider does not.
        cx.spawn_in(window, async move |_, cx| {
            let provider =
                editor.read_with(cx, |editor, _| editor.lsp().completion_provider.clone())?;
            if let Some(provider) = provider
                && cx.update(|window, cx| provider.accept_completion(&item, window, cx))?
            {
                return Ok(());
            }
            editor.update_in(cx, |editor, window, cx| {
                editor.insert_completion(&item, range, window, cx);
            })
        })
        .detach();

        self.hide(cx);
    }

    pub(crate) fn handle_action(
        &mut self,
        action: Box<dyn Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.open {
            return false;
        }

        cx.propagate();
        if input::Enter::is_primary(&*action) {
            self.on_action_enter(window, cx);
        } else if action.partial_eq(&input::Escape) {
            self.on_action_escape(window, cx);
        } else if action.partial_eq(&input::MoveUp) {
            self.on_action_up(window, cx);
        } else if action.partial_eq(&input::MoveDown) {
            self.on_action_down(window, cx);
        } else {
            return false;
        }

        true
    }

    fn on_action_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.list.read(cx).delegate().selected_item().cloned() else {
            return;
        };
        self.select_item(&item, window, cx);
    }

    fn on_action_escape(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.hide(cx);
    }

    fn on_action_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.move_highlight(window, cx, |list, window, cx| {
            list.on_action_select_prev(&actions::SelectUp, window, cx)
        });
    }

    fn on_action_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.move_highlight(window, cx, |list, window, cx| {
            list.on_action_select_next(&actions::SelectDown, window, cx)
        });
    }

    fn move_highlight(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        by: impl FnOnce(
            &mut ListState<ContextMenuDelegate>,
            &mut Window,
            &mut Context<ListState<ContextMenuDelegate>>,
        ),
    ) {
        let before = self.list.read(cx).delegate().selected_ix;
        self.list.update(cx, |list, cx| by(list, window, cx));
        self.moved = true;
        if self.list.read(cx).delegate().selected_ix != before {
            self.highlight += 1;
        }
        self.announce_selection(window, cx);
    }

    /// Tell the provider which row is highlighted, if that is news: once
    /// nothing is borrowed, and only if the menu is still open on it then,
    /// so that of the rows the keyboard passes over in one go, only the one
    /// it comes to is told of.
    fn announce_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (ix, item) = {
            let delegate = self.list.read(cx).delegate();
            match delegate.selected_item() {
                Some(item) => (delegate.selected_ix, item.clone()),
                None => return,
            }
        };
        let announced = Some((ix, Rc::as_ptr(&item)));
        if self.announced == announced {
            return;
        }
        self.announced = announced;
        let editor = self.editor.clone();
        cx.spawn_in(window, async move |menu, cx| {
            let still = menu.read_with(cx, |menu, cx| {
                let delegate = menu.list.read(cx).delegate();
                menu.open
                    && delegate.selected_ix == ix
                    && delegate
                        .items
                        .get(ix)
                        .is_some_and(|now| Rc::ptr_eq(now, &item) || same_row(now, &item))
            })?;
            let provider =
                editor.read_with(cx, |editor, _| editor.lsp().completion_provider.clone())?;
            if let (true, Some(provider)) = (still, provider) {
                cx.update(|window, cx| provider.completion_selected(ix, &item, window, cx))?;
            }
            anyhow::Ok(())
        })
        .detach();
    }

    /// The user closed the menu: Escape, a row taken, a click away, or its
    /// caret scrolled out of sight. The editor is told once nothing is
    /// borrowed, whatever list it holds by then: a list presented since the
    /// last frame is the one the user closed.
    pub(crate) fn hide(&mut self, cx: &mut Context<Self>) {
        self.close(cx);
        let editor = self.editor.clone();
        cx.spawn(async move |_, cx| {
            let _ = editor.update(cx, |editor, cx| editor.dismiss_completion_overlay(cx));
        })
        .detach();
    }

    /// The editor's completion state closed, and the menu follows it. It
    /// tells the editor nothing: a close it told it of a moment later closed
    /// a list presented in that moment, and a host had to present it again.
    pub(crate) fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.trigger_start_offset = None;
        self.announced = None;
        cx.notify();
    }

    /// Sets the trigger start offset if it is not already set.
    pub(crate) fn update_query(&mut self, start_offset: usize, query: impl Into<SharedString>) {
        if self.trigger_start_offset.is_none() {
            self.trigger_start_offset = Some(start_offset);
        }
        self.query = query.into();
    }

    /// Where each item's label matches what was typed, as the editor's
    /// completion provider says.
    fn label_matches(&self, items: &[CompletionItem], cx: &App) -> Vec<Option<Rc<[Range<usize>]>>> {
        let provider = self
            .editor
            .upgrade()
            .and_then(|editor| editor.read(cx).lsp().completion_provider.clone());
        items
            .iter()
            .map(|item| {
                provider
                    .as_ref()
                    .and_then(|provider| provider.completion_label_matches(item))
                    .map(Rc::from)
            })
            .collect()
    }

    fn show_kinds(&self, cx: &App) -> bool {
        self.editor
            .upgrade()
            .is_some_and(|editor| editor.read(cx).lsp().completion_menu.show_kinds)
    }

    pub(crate) fn show(
        &mut self,
        offset: usize,
        items: impl Into<Vec<CompletionItem>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let items = items.into();
        // A list for the caret where it was, handed after the keyboard moved
        // the highlight, a late answer or one asked for again with no key
        // typed since, keeps the highlight on the same item where the new
        // list has it: an Enter pressed as the list lands takes the row the
        // user moved to, not the new list's first.
        let kept = (self.open && self.moved && offset == self.offset)
            .then(|| self.list.read(cx).delegate().selected_item().cloned())
            .flatten()
            .and_then(|was| items.iter().position(|item| same_row(item, &was)));
        let matches = self.label_matches(&items, cx);
        let show_kinds = self.show_kinds(cx);
        self.offset = offset;
        self.open = true;
        self.width = None;
        if kept.is_none() {
            self.moved = false;
            self.highlight += 1;
        }
        let row = IndexPath::new(kept.unwrap_or(0));
        self.list.update(cx, |this, cx| {
            let longest_ix = items
                .iter()
                .enumerate()
                .max_by_key(|(_, item)| row_chars(item))
                .map(|(ix, _)| ix)
                .unwrap_or(0);

            this.delegate_mut().query = self.query.clone();
            this.delegate_mut().show_kinds = show_kinds;
            this.delegate_mut().set_items(items, matches);
            this.set_selected_index(Some(row), window, cx);
            // The row selected is shown, the first of a new list: a list
            // that had been scrolled would otherwise keep its offset into
            // the new one.
            this.scroll_to_item(row, ScrollStrategy::Top, window, cx);
            this.set_item_to_measure_index(IndexPath::new(longest_ix), window, cx);
        });
        self.announce_selection(window, cx);

        cx.notify();
    }

    /// The list shown, with some of its items replaced in place: resolved
    /// for their documentation. The highlighted row and the scroll stay.
    pub(crate) fn update_items(
        &mut self,
        items: Vec<CompletionItem>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.open {
            return;
        }
        let matches = self.label_matches(&items, cx);
        // a detail resolved may be wider than the one it replaces
        self.width = None;
        self.list.update(cx, |this, cx| {
            this.delegate_mut().replace_items(items, matches);
            cx.notify();
        });
        // The row told of is the same row, its item handed back: not news.
        // Told again, a provider that resolves each row it is told of would
        // resolve it, hand it back and be told of it without end.
        if let Some((ix, _)) = self.announced {
            let delegate = self.list.read(cx).delegate();
            let same = (delegate.selected_ix == ix)
                .then(|| delegate.items.get(ix).map(Rc::as_ptr))
                .flatten();
            if let Some(item) = same {
                self.announced = Some((ix, item));
            }
        }
        self.announce_selection(window, cx);
        cx.notify();
    }

    /// How wide the list must be for its widest row to show whole: the rows
    /// laid out unwrapped, as they are painted, plus the scrollbar's track
    /// when the rows overflow the menu, because the track is drawn over
    /// their ends.
    ///
    /// The list cannot tell this itself. It measures one row at the width
    /// it was drawn at last, so a menu that stays open could narrow but
    /// never widen again, and a menu stacked above its documentation panel
    /// took the whole `max_width` whatever its rows.
    ///
    /// In a list longer than [`MEASURED_ROWS`], only the rows [`RowWidths`]
    /// ranks widest are laid out. Layout asserts it runs inside a draw, so
    /// this is called from `render`.
    fn measure_width(
        &self,
        list_max: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> (Pixels, Pixels) {
        let (row_count, candidates) = {
            let delegate = self.list.read(cx).delegate();
            let items = &delegate.items;
            let candidates: Vec<CompletionMenuItem> = if items.len() <= MEASURED_ROWS {
                (0..items.len()).filter_map(|ix| delegate.row(ix)).collect()
            } else {
                let mut widths = RowWidths::new(window);
                let mut ranked: Vec<(Reverse<Pixels>, usize)> = items
                    .iter()
                    .enumerate()
                    .map(|(ix, item)| (Reverse(widths.row(item)), ix))
                    .collect();
                ranked.select_nth_unstable(MEASURED_ROWS - 1);
                ranked.truncate(MEASURED_ROWS);
                ranked
                    .into_iter()
                    .filter_map(|(_, ix)| delegate.row(ix))
                    .collect()
            };
            (items.len(), candidates)
        };

        let available_space = size(AvailableSpace::MaxContent, AvailableSpace::MinContent);
        let (mut width, mut row_height) = (px(0.), px(0.));
        for row in candidates {
            let row =
                row.gutter(px(0.))
                    .into_any_element()
                    .layout_as_root(available_space, window, cx);
            width = width.max(row.width);
            row_height = row_height.max(row.height);
        }

        let mut gutter = px(0.);
        if row_height * row_count as f32 > list_max {
            gutter = cx
                .try_global::<gpui_base::Theme>()
                .map(|theme| theme.scrollbar.styles().track_width())
                .unwrap_or_else(|| gpui_base::ScrollbarStyles::default().track_width());
        }
        (width + gutter, gutter)
    }

    /// Where the list and its documentation were drawn last.
    #[cfg(test)]
    pub(crate) fn placed(&self) -> Option<Placed> {
        self.placed.get()
    }
}

impl Render for CompletionMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return Empty.into_any_element();
        }

        if self.list.read(cx).delegate().items.is_empty() {
            self.open = false;
            return Empty.into_any_element();
        }

        let Some(editor) = self.editor.upgrade() else {
            return Empty.into_any_element();
        };
        // How tall the list may be, from where the caret's line was drawn
        // last; the placement checks it against the line as drawn in this
        // frame, and draws the menu again when that has moved.
        let padding = MENU_PADDING.to_pixels(window.rem_size());
        let window_area = Bounds::new(point(px(0.), px(0.)), window.viewport_size());
        let list_max = editor
            .read(cx)
            .painted_caret()
            .map_or(MAX_MENU_HEIGHT, |caret| {
                list_height(
                    caret.line(),
                    caret.input_bounds.intersect(&window_area),
                    padding,
                )
            });
        let width = match self.width {
            Some((width, measured_for)) if measured_for == list_max => width,
            _ => {
                let (width, gutter) = self.measure_width(list_max, window, cx);
                self.width = Some((width, list_max));
                self.list
                    .update(cx, |list, _| list.delegate_mut().gutter = gutter);
                width
            }
        };
        let configured_max = editor.read(cx).lsp().completion_menu.max_width;
        let window_width = window.viewport_size().width;
        // The popover is the list and its padding, between its floor and its
        // ceiling, and never wider than the window.
        let popover_width = (width + padding * 2.)
            .min(configured_max)
            .max(MIN_MENU_WIDTH)
            .min(window_width);

        if self.docs_scroll.1 != self.highlight {
            self.docs_scroll = (ScrollHandle::new(), self.highlight);
        }
        let docs = {
            let delegate = self.list.read(cx).delegate();
            delegate
                .selected_item()
                .and_then(|item| docs_of(item))
                .map(|(header, body)| DocsSpec {
                    header,
                    body,
                    scroll: self.docs_scroll.0.clone(),
                })
        };

        // The list asks for its widest row and fills the popover: the rows
        // fill it at the floor, and a row wider than the ceiling is cut.
        let list = editor_popover("completion-menu", cx)
            .p(MENU_PADDING)
            .w(popover_width)
            .child(
                List::new(&self.list)
                    .w(width)
                    .min_w_full()
                    .max_w_full()
                    .max_h(list_max),
            )
            .into_any_element();

        deferred(Placement {
            editor,
            menu: cx.weak_entity(),
            list,
            list_max,
            docs,
            docs_width: configured_max,
            docs_element: None,
            placed: self.placed.clone(),
        })
        .into_any_element()
    }
}

/// What the documentation panel is to show, and where it is scrolled to.
struct DocsSpec {
    header: Option<SharedString>,
    body: Option<DocsBody>,
    scroll: ScrollHandle,
}

impl DocsSpec {
    /// The panel, `width` wide and at most `max_height` tall: longer
    /// documentation scrolls in it. The panel is built every frame, as it is
    /// placed, and scrolls by a handle the menu keeps.
    fn element(
        &self,
        width: Pixels,
        max_height: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let body = self.body.as_ref().map(|body| match body {
            DocsBody::Markdown(text) => render_markdown("completion-doc", text.clone(), window, cx)
                .selectable(false)
                .into_any_element(),
            DocsBody::Plain(text) => div().py_1().child(text.clone()).into_any_element(),
        });
        let has_body = body.is_some();
        let header = self.header.clone().map(|header| {
            div()
                .py_1()
                .when(has_body, |this| {
                    this.border_b_1().border_color(cx.theme().border)
                })
                .child(header)
        });
        // prose in the interface's font, as the header's code is in the
        // editor's
        let body = body.map(|body| {
            div()
                .font_family(cx.theme().font_family.clone())
                .child(body)
        });
        // the popover's own padding, `editor_popover`'s, around what scrolls
        let padding = MENU_PADDING.to_pixels(window.rem_size());
        editor_popover("completion-docs", cx)
            .relative()
            .w(width)
            // A click in the panel, on its text, a link or its scrollbar,
            // leaves the keyboard with the editor: the text took it, and the
            // keys typed after went nowhere while the list stayed open.
            .capture_any_mouse_down(|_, window, _| window.prevent_default())
            .child(
                div()
                    .id("completion-docs-scroll")
                    .max_h((max_height - padding * 2.).max(px(0.)))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .px_2()
                    .child(
                        div()
                            .debug_selector(|| "completion-docs-content".to_string())
                            .children(header)
                            .children(body),
                    ),
            )
            .vertical_scrollbar(&self.scroll)
            .into_any_element()
    }
}

/// The list and its documentation panel, placed as they are prepainted:
/// after the editor has laid out its text for the frame, so from where the
/// caret is drawn in it ([`EditorState::painted_caret`]), not a frame
/// behind. Below the caret's line when the list fits there inside the
/// editor, else above it when it fits there; a list that fits on neither
/// side is as tall as the side with more room lets it be ([`list_height`]),
/// else it goes by the window ([`goes_below`]). The documentation goes
/// beside the list, right or left, else past it, away from the caret, else
/// on the caret's other side ([`docs_side`]), inside the editor. A caret
/// out of the editor's sight closes the menu.
struct Placement {
    editor: Entity<EditorState>,
    menu: WeakEntity<CompletionMenu>,
    list: AnyElement,
    /// How tall the list was let be, as the menu rendered it.
    list_max: Pixels,
    docs: Option<DocsSpec>,
    /// The documentation panel's width, when there is room for it.
    docs_width: Pixels,
    /// The documentation panel, once it is laid out.
    docs_element: Option<AnyElement>,
    placed: Rc<Cell<Option<Placed>>>,
}

impl IntoElement for Placement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// Where the documentation panel goes, relative to the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DocsSide {
    Right,
    Left,
    /// Above or below the list: past it, away from the caret, or on the
    /// caret's other side when there is too little room past it.
    Beyond,
}

/// The documentation panel's side and width, beside a list at `list` in a
/// window `window_width` wide, the panel at most `full` wide: the right when
/// [`PREFERRED_DOCS_WIDTH`] of it fits there, else the left when all of it
/// does, else the wider side when [`MIN_DOCS_WIDTH`] fits, else past the
/// list.
fn docs_side(list: Bounds<Pixels>, window_width: Pixels, full: Pixels) -> (DocsSide, Pixels) {
    let full = full.min(window_width - WINDOW_MARGIN * 2.);
    let right = window_width - WINDOW_MARGIN - (list.right() + POPOVER_GAP);
    let left = list.left() - POPOVER_GAP - WINDOW_MARGIN;
    if right >= full.min(PREFERRED_DOCS_WIDTH) {
        (DocsSide::Right, right.min(full))
    } else if left >= full {
        (DocsSide::Left, full)
    } else if right.max(left) >= MIN_DOCS_WIDTH {
        if right >= left {
            (DocsSide::Right, right)
        } else {
            (DocsSide::Left, left)
        }
    } else {
        (DocsSide::Beyond, full)
    }
}

/// How tall the list may be beside the caret's `line` in the editor's
/// `area`: [`MAX_MENU_HEIGHT`], or, when it fits on neither side of the
/// line there, what the side with more room holds, less the popover's
/// `padding`. It then scrolls, rather than run past the editor over what is
/// under it, a status bar, where a click took its last row. With less than
/// [`MIN_LIST_HEIGHT`] of room either side it is not shrunk, and goes where
/// the window has room.
fn list_height(line: Bounds<Pixels>, area: Bounds<Pixels>, padding: Pixels) -> Pixels {
    let below = area.bottom() - (line.bottom() + POPOVER_GAP);
    let above = line.top() - POPOVER_GAP - area.top();
    let room = below.max(above) - padding * 2.;
    if room >= MIN_LIST_HEIGHT {
        room.min(MAX_MENU_HEIGHT)
    } else {
        MAX_MENU_HEIGHT
    }
}

/// Whether the caret's `line` is in sight in the editor's `area`, any of
/// it. Across, the caret is kept in sight as it moves, and an editor
/// narrower than its gutter draws it past its text's bounds.
fn in_sight(line: Bounds<Pixels>, area: Bounds<Pixels>) -> bool {
    line.bottom() > area.top() && line.top() < area.bottom()
}

/// Whether a list `height` tall goes below the caret's `line`: below when
/// it fits there inside `editor`, above when it fits there instead; failing
/// both, the same in the `window`, and else on the side with more room.
fn goes_below(
    line: Bounds<Pixels>,
    editor: Bounds<Pixels>,
    window: Bounds<Pixels>,
    height: Pixels,
) -> bool {
    let room = |area: Bounds<Pixels>| {
        (
            area.bottom() - (line.bottom() + POPOVER_GAP),
            line.top() - POPOVER_GAP - area.top(),
        )
    };
    let (below, above) = room(editor);
    if height <= below || height <= above {
        return height <= below;
    }
    let (below, above) = room(window);
    height <= below || (height > above && below >= above)
}

impl Element for Placement {
    type RequestLayoutState = ();
    type PrepaintState = Option<Placed>;

    fn id(&self) -> Option<ElementId> {
        Some("completion-placement".into())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (window.request_layout(Style::default(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let viewport = window.viewport_size();
        let window_area = Bounds::new(point(px(0.), px(0.)), viewport);
        // as much of the editor as is in the window: a list inside it is
        // over nothing of the screen around it, a status bar under it
        let caret = self
            .editor
            .read(cx)
            .painted_caret()
            .map(|caret| (caret, caret.input_bounds.intersect(&window_area)))
            .filter(|(caret, area)| in_sight(caret.line(), *area));
        let Some((caret, area)) = caret else {
            // The caret scrolled out of the editor's sight: the list is
            // shown nowhere, and an Enter would take a row nobody sees. The
            // menu closes, as for a click away.
            self.placed.set(None);
            let menu = self.menu.clone();
            cx.defer(move |cx| {
                let _ = menu.update(cx, |menu, cx| menu.hide(cx));
            });
            return None;
        };
        let line = caret.line();
        let padding = MENU_PADDING.to_pixels(window.rem_size());
        // the room the list was given, from the line as drawn a frame
        // before: the menu draws again with the room it has now
        if list_height(line, area, padding) != self.list_max {
            let _ = self.menu.update(cx, |_, cx| cx.notify());
        }

        // each popover is as big as its content, its width set
        let available = AvailableSpace::min_size();
        let list_size = self.list.layout_as_root(available, window, cx);
        // under the caret, moved left until it ends inside the window's
        // margin, but never past its left edge
        let x = (caret.bounds.left() - px(4.))
            .min(viewport.width - WINDOW_MARGIN - list_size.width)
            .max(px(0.));
        let below = goes_below(line, area, window_area, list_size.height);
        let y = if below {
            line.bottom() + POPOVER_GAP
        } else {
            line.top() - POPOVER_GAP - list_size.height
        };
        let list = Bounds::new(point(x, y), list_size);
        self.list.prepaint_at(list.origin, window, cx);

        // The documentation is kept inside the editor too, top to bottom,
        // unless the editor is too short for it.
        let docs_area = if area.size.height >= MIN_DOCS_HEIGHT {
            area
        } else {
            window_area
        };
        let (top, bottom) = (
            docs_area.top() + WINDOW_MARGIN,
            docs_area.bottom() - WINDOW_MARGIN,
        );
        let docs = self.docs.as_ref().map(|spec| {
            let (side, width) = docs_side(list, viewport.width, self.docs_width);
            // where the panel starts, whether it grows down from there or
            // up, and the room it has
            let (x, from, down, room) = match side {
                DocsSide::Right | DocsSide::Left => {
                    let x = if side == DocsSide::Right {
                        list.right() + POPOVER_GAP
                    } else {
                        list.left() - POPOVER_GAP - width
                    };
                    // level with the list's top below the caret, with its
                    // bottom above it, moved up or down as far as it must
                    // to stay in the editor: beside the list it is over
                    // neither the caret nor its list
                    if below {
                        (x, list.top(), true, bottom - top)
                    } else {
                        (x, list.bottom(), false, bottom - top)
                    }
                }
                DocsSide::Beyond => {
                    let x = list
                        .left()
                        .min(viewport.width - WINDOW_MARGIN - width)
                        .max(WINDOW_MARGIN);
                    let (beyond, opposite) = if below {
                        let beyond = list.bottom() + POPOVER_GAP;
                        let opposite = line.top() - POPOVER_GAP;
                        (
                            (beyond, true, bottom - beyond),
                            (opposite, false, opposite - top),
                        )
                    } else {
                        let beyond = list.top() - POPOVER_GAP;
                        let opposite = line.bottom() + POPOVER_GAP;
                        (
                            (beyond, false, beyond - top),
                            (opposite, true, bottom - opposite),
                        )
                    };
                    let (from, down, room) =
                        if beyond.2 >= MIN_DOCS_HEIGHT || beyond.2 >= opposite.2 {
                            beyond
                        } else {
                            opposite
                        };
                    (x, from, down, room)
                }
            };
            let mut element =
                spec.element(width, MAX_DOCS_HEIGHT.min(room.max(px(0.))), window, cx);
            let docs_size = element.layout_as_root(available, window, cx);
            let mut y = if down { from } else { from - docs_size.height };
            if side != DocsSide::Beyond {
                y = y.min(bottom - docs_size.height).max(top);
            }
            let bounds = Bounds::new(point(x, y), docs_size);
            element.prepaint_at(bounds.origin, window, cx);
            (element, bounds)
        });

        let placed = Placed {
            list,
            docs: docs.as_ref().map(|(_, bounds)| *bounds),
            above: !below,
        };
        self.placed.set(Some(placed));
        self.docs_element = docs.map(|(element, _)| element);
        Some(placed)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        placed: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(placed) = *placed else {
            return;
        };
        self.list.paint(window, cx);
        if let Some(docs) = self.docs_element.as_mut() {
            docs.paint(window, cx);
        }
        // a mouse button pressed anywhere but on the list or its
        // documentation closes the menu
        let menu = self.menu.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
            if phase != DispatchPhase::Capture
                || placed.list.contains(&event.position)
                || placed
                    .docs
                    .is_some_and(|docs| docs.contains(&event.position))
            {
                return;
            }
            let _ = menu.update(cx, |menu, cx| menu.hide(cx));
        });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Editor;
    use gpui::{Bounds, TestAppContext, VisualTestContext};
    use lsp_types::{CompletionItemLabelDetails, MarkupContent};
    use std::cell::RefCell;

    /// An editor with a completion menu of its own beside it, so the tests
    /// can hand the menu any list and read back how it was drawn.
    struct MenuProbe {
        state: Entity<EditorState>,
        menu: Entity<CompletionMenu>,
        /// Room left of the editor, to put the cursor near the window's edge.
        indent: Pixels,
        /// Room above the editor, to put the cursor near the window's bottom.
        top: Pixels,
        /// How tall the editor is; what is under it, to the window's
        /// bottom, stands for a status bar.
        height: Pixels,
    }

    impl Render for MenuProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            // the menu is placed from the input's origin, as the input's
            // own overlays are
            div().size_full().pl(self.indent).pt(self.top).child(
                div()
                    .relative()
                    .child(Editor::new(&self.state).h(self.height))
                    .child(self.menu.clone()),
            )
        }
    }

    fn probe(cx: &mut TestAppContext) -> (Entity<MenuProbe>, &mut VisualTestContext) {
        cx.update(crate::init);
        let (probe, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|cx| EditorState::new(window, cx).language("sql"));
            let menu = CompletionMenu::new(state.clone(), window, cx);
            MenuProbe {
                state,
                menu,
                indent: px(0.),
                top: px(0.),
                height: px(200.),
            }
        });
        // the menu sits under the cursor, which is known once the editor
        // has been laid out
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (probe, cx)
    }

    fn item(label: &str, detail: Option<&str>) -> CompletionItem {
        CompletionItem {
            label: label.into(),
            detail: detail.map(Into::into),
            ..Default::default()
        }
    }

    /// Show `items`, draw, and return what the menu measured and the bounds
    /// the list was drawn in.
    fn show(
        probe: &Entity<MenuProbe>,
        items: Vec<CompletionItem>,
        cx: &mut VisualTestContext,
    ) -> (Pixels, Bounds<Pixels>) {
        let menu = probe.read_with(cx, |probe, _| probe.menu.clone());
        cx.update(|window, cx| {
            menu.update(cx, |menu, cx| menu.show(0, items, window, cx));
            window.draw(cx).clear(cx);
        });
        cx.read(|cx| {
            let menu = menu.read(cx);
            let bounds = menu.list.read(cx).scroll_handle().bounds();
            (menu.width.expect("the menu measured its rows").0, bounds)
        })
    }

    fn placed(probe: &Entity<MenuProbe>, cx: &mut VisualTestContext) -> Placed {
        cx.read(|cx| probe.read(cx).menu.read(cx).placed())
            .expect("the menu was placed")
    }

    /// The menu the input draws itself, kept in step with its state as it
    /// renders: the one a host drives by presenting lists.
    fn own_menu(state: &Entity<EditorState>, cx: &mut VisualTestContext) -> Entity<CompletionMenu> {
        cx.read(|cx| crate::input::overlay::completion_menu_of(state, cx))
            .expect("the input's menu, while it is open")
    }

    fn redraw(
        probe: &Entity<MenuProbe>,
        cx: &mut VisualTestContext,
        f: impl FnOnce(&mut MenuProbe),
    ) {
        cx.update(|window, cx| {
            probe.update(cx, |probe, cx| {
                f(probe);
                cx.notify();
            });
            window.draw(cx).clear(cx);
        });
    }

    fn narrow() -> Vec<CompletionItem> {
        vec![item("id", Some("int"))]
    }

    fn wide() -> Vec<CompletionItem> {
        vec![
            item("id", Some("integer · pk")),
            item("user_id", Some("integer · → users.id")),
        ]
    }

    fn documented(label: &str, documentation: Documentation) -> CompletionItem {
        CompletionItem {
            documentation: Some(documentation),
            ..item(label, None)
        }
    }

    fn markdown(value: &str) -> Documentation {
        Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: value.into(),
        })
    }

    /// The list used to measure one row at the width it was drawn at last,
    /// so an open menu could narrow and never widen again.
    #[gpui::test]
    fn the_menu_widens_again_after_a_narrow_list(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);

        let (_, first) = show(&probe, narrow(), cx);
        let (measured, wide_bounds) = show(&probe, wide(), cx);
        let (_, again) = show(&probe, narrow(), cx);
        let (_, wide_again) = show(&probe, wide(), cx);

        assert!(wide_bounds.size.width > first.size.width);
        assert_eq!(
            wide_bounds.size.width, measured,
            "the list is as wide as its widest row"
        );
        assert_eq!(again.size.width, first.size.width);
        assert_eq!(wide_again.size.width, wide_bounds.size.width);
    }

    /// With its documentation above or below it, the popover used to take
    /// the whole `max_width` whatever its rows.
    #[gpui::test]
    fn a_menu_with_its_documentation_below_is_as_wide_as_its_rows(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let rows = |cx: &mut VisualTestContext| {
            let mut items = wide();
            items[0].documentation = Some(markdown("The key."));
            show(&probe, items, cx)
        };
        let (_, beside) = rows(cx);
        assert_eq!(
            placed(&probe, cx).docs.map(|d| d.left() > beside.right()),
            Some(true)
        );

        // no room beside the list in a window this narrow, so it goes below
        cx.simulate_resize(size(px(320.), px(700.)));
        let (measured, column) = rows(cx);
        let docs = placed(&probe, cx).docs.expect("documented");
        assert!(docs.top() > column.bottom(), "below the list");

        assert_eq!(column.size.width, measured);
        assert_eq!(column.size.width, beside.size.width);
    }

    /// A row wider than the ceiling is cut by the popover, and the rows of a
    /// narrow list fill the popover at its floor.
    #[gpui::test]
    fn the_list_fills_the_popover_between_its_floor_and_ceiling(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let long = "x".repeat(200);

        let (measured, capped) = show(&probe, vec![item(&long, None)], cx);
        let (tiny, floor) = show(&probe, vec![item("a", None)], cx);

        let max_width = cx.update(|_, cx| {
            let state = probe.read(cx).state.clone();
            state.read(cx).lsp().completion_menu.max_width
        });
        assert!(measured > max_width);
        assert!(capped.size.width < max_width);
        assert!(capped.size.width > max_width - px(20.));
        assert!(floor.size.width > tiny, "the rows fill the popover's floor");
        assert!(floor.size.width < px(120.));
    }

    /// Near the window's right edge the popover moves left until it ends
    /// inside the window, rather than running past it; the room is counted
    /// from where the popover really starts, not from the input's own left
    /// edge.
    #[gpui::test]
    fn the_menu_stays_inside_the_window_at_its_right_edge(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let (window_width, padding) = cx.update(|window, _| {
            (
                window.viewport_size().width,
                MENU_PADDING.to_pixels(window.rem_size()),
            )
        });
        let (_, away) = show(&probe, wide(), cx);
        let (_, floor) = show(&probe, narrow(), cx);

        // the editor, and the cursor with it, 60 px short of the edge
        redraw(&probe, cx, |probe| probe.indent = window_width - px(60.));
        let (_, wide_at_edge) = show(&probe, wide(), cx);
        let (_, narrow_at_edge) = show(&probe, narrow(), cx);

        // the list is drawn inside the popover's padding, and it is the
        // popover that must end inside the window
        for (at_edge, away) in [(wide_at_edge, away), (narrow_at_edge, floor)] {
            assert!(
                at_edge.right() + padding + WINDOW_MARGIN <= window_width,
                "{at_edge:?} in a window {window_width:?} wide"
            );
            assert_eq!(at_edge.size.width, away.size.width, "the rows are whole");
        }
        assert!(wide_at_edge.left() < window_width - px(60.));
    }

    /// In a long list the rows laid out are those drawn widest, not those
    /// longest in bytes or in characters: an ideograph is one character and
    /// about two letters wide, and an arrow is three bytes and one letter.
    #[gpui::test]
    fn the_widest_row_is_found_by_its_width_not_its_length(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        // twelve ideographs from outside the Basic Multilingual Plane, which
        // the test text system draws two ems wide, as a real font draws any
        // ideograph: 12 characters, 48 bytes, 24 ems
        let widest = item(&"𠮷".repeat(12), None);
        let (alone, _) = show(&probe, vec![widest.clone(); MEASURED_ROWS + 8], cx);

        // each longer than the widest in characters (18) and in bytes (52),
        // and narrower: 18 ems and the gap
        let arrows = item("x", Some(&"→".repeat(17)));
        let mut items = vec![arrows; MEASURED_ROWS + 7];
        items.insert(MEASURED_ROWS / 2, widest);
        let (among, _) = show(&probe, items, cx);

        assert_eq!(among, alone);
    }

    /// Typing narrows a list the user may have scrolled; the first row is
    /// selected in the new one, so the new one is shown from the top.
    #[gpui::test]
    fn a_new_list_is_shown_from_its_first_row(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let menu = probe.read_with(cx, |probe, _| probe.menu.clone());
        let rows = |n| (0..n).map(|i| item(&format!("col_{i:03}"), None)).collect();
        let scroll_y = |cx: &mut VisualTestContext| {
            cx.read(|cx| {
                let list = menu.read(cx).list.read(cx);
                list.scroll_handle().base_handle().offset().y
            })
        };

        show(&probe, rows(300), cx);
        cx.update(|window, cx| {
            menu.update(cx, |menu, cx| {
                menu.list.update(cx, |list, cx| {
                    list.scroll_to_item(IndexPath::new(200), ScrollStrategy::Top, window, cx)
                })
            });
            window.draw(cx).clear(cx);
        });
        assert!(scroll_y(cx) < px(0.), "the list scrolled down");

        show(&probe, rows(120), cx);
        assert_eq!(scroll_y(cx), px(0.));
    }

    /// The menu is placed under the caret where the caret is drawn in the
    /// same frame. Placed from the layout the editor kept at its last
    /// paint, it was a frame behind after an edit, and every keystroke with
    /// a list open drew a frame more to catch up.
    #[gpui::test]
    fn the_menu_is_under_the_caret_in_the_frame_of_an_edit(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let state = probe.read_with(cx, |probe, _| probe.state.clone());
        let (_, before) = show(&probe, wide(), cx);
        let caret = |cx: &mut VisualTestContext| {
            cx.read(|cx| state.read(cx).painted_caret().expect("laid out").bounds)
        };
        let at = caret(cx);
        assert_eq!(placed(&probe, cx).list.left(), at.left() - px(4.));
        assert!(
            before.left() > at.left() - px(4.),
            "the rows inside the popover"
        );

        cx.update(|window, cx| {
            state.update(cx, |state, cx| state.insert("select ", window, cx));
            window.draw(cx).clear(cx);
        });
        let now = placed(&probe, cx).list;
        assert!(caret(cx).left() > at.left(), "the caret moved");
        assert_eq!(
            now.left(),
            caret(cx).left() - px(4.),
            "and the menu with it"
        );
        assert!(now.top() > caret(cx).bottom(), "below its line");
    }

    /// Near the window's bottom a list that does not fit below the caret's
    /// line opens above it: running off the window, its last rows lay over
    /// whatever is below the editor, and a click there took one of them.
    #[gpui::test]
    fn a_list_near_the_bottom_opens_above_the_caret(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let height = cx.update(|window, _| window.viewport_size().height);
        // an editor with room for the list under its first line
        redraw(&probe, cx, |probe| probe.height = px(600.));
        let rows = |n| {
            (0..n)
                .map(|i| item(&format!("row_{i}"), None))
                .collect::<Vec<_>>()
        };

        show(&probe, rows(20), cx);
        let top = placed(&probe, cx);
        assert!(!top.above, "room below at the top of the window");

        // the editor's first line 40 px from the window's bottom
        redraw(&probe, cx, |probe| probe.top = height - px(40.));
        show(&probe, rows(20), cx);
        let bottom = placed(&probe, cx);
        let line = cx.read(|cx| {
            let state = probe.read(cx).state.read(cx);
            state.painted_caret().expect("laid out").line()
        });
        assert!(bottom.above);
        assert!(bottom.list.bottom() <= line.top(), "above the caret's line");
        assert!(bottom.list.bottom() <= height);
        assert_eq!(bottom.list.size, top.list.size, "the same list, whole");

        // a list that fits below the line in the editor still goes there
        redraw(&probe, cx, |probe| probe.top = height - px(200.));
        show(&probe, rows(2), cx);
        assert!(!placed(&probe, cx).above);
    }

    /// The placement's own rules: below when the list fits under the line
    /// inside the editor, else above when it fits there, else the side with
    /// more room; the documentation beside the list where it fits, else
    /// past it.
    #[test]
    fn where_a_list_and_its_documentation_go() {
        let editor = Bounds::new(point(px(0.), px(0.)), size(px(800.), px(600.)));
        let window = Bounds::new(point(px(0.), px(0.)), size(px(800.), px(620.)));
        let line = |top: f32| Bounds::new(point(px(100.), px(top)), size(px(2.), px(20.)));
        let below = |top: f32, height: f32| goes_below(line(top), editor, window, px(height));
        assert!(below(0., 240.));
        assert!(!below(500., 240.));
        assert!(below(300., 240.), "fits below: 276 px");
        // fits neither side: the side with more room
        assert!(!below(350., 400.));
        assert!(below(150., 500.));
        // an editor mostly out of the window, the line 40 px from its
        // bottom: the window has the room above
        let low = Bounds::new(point(px(0.), px(580.)), size(px(800.), px(40.)));
        assert!(!goes_below(line(580.), low, window, px(240.)));
        // below in the window, where the editor has no room either side
        let short = Bounds::new(point(px(0.), px(0.)), size(px(800.), px(60.)));
        assert!(goes_below(line(20.), short, window, px(240.)));

        let list = |left: f32| Bounds::new(point(px(left), px(0.)), size(px(300.), px(200.)));
        assert_eq!(
            docs_side(list(100.), px(1440.), px(520.)),
            (DocsSide::Right, px(520.))
        );
        assert_eq!(
            docs_side(list(900.), px(1440.), px(520.)),
            (DocsSide::Left, px(520.))
        );
        // not the whole width on either side, but enough on one
        let (side, width) = docs_side(list(400.), px(1100.), px(520.));
        assert_eq!(side, DocsSide::Right);
        assert_eq!(width, px(1100.) - WINDOW_MARGIN - px(704.));
        // more room on the left, and enough on the right: the right, as
        // IntelliJ and VS Code have it, where the left went over the tree
        let (side, width) = docs_side(list(486.), px(1221.), px(520.));
        assert_eq!(side, DocsSide::Right);
        assert_eq!(width, px(1221.) - WINDOW_MARGIN - px(790.));
        // too little on the right for the panel to read: all of it left
        assert_eq!(
            docs_side(list(900.), px(1500.), px(520.)),
            (DocsSide::Left, px(520.))
        );
        assert_eq!(
            docs_side(list(200.), px(700.), px(520.)).0,
            DocsSide::Beyond
        );
    }

    /// A list shown while the documentation of a row is long: the panel
    /// stops at its height and scrolls, rather than running off the window;
    /// with its documentation below the list, the whole of it is there, not
    /// its first line.
    #[gpui::test]
    fn long_documentation_scrolls_in_a_panel_of_its_own_height(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        // an editor taller than the panel, which is kept inside it
        redraw(&probe, cx, |probe| probe.height = px(600.));
        let long = (0..80)
            .map(|i| format!("Paragraph {i} of the documentation."))
            .collect::<Vec<_>>()
            .join("\n\n");
        show(&probe, vec![documented("norm", markdown(&long))], cx);
        let docs = placed(&probe, cx).docs.expect("a panel");
        assert!(docs.size.height <= MAX_DOCS_HEIGHT, "{docs:?}");
        assert!(docs.size.height > MAX_DOCS_HEIGHT - px(20.), "{docs:?}");

        // below the list, where only its first line used to be shown
        cx.simulate_resize(size(px(320.), px(700.)));
        show(
            &probe,
            vec![documented("norm", markdown("One.\n\nTwo.\n\nThree."))],
            cx,
        );
        let short = placed(&probe, cx).docs.expect("a panel");
        show(&probe, vec![documented("norm", markdown("One."))], cx);
        let one = placed(&probe, cx).docs.expect("a panel");
        let list = placed(&probe, cx).list;
        assert!(one.top() > list.bottom(), "below the list");
        assert!(
            short.size.height > one.size.height * 2.,
            "{short:?} {one:?}"
        );
    }

    /// No panel for an item with nothing to say, not an empty one; plain
    /// text is text, not Markdown; and the detail is said above the
    /// documentation when the row shows the type instead.
    #[test]
    fn what_the_documentation_panel_says() {
        assert_eq!(docs_of(&item("len", None)), None);
        assert_eq!(docs_of(&documented("len", markdown("  \n "))), None);
        assert_eq!(
            docs_of(&documented("len", Documentation::String("a *b* c".into()))),
            Some((None, Some(DocsBody::Plain("a *b* c".into()))))
        );
        let plain = Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::PlainText,
            value: "x_y_z".into(),
        });
        assert_eq!(
            docs_of(&documented("len", plain)),
            Some((None, Some(DocsBody::Plain("x_y_z".into()))))
        );
        assert_eq!(
            docs_of(&documented("len", markdown("**Returns** the length."))),
            Some((
                None,
                Some(DocsBody::Markdown("**Returns** the length.".into()))
            ))
        );
        // the detail is on the row already: said once
        assert_eq!(docs_of(&item("len", Some("fn(&self) -> usize"))), None);
        let laid_out = CompletionItem {
            label_details: Some(CompletionItemLabelDetails {
                detail: Some("()".into()),
                description: Some("int".into()),
            }),
            ..item("length", Some("int java.lang.String.length()"))
        };
        assert_eq!(
            docs_of(&laid_out),
            Some((Some("int java.lang.String.length()".into()), None))
        );
    }

    /// An item without documentation shows no panel beside its row.
    #[gpui::test]
    fn an_item_without_documentation_has_no_panel(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        show(&probe, vec![documented("norm", markdown("The norm."))], cx);
        assert!(placed(&probe, cx).docs.is_some());
        show(&probe, vec![item("norm", None)], cx);
        assert_eq!(placed(&probe, cx).docs, None);
    }

    /// Where a label is painted as matched: the ranges given, cut to the
    /// label and its characters; else its start, as long as the filter
    /// text.
    #[test]
    fn a_label_is_painted_where_it_matched() {
        assert_eq!(
            label_highlights("HashMap", Some(&[0..1, 4..5]), 0),
            vec![0..1, 4..5]
        );
        assert_eq!(label_highlights("HashMap", None, 4), vec![0..4]);
        assert_eq!(
            label_highlights("HashMap", Some(&[]), 4),
            Vec::<Range<usize>>::new()
        );
        // past the end, inside a character, overlapping: cut or left out
        assert_eq!(label_highlights("ab", Some(&[0..9]), 0), vec![0..2]);
        assert_eq!(
            label_highlights("é", Some(&[0..1]), 0),
            Vec::<Range<usize>>::new()
        );
        assert_eq!(
            label_highlights("abcd", Some(&[0..2, 1..3, 3..4]), 0),
            vec![0..2, 3..4]
        );
    }

    #[test]
    fn a_kind_is_a_letter_and_a_colour() {
        use CompletionItemKind as K;
        assert_eq!(kind_letter(K::METHOD), Some(("m", KindColor::Callable)));
        assert_eq!(kind_letter(K::CLASS), Some(("C", KindColor::Type)));
        assert_eq!(kind_letter(K::FIELD), Some(("f", KindColor::Value)));
        assert_eq!(kind_letter(K::SNIPPET), Some(("s", KindColor::Snippet)));
        // every kind of the protocol has one, and no two of a colour share
        // a letter
        let kinds =
            (1..=25).map(|k| kind_letter(serde_json::from_value(serde_json::json!(k)).unwrap()));
        let letters: Vec<_> = kinds.map(|k| k.expect("a letter")).collect();
        for (i, a) in letters.iter().enumerate() {
            for b in &letters[i + 1..] {
                assert!(a != b, "{a:?} twice");
            }
        }
    }

    /// A provider that records what the menu tells it, and says where
    /// labels matched.
    #[derive(Default)]
    struct Recording {
        offered: RefCell<Vec<CompletionItem>>,
        selected: RefCell<Vec<(usize, String)>>,
        takes: bool,
    }

    impl input::CompletionProvider for Recording {
        fn completions(
            &self,
            _: &ropey::Rope,
            _: usize,
            _: lsp_types::CompletionContext,
            _: &mut Window,
            _: &mut App,
        ) -> gpui::Task<anyhow::Result<lsp_types::CompletionResponse>> {
            gpui::Task::ready(Ok(lsp_types::CompletionResponse::Array(vec![item(
                "from_the_provider",
                None,
            )])))
        }

        fn is_completion_trigger(&self, _: usize, _: &str, _: &mut App) -> bool {
            true
        }

        fn accept_completion(&self, item: &CompletionItem, _: &mut Window, _: &mut App) -> bool {
            self.offered.borrow_mut().push(item.clone());
            self.takes
        }

        fn completion_selected(
            &self,
            index: usize,
            item: &CompletionItem,
            _: &mut Window,
            _: &mut App,
        ) {
            self.selected.borrow_mut().push((index, item.label.clone()));
        }

        fn completion_label_matches(&self, item: &CompletionItem) -> Option<Vec<Range<usize>>> {
            (item.label == "HashMap").then(|| vec![0..1, 4..5])
        }
    }

    fn provide(
        probe: &Entity<MenuProbe>,
        provider: Rc<Recording>,
        cx: &mut VisualTestContext,
    ) -> Entity<EditorState> {
        let state = probe.read_with(cx, |probe, _| probe.state.clone());
        cx.update(|_, cx| {
            state.update(cx, |state, _| {
                state.lsp_mut().completion_provider = Some(provider)
            })
        });
        state
    }

    /// Enter on a row offers the item to the provider first: one that takes
    /// it gets the item as it was handed to the menu, `data` and all, and
    /// the editor inserts nothing; one that leaves it has the editor insert
    /// it, as before the hook.
    #[gpui::test]
    fn an_accepted_item_is_the_providers_to_take(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let menu = probe.read_with(cx, |probe, _| probe.menu.clone());
        let accept = |takes: bool, cx: &mut VisualTestContext| {
            let provider = Rc::new(Recording {
                takes,
                ..Default::default()
            });
            let state = provide(&probe, provider.clone(), cx);
            let greet = CompletionItem {
                data: Some(serde_json::json!({ "row": 7 })),
                ..item("greet", Some("fn(&str)"))
            };
            show(&probe, vec![greet], cx);
            cx.update(|window, cx| menu.update(cx, |menu, cx| menu.on_action_enter(window, cx)));
            cx.run_until_parked();
            let offered = provider.offered.borrow().clone();
            (cx.read(|cx| state.read(cx).value().to_string()), offered)
        };

        let (value, offered) = accept(true, cx);
        assert_eq!(value, "", "the provider took it: nothing inserted");
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].label, "greet");
        assert_eq!(offered[0].data, Some(serde_json::json!({ "row": 7 })));
        assert!(!cx.read(|cx| menu.read(cx).open), "the menu closed");

        let (value, offered) = accept(false, cx);
        assert_eq!(value, "greet", "left to the editor, which inserts it");
        assert_eq!(offered.len(), 1);
    }

    /// The provider is told which row is highlighted: the first of a list
    /// shown, and each the keyboard moves to, with nothing borrowed so that
    /// it can resolve the item and hand it back. Handed back in place, the
    /// item keeps its row highlighted and the list where it was scrolled; a
    /// list replaced since is left alone.
    #[gpui::test]
    fn the_highlighted_row_is_told_and_resolved_in_place(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let provider = Rc::new(Recording::default());
        let state = provide(&probe, provider.clone(), cx);
        let rows: Vec<CompletionItem> = (0..30)
            .map(|i| item(&format!("row_{i:02}"), None))
            .collect();
        // through the editor's state, as a host presents a list, so the
        // menu is kept in step with it as the input renders
        let present = |items: Vec<CompletionItem>, cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                state.update(cx, |state, cx| {
                    state.present_completion_items(0, "", items, cx)
                });
                window.draw(cx).clear(cx);
            });
            cx.run_until_parked();
        };
        let draw = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.run_until_parked();
        };
        present(rows.clone(), cx);
        let menu = own_menu(&state, cx);
        assert_eq!(
            provider.selected.borrow().as_slice(),
            [(0, "row_00".to_string())]
        );

        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                for _ in 0..20 {
                    state.route_overlay_action(Box::new(input::MoveDown), window, cx);
                }
            })
        });
        draw(cx);
        // the row it came to, not each it passed on the way
        let told = [(0, "row_00".to_string()), (20, "row_20".to_string())];
        assert_eq!(provider.selected.borrow().as_slice(), told);
        let (selected, scroll) = cx.read(|cx| {
            let list = menu.read(cx).list.read(cx);
            (
                list.delegate().selected_ix,
                list.scroll_handle().base_handle().offset().y,
            )
        });
        assert_eq!(selected, 20);
        assert!(scroll < px(0.), "scrolled to it");

        // resolved: its documentation handed back for the row
        let resolved = CompletionItem {
            documentation: Some(markdown("Row twenty.")),
            ..rows[20].clone()
        };
        let replaced = cx.update(|_, cx| {
            state.update(cx, |state, cx| {
                state.replace_completion_item(20, &rows[20], resolved.clone(), cx)
            })
        });
        assert!(replaced);
        draw(cx);
        let (selected, scroll_after, docs) = cx.read(|cx| {
            let menu = menu.read(cx);
            let list = menu.list.read(cx);
            (
                list.delegate().selected_ix,
                list.scroll_handle().base_handle().offset().y,
                list.delegate().items[20].documentation.clone(),
            )
        });
        assert_eq!(selected, 20, "the row stays highlighted");
        assert_eq!(scroll_after, scroll, "and the list where it was");
        assert_eq!(docs, resolved.documentation);
        // the row handed back is not news: told again, a provider that
        // resolves each row it is told of would ask for it without end
        assert_eq!(provider.selected.borrow().as_slice(), told);
        assert!(
            cx.read(|cx| menu.read(cx).placed())
                .and_then(|p| p.docs)
                .is_some(),
            "its documentation shown"
        );

        // an item that is not the one there any more is not put in
        let stale = cx.update(|_, cx| {
            state.update(cx, |state, cx| {
                state.replace_completion_item(3, &rows[20], resolved, cx)
            })
        });
        assert!(!stale);
    }

    /// Where a label matched is the provider's to say, and painted there.
    #[gpui::test]
    fn the_provider_says_where_a_label_matched(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        provide(&probe, Rc::new(Recording::default()), cx);
        let menu = probe.read_with(cx, |probe, _| probe.menu.clone());
        show(&probe, vec![item("HashMap", None), item("hash", None)], cx);
        let matches = cx.read(|cx| menu.read(cx).list.read(cx).delegate().matches.clone());
        assert_eq!(matches[0].as_deref(), Some(&[0..1, 4..5][..]));
        assert_eq!(matches[1], None);
    }

    /// The kind column is drawn when the editor asks for it: every row is
    /// wider by it.
    #[gpui::test]
    fn rows_begin_with_their_kind_when_asked(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let rows = || {
            vec![CompletionItem {
                kind: Some(CompletionItemKind::METHOD),
                label_details: Some(CompletionItemLabelDetails {
                    detail: Some("(int index)".into()),
                    description: Some("char".into()),
                }),
                ..item("charAt", None)
            }]
        };
        let (without, _) = show(&probe, rows(), cx);
        let state = probe.read_with(cx, |probe, _| probe.state.clone());
        cx.update(|_, cx| {
            state.update(cx, |state, _| {
                state.lsp_mut().completion_menu.show_kinds = true
            })
        });
        let (with, _) = show(&probe, rows(), cx);
        let rem = cx.update(|window, _| window.rem_size());
        assert_eq!(
            with,
            without + KIND_SIZE.to_pixels(rem) + ROW_GAP.to_pixels(rem)
        );
        // the signature and the type are both measured
        let (bare, _) = show(&probe, vec![item("charAt", None)], cx);
        assert!(with > bare + px(40.));
    }

    /// The menu follows its editor's state as it closes, and tells it
    /// nothing: a close it told the editor of a moment later, by a task of
    /// its own, closed a list presented in that moment.
    #[gpui::test]
    fn a_list_presented_as_the_menu_follows_a_close_stays_open(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let state = probe.read_with(cx, |probe, _| probe.state.clone());
        let present = |labels: &[&str], cx: &mut VisualTestContext| {
            let items = labels.iter().map(|l| item(l, None)).collect();
            cx.update(|window, cx| {
                state.update(cx, |state, cx| {
                    state.present_completion_items(0, "", items, cx)
                });
                window.draw(cx).clear(cx);
            });
        };
        // a problem's popover keeps the menu drawn with the state closed
        cx.update(|_, cx| {
            state.update(cx, |state, cx| {
                state.present_diagnostic(gpui_base::input::DiagnosticEntry::default(), cx)
            })
        });
        present(&["first"], cx);
        let menu = own_menu(&state, cx);
        assert!(cx.read(|cx| menu.read(cx).open));

        // an empty list closes the state, the menu follows it, and a list
        // comes before anything the menu did is done
        present(&[], cx);
        assert!(!cx.read(|cx| menu.read(cx).open));
        present(&["second"], cx);
        cx.run_until_parked();
        assert!(
            cx.read(|cx| state.read(cx).completion_menu_state().open),
            "still open"
        );
        draw(cx);
        assert!(cx.read(|cx| menu.read(cx).open));
    }

    /// A provider's answer that is ready at once is shown in the update of
    /// the keystroke that asked, and drawn in its frame, not a frame later.
    #[gpui::test]
    fn an_answer_ready_at_once_is_shown_with_its_keystroke(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let state = provide(&probe, Rc::new(Recording::default()), cx);
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.focus(window, cx);
                gpui::EntityInputHandler::replace_text_in_range(state, None, "s", window, cx);
                let menu = state.completion_menu_state();
                assert!(menu.open, "shown before the update ends");
                assert_eq!(menu.items[0].label, "from_the_provider");
            })
        });
    }

    /// The scrollbar is drawn over the rows' ends, so a list that scrolls is
    /// wider by its track, and one that does not is not; and the rows of one
    /// that scrolls stop short of the track, so that the type at their right
    /// is not under it.
    #[gpui::test]
    fn a_list_that_scrolls_keeps_its_rows_clear_of_the_scrollbar(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let row = item("total_amount_in_cents", Some("integer · orders"));
        let gutter = |cx: &mut VisualTestContext| {
            cx.read(|cx| probe.read(cx).menu.read(cx).list.read(cx).delegate().gutter)
        };

        let (short, _) = show(&probe, vec![row.clone(); 3], cx);
        assert_eq!(gutter(cx), px(0.));
        let (long, _) = show(&probe, vec![row; 40], cx);
        assert_eq!(long - short, gutter(cx));

        let track = cx.update(|_, cx| {
            gpui_base::Theme::global(cx)
                .scrollbar
                .styles()
                .track_width()
        });
        assert!(track > px(0.));
        assert_eq!(long, short + track);
    }

    fn laid_out(label: &str, signature: &str, description: &str) -> CompletionItem {
        CompletionItem {
            label_details: Some(CompletionItemLabelDetails {
                detail: Some(signature.into()),
                description: Some(description.into()),
            }),
            ..item(label, None)
        }
    }

    fn bounds_of(selector: &'static str, cx: &mut VisualTestContext) -> Bounds<Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} was drawn"))
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
    }

    /// A row too wide for the menu keeps its name whole: its signature is
    /// cut first, and its type once the signature is gone, at the row's
    /// end. The type used to keep its width while the name shrank to
    /// nothing, and rust-analyzer's `fn(…)` types drew rows with no name.
    #[gpui::test]
    fn a_row_too_wide_keeps_its_name_whole(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let name = "with_a_very_long_signature";
        show(&probe, vec![laid_out(name, "", "")], cx);
        let alone = bounds_of("completion-label", cx).size.width;
        show(&probe, vec![laid_out("x", "", "bool")], cx);
        let bool_width = bounds_of("completion-type", cx).size.width;

        // a type wider than the menu: the name whole, the signature gone,
        // and the type cut at the row's end
        let long_type = format!(
            "fn(&self, {}) -> bool",
            "HashMap<String, Vec<i32>>, ".repeat(12)
        );
        let (_, list) = show(&probe, vec![laid_out(name, "(…)", &long_type)], cx);
        let (label, ty) = (
            bounds_of("completion-label", cx),
            bounds_of("completion-type", cx),
        );
        assert_eq!(label.size.width, alone, "the name whole");
        assert!(label.left() >= list.left());
        assert!(ty.left() >= label.right(), "{label:?} {ty:?}");
        assert!(ty.right() <= list.right(), "{ty:?} inside {list:?}");
        assert!(ty.size.width > px(40.), "the type drawn, cut: {ty:?}");
        let signature = bounds_of("completion-signature", cx);
        assert!(signature.size.width < px(1.), "cut first: {signature:?}");

        // a signature wider than the menu and a short type: the name and
        // the type whole, the signature cut between them
        let long_signature = format!("({})", "index: usize, ".repeat(40));
        let (_, list) = show(&probe, vec![laid_out(name, &long_signature, "bool")], cx);
        let (label, signature, ty) = (
            bounds_of("completion-label", cx),
            bounds_of("completion-signature", cx),
            bounds_of("completion-type", cx),
        );
        assert_eq!(label.size.width, alone);
        assert_eq!(ty.size.width, bool_width, "the type whole");
        assert!(signature.left() >= label.right(), "{signature:?}");
        assert!(signature.right() <= ty.left(), "{signature:?} {ty:?}");
        assert!(ty.right() <= list.right());
    }

    /// Documentation longer than the panel scrolls in it: the wheel over the
    /// panel moves the text, the panel stays, and the next row's
    /// documentation is shown from its top. The panel used to clip what did
    /// not fit, and nothing past it could be reached.
    #[gpui::test]
    fn the_documentation_scrolls_under_the_wheel(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let long = (0..80)
            .map(|i| format!("Paragraph {i} of the documentation."))
            .collect::<Vec<_>>()
            .join("\n\n");
        show(
            &probe,
            vec![
                documented("norm", markdown(&long)),
                documented("north", markdown(&long)),
            ],
            cx,
        );
        let docs = placed(&probe, cx).docs.expect("a panel");
        let top = bounds_of("completion-docs-content", cx).top();
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: docs.center(),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-200.))),
            ..Default::default()
        });
        draw(cx);
        let scrolled = bounds_of("completion-docs-content", cx).top();
        assert!(scrolled < top - px(100.), "{top:?} to {scrolled:?}");
        assert_eq!(placed(&probe, cx).docs, Some(docs), "the panel stays");

        let menu = probe.read_with(cx, |probe, _| probe.menu.clone());
        cx.update(|window, cx| menu.update(cx, |menu, cx| menu.on_action_down(window, cx)));
        draw(cx);
        assert_eq!(bounds_of("completion-docs-content", cx).top(), top);
    }

    /// A click in the documentation panel leaves the keyboard with the
    /// editor, and the list open. The text in the panel took it, and the
    /// keys typed after went nowhere while the list stayed.
    #[gpui::test]
    fn a_click_in_the_documentation_leaves_the_keyboard_with_the_editor(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let state = probe.read_with(cx, |probe, _| probe.state.clone());
        cx.update(|window, cx| state.update(cx, |state, cx| state.focus(window, cx)));
        show(
            &probe,
            vec![documented("norm", markdown("The norm of the vector."))],
            cx,
        );
        let text = bounds_of("completion-docs-content", cx);
        cx.simulate_click(text.origin + point(px(6.), px(6.)), gpui::Modifiers::none());
        draw(cx);
        let focused = cx.update(|window, cx| {
            gpui::Focusable::focus_handle(state.read(cx), cx).is_focused(window)
        });
        assert!(focused, "the editor has the keyboard");
        assert!(
            cx.read(|cx| probe.read(cx).menu.read(cx).open),
            "the list open"
        );
    }

    /// The documentation beside a list below the caret stays inside the
    /// editor: near its bottom the panel moves up, rather than over what is
    /// under the editor, a status bar, down to the window's edge.
    #[gpui::test]
    fn the_documentation_stays_inside_the_editor(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let height = cx.update(|window, _| window.viewport_size().height);
        // the editor 200 px tall, its bottom 60 px above the window's
        redraw(&probe, cx, |probe| probe.top = height - px(260.));
        let long = (0..80)
            .map(|i| format!("Paragraph {i}."))
            .collect::<Vec<_>>()
            .join("\n\n");
        show(&probe, vec![documented("norm", markdown(&long))], cx);
        let placed = placed(&probe, cx);
        let docs = placed.docs.expect("a panel");
        assert!(!placed.above, "the list below the caret");
        assert!(
            docs.bottom() <= height - px(60.),
            "{docs:?} over the status bar"
        );
        assert!(
            docs.top() >= height - px(260.),
            "{docs:?} inside the editor"
        );
        assert!(
            docs.size.height > px(150.),
            "as tall as the editor lets it: {docs:?}"
        );
    }

    /// With its caret scrolled out of the editor's view the menu closes: the
    /// list was shown nowhere, and Enter took its highlighted row all the
    /// same.
    #[gpui::test]
    fn the_menu_closes_when_the_caret_scrolls_out_of_view(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let state = probe.read_with(cx, |probe, _| probe.state.clone());
        let text = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_value(text, window, cx);
                state.set_selected_range(0..0, cx);
            });
            window.draw(cx).clear(cx);
        });
        show(&probe, vec![item("norm", None)], cx);
        let menu = probe.read_with(cx, |probe, _| probe.menu.clone());
        assert!(cx.read(|cx| menu.read(cx).open));
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: point(px(600.), px(150.)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-2000.))),
            ..Default::default()
        });
        draw(cx);
        draw(cx);
        assert!(
            !cx.read(|cx| menu.read(cx).open),
            "closed with its caret out of view"
        );
    }

    /// A close that is the user's, a click on a row or away from the list,
    /// closes the list even when one was presented since the last frame. The
    /// menu's check for its own close took it for one, the list stayed in
    /// the editor, and the next frame showed it again.
    #[gpui::test]
    fn a_users_close_closes_a_list_presented_since_the_last_frame(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let state = probe.read_with(cx, |probe, _| probe.state.clone());
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.present_completion_items(0, "", vec![item("first", None)], cx)
            });
            window.draw(cx).clear(cx);
        });
        let menu = own_menu(&state, cx);
        cx.update(|_, cx| {
            state.update(cx, |state, cx| {
                state.present_completion_items(0, "", vec![item("second", None)], cx)
            });
            menu.update(cx, |menu, cx| menu.hide(cx));
        });
        cx.run_until_parked();
        assert!(!cx.read(|cx| state.read(cx).completion_menu_state().open));
        draw(cx);
        let shown = cx.read(|cx| {
            crate::input::overlay::completion_menu_of(&state, cx)
                .is_some_and(|menu| menu.read(cx).open)
        });
        assert!(!shown, "not shown again");
    }

    /// In an editor too short for the list on either side of the caret's
    /// line, the list is as tall as the side with more room lets it be, and
    /// scrolls. It went past the editor, over a status bar, and a click
    /// there took one of its rows.
    #[gpui::test]
    fn a_list_in_a_short_editor_stays_inside_it(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let height = cx.update(|window, _| window.viewport_size().height);
        let (top, bottom) = (height - px(180.), height - px(60.));
        redraw(&probe, cx, |probe| {
            probe.top = top;
            probe.height = bottom - top;
        });
        let rows = (0..20).map(|i| item(&format!("row_{i}"), None)).collect();
        show(&probe, rows, cx);
        let placed = placed(&probe, cx);
        assert!(
            placed.list.top() >= top && placed.list.bottom() <= bottom,
            "{:?} inside {top:?}..{bottom:?}",
            placed.list
        );
        assert!(!placed.above, "below the caret, where the room is");

        // given the room again, the list is drawn whole again: the frame
        // the editor grows in finds the list short, and draws it again
        redraw(&probe, cx, |probe| {
            probe.top = px(0.);
            probe.height = px(600.);
        });
        draw(cx);
        let whole = cx
            .read(|cx| probe.read(cx).menu.read(cx).placed())
            .expect("placed");
        assert!(
            whole.list.size.height > placed.list.size.height + px(80.),
            "{whole:?}"
        );
    }

    /// A list handed again with the caret where it was, a late answer or
    /// one asked for again with no key typed since, keeps highlighted the
    /// row the keyboard moved to, where the new list has it: the highlight
    /// went back to the first row, and an Enter pressed as the list came
    /// took that one. After a key typed a list starts from its first row.
    #[gpui::test]
    fn a_list_handed_again_keeps_the_row_moved_to(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let state = probe.read_with(cx, |probe, _| probe.state.clone());
        let present = |labels: &[&str], cx: &mut VisualTestContext| {
            let items = labels.iter().map(|l| item(l, None)).collect();
            cx.update(|_, cx| {
                state.update(cx, |state, cx| {
                    state.present_completion_items(0, "", items, cx)
                })
            });
            draw(cx);
        };
        let highlighted = |cx: &mut VisualTestContext| {
            let menu = own_menu(&state, cx);
            cx.read(|cx| {
                let delegate = menu.read(cx).list.read(cx).delegate();
                delegate.items[delegate.selected_ix].label.clone()
            })
        };
        present(&["alpha", "beta", "gamma", "delta"], cx);
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                for _ in 0..2 {
                    state.route_overlay_action(Box::new(input::MoveDown), window, cx);
                }
            })
        });
        draw(cx);
        assert_eq!(highlighted(cx), "gamma");
        present(&["omega", "alpha", "beta", "gamma", "delta"], cx);
        assert_eq!(highlighted(cx), "gamma", "the row moved to");
        present(&["omega", "alpha"], cx);
        assert_eq!(highlighted(cx), "omega", "not in the list: the first");

        present(&["alpha", "beta", "gamma"], cx);
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.route_overlay_action(Box::new(input::MoveDown), window, cx);
                state.insert("x", window, cx);
            })
        });
        present(&["alpha", "beta", "gamma"], cx);
        assert_eq!(highlighted(cx), "alpha", "a key typed since");
    }
}
