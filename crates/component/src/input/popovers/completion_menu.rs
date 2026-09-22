use std::{cmp::Reverse, rc::Rc};

use gpui::{
    Action, AnyElement, App, AppContext, AvailableSpace, Context, DismissEvent, Empty, Entity,
    EventEmitter, Half as _, HighlightStyle, InteractiveElement as _, IntoElement, ParentElement,
    Pixels, Point, Rems, Render, RenderOnce, SharedString, Styled, StyledText, Subscription,
    WeakEntity, Window, deferred, div, prelude::FluentBuilder, px, relative, rems, size,
};
use lsp_types::CompletionItem;

const MAX_MENU_HEIGHT: Pixels = px(240.);
const MIN_MENU_WIDTH: Pixels = px(120.);
/// The popover's padding around the list. It is `editor_popover`'s own,
/// set here as well because where the popover fits is worked out from it.
const MENU_PADDING: Rems = rems(0.25);
const POPOVER_GAP: Pixels = px(4.);
/// How many rows are laid out to find the widest: the longest by characters,
/// so that a long list costs this many layouts and not one per row.
const MEASURED_ROWS: usize = 32;

use crate::{
    ActiveTheme, IndexPath, Selectable, actions, h_flex,
    input::{
        self, EditorState,
        popovers::{editor_popover, render_markdown},
    },
    label::Label,
    list::{List, ListDelegate, ListEvent, ListState},
};

struct ContextMenuDelegate {
    query: SharedString,
    menu: Entity<CompletionMenu>,
    items: Vec<Rc<CompletionItem>>,
    selected_ix: usize,
}

/// How long a row's text is, which is what picks the rows worth measuring.
/// Characters, not bytes: `·` is two bytes and `→` three, and neither is
/// wider than a letter.
fn row_chars(item: &CompletionItem) -> usize {
    item.label.chars().count() + item.detail.as_deref().map_or(0, |d| d.chars().count())
}

impl ContextMenuDelegate {
    fn set_items(&mut self, items: Vec<CompletionItem>) {
        self.items = items.into_iter().map(Rc::new).collect();
        self.selected_ix = 0;
    }

    fn selected_item(&self) -> Option<&Rc<CompletionItem>> {
        self.items.get(self.selected_ix)
    }
}

#[derive(IntoElement)]
struct CompletionMenuItem {
    ix: usize,
    item: Rc<CompletionItem>,
    children: Vec<AnyElement>,
    selected: bool,
    highlight_prefix: SharedString,
}

impl CompletionMenuItem {
    fn new(ix: usize, item: Rc<CompletionItem>) -> Self {
        Self {
            ix,
            item,
            children: vec![],
            selected: false,
            highlight_prefix: "".into(),
        }
    }

    fn highlight_prefix(mut self, s: impl Into<SharedString>) -> Self {
        self.highlight_prefix = s.into();
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
impl RenderOnce for CompletionMenuItem {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let item = self.item;

        let deprecated = item.deprecated.unwrap_or(false);
        let matched_len = item
            .filter_text
            .as_ref()
            .map(|s| s.len())
            .unwrap_or(self.highlight_prefix.len())
            .min(item.label.len());

        let highlights = vec![(
            0..matched_len,
            HighlightStyle {
                color: Some(cx.theme().blue),
                ..Default::default()
            },
        )];

        h_flex()
            .id(self.ix)
            .gap_2()
            .p_1()
            .text_xs()
            .line_height(relative(1.))
            .rounded(cx.theme().radius.half())
            .when(item.deprecated.unwrap_or(false), |this| this.line_through())
            .hover(|this| this.bg(cx.theme().accent.opacity(0.8)))
            .when(self.selected, |this| {
                this.bg(cx.theme().tokens.accent)
                    .text_color(cx.theme().accent_foreground)
            })
            .child(div().child(StyledText::new(item.label.clone()).with_highlights(highlights)))
            .when(item.detail.is_some(), |this| {
                this.child(
                    Label::new(item.detail.as_deref().unwrap_or("").to_string())
                        .text_color(cx.theme().muted_foreground)
                        .when(deprecated, |this| this.line_through())
                        .italic(),
                )
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
        let item = self.items.get(ix.row)?;
        Some(CompletionMenuItem::new(ix.row, item.clone()).highlight_prefix(self.query.clone()))
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
    /// render after the items change. `None` until then.
    width: Option<Pixels>,
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
                _subscriptions,
            }
        })
    }

    fn select_item(&mut self, item: &CompletionItem, window: &mut Window, cx: &mut Context<Self>) {
        let item = item.clone();
        let range = self.trigger_start_offset.unwrap_or(self.offset)..self.offset;

        let editor = self.editor.clone();

        cx.spawn_in(window, async move |_, cx| {
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
        self.list.update(cx, |this, cx| {
            this.on_action_select_prev(&actions::SelectUp, window, cx)
        });
    }

    fn on_action_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list.update(cx, |this, cx| {
            this.on_action_select_next(&actions::SelectDown, window, cx)
        });
    }

    /// Hide the completion menu and reset the trigger start offset.
    pub(crate) fn hide(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.trigger_start_offset = None;
        let editor = self.editor.clone();
        cx.spawn(async move |_, cx| {
            let _ = editor.update(cx, |editor, cx| editor.dismiss_completion_overlay(cx));
        })
        .detach();
        cx.notify();
    }

    /// Sets the trigger start offset if it is not already set.
    pub(crate) fn update_query(&mut self, start_offset: usize, query: impl Into<SharedString>) {
        if self.trigger_start_offset.is_none() {
            self.trigger_start_offset = Some(start_offset);
        }
        self.query = query.into();
    }

    pub(crate) fn show(
        &mut self,
        offset: usize,
        items: impl Into<Vec<CompletionItem>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let items = items.into();
        self.offset = offset;
        self.open = true;
        self.width = None;
        self.list.update(cx, |this, cx| {
            let longest_ix = items
                .iter()
                .enumerate()
                .max_by_key(|(_, item)| row_chars(item))
                .map(|(ix, _)| ix)
                .unwrap_or(0);

            this.delegate_mut().query = self.query.clone();
            this.delegate_mut().set_items(items);
            this.set_selected_index(Some(IndexPath::new(0)), window, cx);
            this.set_item_to_measure_index(IndexPath::new(longest_ix), window, cx);
        });

        cx.notify();
    }

    /// How wide the list must be for its widest row to show whole: the rows
    /// laid out unwrapped, as they are painted, plus the scrollbar's track
    /// when the rows overflow the menu, because the track is drawn over
    /// their ends.
    ///
    /// The list cannot tell this itself. It measures one row at the width
    /// it was drawn at last, so a menu that stays open could narrow but
    /// never widen again, and a menu laid out below a documentation panel
    /// took the whole `max_width` whatever its rows.
    ///
    /// Only the longest [`MEASURED_ROWS`] rows by characters are laid out.
    /// Layout asserts it runs inside a draw, so this is called from `render`.
    fn measure_width(&self, window: &mut Window, cx: &mut App) -> Pixels {
        let (query, items) = {
            let delegate = self.list.read(cx).delegate();
            (delegate.query.clone(), delegate.items.clone())
        };

        let mut candidates: Vec<usize> = (0..items.len()).collect();
        if candidates.len() > MEASURED_ROWS {
            candidates.select_nth_unstable_by_key(MEASURED_ROWS - 1, |&ix| {
                Reverse(row_chars(&items[ix]))
            });
            candidates.truncate(MEASURED_ROWS);
        }

        let available_space = size(AvailableSpace::MaxContent, AvailableSpace::MinContent);
        let (mut width, mut row_height) = (px(0.), px(0.));
        for ix in candidates {
            let row = CompletionMenuItem::new(ix, items[ix].clone())
                .highlight_prefix(query.clone())
                .into_any_element()
                .layout_as_root(available_space, window, cx);
            width = width.max(row.width);
            row_height = row_height.max(row.height);
        }

        if row_height * items.len() as f32 > MAX_MENU_HEIGHT {
            width += cx
                .try_global::<gpui_base::Theme>()
                .map(|theme| theme.scrollbar.styles().track_width())
                .unwrap_or_else(|| gpui_base::ScrollbarStyles::default().track_width());
        }
        width
    }

    fn origin(&self, cx: &App) -> Option<Point<Pixels>> {
        let editor = self.editor.upgrade()?;
        let editor = editor.read(cx);
        let Some((cursor_bounds, line_height)) = editor.cursor_layout() else {
            return None;
        };
        let cursor_origin = cursor_bounds.origin;

        let scroll_origin = editor.scroll_offset();

        Some(
            scroll_origin + cursor_origin - editor.input_bounds().origin
                + Point::new(-px(4.), line_height + px(4.)),
        )
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

        let Some(pos) = self.origin(cx) else {
            return Empty.into_any_element();
        };

        let selected_documentation = self
            .list
            .read(cx)
            .delegate()
            .selected_item()
            .and_then(|item| item.documentation.clone());

        let Some(editor) = self.editor.upgrade() else {
            return Empty.into_any_element();
        };
        let width = match self.width {
            Some(width) => width,
            None => {
                let width = self.measure_width(window, cx);
                self.width = Some(width);
                width
            }
        };
        let configured_max = editor.read(cx).lsp().completion_menu.max_width;
        let window_width = window.bounds().size.width;
        let abs_pos = editor.read(cx).input_bounds().origin + pos;
        // The popover is the list and its padding, between its floor and its
        // ceiling. Where that would run past the window's right edge, it is
        // moved left until it ends inside it, but never past the left edge.
        // (`pos` is relative to the input, so the room is counted from
        // `abs_pos`.)
        let popover_width = (width + MENU_PADDING.to_pixels(window.rem_size()) * 2.)
            .min(configured_max)
            .max(MIN_MENU_WIDTH);
        let shift = (abs_pos.x + popover_width - window_width)
            .min(abs_pos.x)
            .max(px(0.));
        let max_width = configured_max.min(window_width - (abs_pos.x - shift));
        let vertical_layout =
            abs_pos.x + configured_max + POPOVER_GAP + configured_max + POPOVER_GAP
                > window.bounds().size.width;

        deferred(
            div()
                .absolute()
                .left(pos.x - shift)
                .top(pos.y)
                .flex()
                .flex_row()
                .gap(POPOVER_GAP)
                .items_start()
                .when(vertical_layout, |this| this.flex_col())
                .child(
                    // The list asks for its widest row, the popover clamps
                    // that between its floor and `max_width`, and the list
                    // then fills the popover: the rows fill it at the floor,
                    // and a row wider than the ceiling is cut by it.
                    editor_popover("completion-menu", cx)
                        .p(MENU_PADDING)
                        .max_w(max_width)
                        .min_w(MIN_MENU_WIDTH)
                        .child(
                            List::new(&self.list)
                                .w(width)
                                .min_w_full()
                                .max_w_full()
                                .max_h(MAX_MENU_HEIGHT),
                        ),
                )
                .when_some(selected_documentation, |this, documentation| {
                    let mut doc = match documentation {
                        lsp_types::Documentation::String(s) => s.clone(),
                        lsp_types::Documentation::MarkupContent(mc) => mc.value.clone(),
                    };
                    if vertical_layout {
                        doc = doc.split("\n").next().unwrap_or_default().to_string();
                    }

                    this.child(
                        div().child(
                            editor_popover("completion-menu", cx)
                                .w(configured_max)
                                .px_2()
                                .child(render_markdown("doc", doc, window, cx)),
                        ),
                    )
                })
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.hide(cx);
                })),
        )
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Editor;
    use gpui::{Bounds, TestAppContext, VisualTestContext};

    /// An editor with a completion menu of its own beside it, so the tests
    /// can hand the menu any list and read back how it was drawn.
    struct MenuProbe {
        state: Entity<EditorState>,
        menu: Entity<CompletionMenu>,
        /// Room left of the editor, to put the cursor near the window's edge.
        indent: Pixels,
    }

    impl Render for MenuProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            // the menu is placed from the input's origin, as the input's
            // own overlays are
            div().size_full().pl(self.indent).child(
                div()
                    .relative()
                    .child(Editor::new(&self.state).h(px(200.)))
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
            (menu.width.expect("the menu measured its rows"), bounds)
        })
    }

    fn set_max_width(probe: &Entity<MenuProbe>, max_width: Pixels, cx: &mut VisualTestContext) {
        let state = probe.read_with(cx, |probe, _| probe.state.clone());
        cx.update(|_, cx| {
            state.update(cx, |state, _| {
                state.lsp_mut().completion_menu.max_width = max_width
            })
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

    /// Laid out below a documentation panel, the popover used to take the
    /// whole `max_width` whatever its rows.
    #[gpui::test]
    fn a_menu_laid_out_in_a_column_is_as_wide_as_its_rows(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let (_, beside) = show(&probe, wide(), cx);

        // far more than two popovers fit beside each other, so the menu
        // goes into a column
        set_max_width(&probe, px(5000.), cx);
        let (measured, column) = show(&probe, wide(), cx);

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
        let (_, floor) = show(&probe, vec![item("a", None)], cx);
        let (tiny, _) = show(&probe, vec![item("a", None)], cx);

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
        let window_width = cx.update(|window, _| window.bounds().size.width);
        let (_, away) = show(&probe, wide(), cx);
        let (_, floor) = show(&probe, narrow(), cx);

        // the editor, and the cursor with it, 60 px short of the edge
        cx.update(|window, cx| {
            probe.update(cx, |probe, cx| {
                probe.indent = window_width - px(60.);
                cx.notify();
            });
            window.draw(cx).clear(cx);
        });
        let (_, wide_at_edge) = show(&probe, wide(), cx);
        let (_, narrow_at_edge) = show(&probe, narrow(), cx);

        for (at_edge, away) in [(wide_at_edge, away), (narrow_at_edge, floor)] {
            assert!(
                at_edge.right() <= window_width,
                "{at_edge:?} in a window {window_width:?} wide"
            );
            assert_eq!(at_edge.size.width, away.size.width, "the rows are whole");
        }
        assert!(wide_at_edge.left() < window_width - px(60.));
    }

    /// The widest row is looked for among the longest by characters: a
    /// detail full of arrows is long in bytes and no wider than its letters.
    #[gpui::test]
    fn the_widest_row_is_found_by_characters_not_bytes(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let widest = item("abcdefghijklmnopqrstuvwxyz", None);
        let (alone, _) = show(&probe, vec![widest.clone(); MEASURED_ROWS + 8], cx);

        let arrows = item("x", Some("→→→→→→→→→→"));
        let mut items = vec![arrows; MEASURED_ROWS + 7];
        items.insert(MEASURED_ROWS / 2, widest);
        let (among, _) = show(&probe, items, cx);

        assert_eq!(among, alone);
    }

    /// The scrollbar is drawn over the rows' ends, so a list that scrolls is
    /// wider by its track, and one that does not is not.
    #[gpui::test]
    fn a_list_that_scrolls_keeps_its_rows_clear_of_the_scrollbar(cx: &mut TestAppContext) {
        let (probe, cx) = probe(cx);
        let row = item("total_amount_in_cents", Some("integer · orders"));

        let (short, _) = show(&probe, vec![row.clone(); 3], cx);
        let (long, _) = show(&probe, vec![row; 40], cx);

        let track = cx.update(|_, cx| {
            gpui_base::Theme::global(cx)
                .scrollbar
                .styles()
                .track_width()
        });
        assert!(track > px(0.));
        assert_eq!(long, short + track);
    }
}
