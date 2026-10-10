use ropey::Rope;
use rust_i18n::t;

use gpui::{
    App, AppContext as _, Context, Empty, Entity, FocusHandle, Focusable, Half,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render, Styled, Subscription,
    WeakEntity, Window, actions, div, prelude::FluentBuilder as _,
};

use crate::{
    ActiveTheme, Disableable, ElementExt, IconName, Selectable, Sizable,
    button::{Button, ButtonVariants},
    h_flex,
    input::{
        Enter, Escape, IndentInline, Input, InputBaseState, InputEvent, InputState, OutdentInline,
        Replace,
    },
    label::Label,
    v_flex,
};

const CONTEXT: &'static str = "SearchPanel";

actions!(input, [Tab]);

#[cfg(test)]
use gpui_base::input::SearchMatcher;

#[derive(Clone, Copy)]
#[cfg(test)]
enum MoveDirection {
    Up,
    Down,
}

#[cfg(test)]
fn next_scroll_direction(
    previous_match_ix: usize,
    current_match_ix: usize,
) -> Option<MoveDirection> {
    if current_match_ix <= previous_match_ix {
        None
    } else {
        Some(MoveDirection::Down)
    }
}

#[cfg(test)]
fn prev_scroll_direction(
    previous_match_ix: usize,
    current_match_ix: usize,
) -> Option<MoveDirection> {
    if current_match_ix >= previous_match_ix {
        None
    } else {
        Some(MoveDirection::Up)
    }
}

pub(super) struct SearchPanel<M: crate::input::overlay::OverlayMode> {
    editor: WeakEntity<InputBaseState<M>>,
    search_input: Entity<InputState>,
    replace_input: Entity<InputState>,
    session: gpui_base::input::SearchSession,
    input_width: Pixels,

    _subscriptions: Vec<Subscription>,
}

impl<M: crate::input::overlay::OverlayMode> SearchPanel<M> {
    pub(super) fn sync_session(&mut self, session: &gpui_base::input::SearchSession) {
        self.session = session.clone();
    }

    /// The query the panel's search input currently holds.
    pub(super) fn query(&self, cx: &App) -> gpui::SharedString {
        self.search_input.read(cx).value()
    }

    pub(crate) fn new(
        editor: Entity<InputBaseState<M>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let search_input = cx.new(|cx| InputState::new(window, cx));
        let replace_input = cx.new(|cx| InputState::new(window, cx));

        cx.new(|cx| {
            let _subscriptions =
                vec![
                    cx.subscribe(&search_input, |this: &mut Self, _, ev: &InputEvent, cx| {
                        // Handle search input changes
                        match ev {
                            InputEvent::Change => {
                                this.update_search_query(cx);
                            }
                            _ => {}
                        }
                    }),
                ];

            Self {
                editor: editor.downgrade(),
                search_input,
                replace_input,
                session: gpui_base::input::SearchSession::default(),
                input_width: Pixels::ZERO,
                _subscriptions,
            }
        })
    }

    pub(super) fn show_with_focus(
        &mut self,
        selected_text: &Rope,
        replace_mode: bool,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session.open = true;
        self.session.replace_mode = replace_mode;
        if focus {
            self.search_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
        }

        self.search_input.update(cx, |this, cx| {
            if selected_text.len() > 0 {
                this.set_value(selected_text.to_string(), window, cx);
            }
            this.select_all(window, cx);
        });

        // The query the bar opens on, the selection or the one it last
        // held, is the editor's already, counted from where the search
        // begins and with the view left where it is: opening the bar moves
        // nothing, as VS Code's does not, and only what is typed is
        // followed. `set_value` emits no `InputEvent::Change`, so an input
        // that holds something else is handed over here, as typed.
        if self.search_input.read(cx).value() != self.session.query {
            self.update_search_query(cx);
        }
    }

    /// Hand the editor the search input's value as the query, which it
    /// follows to its first match from where the search began, and take
    /// its matches for the bar's count.
    fn update_search_query(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value();
        let editor = self.editor.clone();
        let _ = editor.update(cx, |state, cx| {
            state.set_search_query(query.clone(), self.session.case_insensitive, cx);
        });
        if let Ok(session) = editor.read_with(cx, |state, _| state.search_session().clone()) {
            self.session = session;
        }
        cx.notify();
    }

    fn replaceable(&self, cx: &App) -> bool {
        self.editor
            .read_with(cx, |editor, _| editor.is_replaceable())
            .unwrap_or(false)
    }

    pub(super) fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.hide_with_focus(true, window, cx);
    }

    pub(super) fn hide_with_focus(
        &mut self,
        focus_editor: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session.open = false;
        let _ = self.editor.update(cx, |state, cx| state.close_search(cx));
        if focus_editor {
            if let Some(editor) = self.editor.upgrade() {
                editor.read(cx).focus_handle(cx).focus(window, cx);
            }
        }
        cx.notify();
    }

    fn on_action_enter(&mut self, action: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        if action.shift {
            self.prev(window, cx);
        } else {
            self.next(window, cx);
        }
    }

    fn on_action_escape(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        self.hide(window, cx);
    }

    fn on_action_tab(&mut self, _: &IndentInline, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_focus(window, cx);
    }

    fn on_action_tab_prev(
        &mut self,
        _: &OutdentInline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_focus(window, cx);
    }

    /// Cycle focus between the search and the replace input, to keep the Tab key
    /// staying in the panel.
    ///
    /// There are only 2 inputs, so the forward and the backward are the same.
    fn cycle_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.session.replace_mode || !self.replaceable(cx) {
            return;
        }

        let search_focus_handle = self.search_input.read(cx).focus_handle(cx);
        let focus_handle = if search_focus_handle.is_focused(window) {
            self.replace_input.read(cx).focus_handle(cx)
        } else {
            search_focus_handle
        };
        focus_handle.focus(window, cx);
    }

    fn on_action_replace(&mut self, _: &Replace, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_replace_mode(window, cx);
    }

    /// Toggle the replace field, and move focus to the field that is going to be used.
    fn toggle_replace_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.replaceable(cx) {
            return;
        }

        self.session.replace_mode = !self.session.replace_mode;
        let replace_mode = self.session.replace_mode;
        let _ = self.editor.update(cx, |state, cx| {
            state.set_search_replace_mode(replace_mode, cx);
        });
        let focus_handle = if self.session.replace_mode {
            self.replace_input.read(cx).focus_handle(cx)
        } else {
            self.search_input.read(cx).focus_handle(cx)
        };
        focus_handle.focus(window, cx);
        cx.notify();
    }

    fn prev(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let _ = self.editor.update(cx, |state, cx| {
            _ = state.previous_search_match(cx);
        });
    }

    fn next(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let _ = self.editor.update(cx, |state, cx| {
            _ = state.next_search_match(cx);
        });
    }

    fn replace_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.replaceable(cx) {
            self.session.replace_mode = false;
            cx.notify();
            return;
        }

        let replacement = self.replace_input.read(cx).value();
        let _ = self.editor.update(cx, |state, cx| {
            _ = state.replace_current_search_match(&replacement, window, cx);
        });
    }

    fn replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.replaceable(cx) {
            self.session.replace_mode = false;
            cx.notify();
            return;
        }

        let replacement = self.replace_input.read(cx).value();
        let _ = self.editor.update(cx, |state, cx| {
            _ = state.replace_all_search_matches(&replacement, window, cx);
        });
    }
}

impl<M: crate::input::overlay::OverlayMode> Focusable for SearchPanel<M> {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search_input.read(cx).focus_handle(cx)
    }
}

impl<M: crate::input::overlay::OverlayMode> Render for SearchPanel<M> {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.session.open {
            return Empty.into_any_element();
        }

        let has_matches = !self.session.matcher.is_empty();
        let allow_replace = self.replaceable(cx);
        if !allow_replace {
            self.session.replace_mode = false;
        }

        v_flex()
            .id("search-panel")
            .occlude()
            .track_focus(&self.focus_handle(cx))
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::on_action_enter))
            .on_action(cx.listener(Self::on_action_escape))
            .on_action(cx.listener(Self::on_action_tab))
            .on_action(cx.listener(Self::on_action_tab_prev))
            .on_action(cx.listener(Self::on_action_replace))
            .font_family(cx.theme().font_family.clone())
            .items_center()
            .py_2()
            .px_3()
            .w_full()
            .gap_1()
            .bg(cx.theme().tokens.popover)
            .border_b_1()
            .rounded(cx.theme().radius.half())
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .gap_1()
                            .child(
                                Input::new(&self.search_input)
                                    .focus_bordered(false)
                                    .suffix(
                                        Button::new("case-insensitive")
                                            .selected(!self.session.case_insensitive)
                                            .toggled(!self.session.case_insensitive)
                                            .xsmall()
                                            .compact()
                                            .text()
                                            .icon(IconName::CaseSensitive)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.session.case_insensitive =
                                                    !this.session.case_insensitive;
                                                this.update_search_query(cx);
                                                cx.notify();
                                            })),
                                    )
                                    .small()
                                    .w_full()
                                    .shadow_none(),
                            )
                            .on_prepaint({
                                let view = cx.entity();
                                move |bounds, _, cx| {
                                    view.update(cx, |r, _| r.input_width = bounds.size.width)
                                }
                            }),
                    )
                    .when(allow_replace, |this| {
                        this.child(
                            Button::new("replace-mode")
                                .xsmall()
                                .ghost()
                                .icon(IconName::Replace)
                                .selected(self.session.replace_mode)
                                .toggled(self.session.replace_mode)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_replace_mode(window, cx);
                                })),
                        )
                    })
                    .child(
                        Button::new("prev")
                            .xsmall()
                            .ghost()
                            .icon(IconName::ChevronLeft)
                            .disabled(!has_matches)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.prev(window, cx);
                            })),
                    )
                    .child(
                        Button::new("next")
                            .xsmall()
                            .ghost()
                            .icon(IconName::ChevronRight)
                            .disabled(!has_matches)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.next(window, cx);
                            })),
                    )
                    .child(
                        Label::new(self.session.matcher.label())
                            .when(!has_matches, |this| {
                                this.text_color(cx.theme().muted_foreground)
                            })
                            .text_left()
                            .min_w_16(),
                    )
                    .child(div().w_7())
                    .child(
                        Button::new("close")
                            .xsmall()
                            .ghost()
                            .icon(IconName::Close)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_action_escape(&Escape, window, cx);
                            })),
                    ),
            )
            .when(self.session.replace_mode && allow_replace, |this| {
                this.child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(
                            Input::new(&self.replace_input)
                                .focus_bordered(false)
                                .small()
                                .w(self.input_width)
                                .shadow_none(),
                        )
                        .child(
                            Button::new("replace-one")
                                .small()
                                .label(t!("Input.Replace"))
                                .disabled(!has_matches)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.replace_next(window, cx);
                                })),
                        )
                        .child(
                            Button::new("replace-all")
                                .small()
                                .label(t!("Input.Replace All"))
                                .disabled(!has_matches)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.replace_all(window, cx);
                                })),
                        ),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    /// 400 lines in an editor, "needle N" on lines 20, 250 and 330, the
    /// caret on line 150 and the view on it.
    struct Probe {
        editor: Entity<crate::input::EditorState>,
    }

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(crate::input::Editor::new(&self.editor).h_full())
        }
    }

    /// What the editor's find bar holds: its input's value and its count.
    fn bar(
        editor: &Entity<crate::input::EditorState>,
        cx: &mut gpui::VisualTestContext,
    ) -> (String, String) {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|_, cx| {
            let panel =
                crate::input::overlay::search_panel_of(editor, cx).expect("the bar is open");
            let panel = panel.read(cx);
            (
                panel.search_input.read(cx).value().to_string(),
                panel.session.matcher.label(),
            )
        })
    }

    /// The line the editor's current match is on, which is in view.
    fn current_line(
        editor: &Entity<crate::input::EditorState>,
        cx: &mut gpui::VisualTestContext,
    ) -> usize {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|_, cx| {
            use gpui_base::input::RopeExt as _;
            let state = editor.read(cx);
            let matcher = &state.search_session().matcher;
            let range = matcher.matched_ranges()[matcher.current_match_index()].clone();
            let line = state.text().offset_to_point(range.start).row;
            let rows = state.visible_row_range().expect("laid out");
            assert!(rows.contains(&line), "line {line} is not in view, {rows:?}");
            line
        })
    }

    /// Typed into the find bar itself, a character at a time, the query is
    /// followed to its first match from the caret, the bar counting from
    /// it, and Enter goes on to the next; typed again with the view where
    /// Enter left it, it is followed back up. Opened again on the query it
    /// held, the bar moves nothing. The bar handed the editor each change
    /// and the editor counted from the first match in the text and stayed
    /// where it was until Enter, which went to the second
    /// (elundqvist/kvist#208).
    #[gpui::test]
    fn the_find_bar_follows_what_is_typed_into_it(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use gpui_base::input::Position;

        cx.update(crate::init);
        let mut editor = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|cx| crate::input::EditorState::new(window, cx));
            editor = Some(state.clone());
            Probe { editor: state }
        });
        let editor = editor.unwrap();
        let text: String = (0..400)
            .map(|i| {
                if [20, 250, 330].contains(&i) {
                    format!("needle {i}\n")
                } else {
                    format!("line {i}\n")
                }
            })
            .collect();
        cx.update(|window, cx| {
            editor.update(cx, |state, cx| {
                state.set_value(text, window, cx);
                state.set_cursor_position(Position::new(150, 0), window, cx);
            });
            window.draw(cx).clear(cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            editor.update(cx, |state, cx| state.open_search(false, cx));
            window.draw(cx).clear(cx);
        });
        assert_eq!(bar(&editor, cx), (String::new(), "0/0".to_string()));
        // the bar takes the keys, as it does when it opens outside a test
        cx.update(|window, cx| {
            let panel = crate::input::overlay::search_panel_of(&editor, cx).unwrap();
            let input = panel.read(cx).search_input.clone();
            input.read(cx).focus_handle(cx).focus(window, cx);
            window.draw(cx).clear(cx);
        });
        cx.simulate_input("needle");
        assert_eq!(bar(&editor, cx), ("needle".to_string(), "2/3".to_string()));
        assert_eq!(
            current_line(&editor, cx),
            250,
            "typed, the first from the caret"
        );
        cx.simulate_keystrokes("enter");
        assert_eq!(bar(&editor, cx).1, "3/3");
        assert_eq!(current_line(&editor, cx), 330, "Enter, on to the next");
        // the query typed over, with the view down where Enter left it
        cx.update(|window, cx| {
            let panel = crate::input::overlay::search_panel_of(&editor, cx).unwrap();
            let input = panel.read(cx).search_input.clone();
            input.update(cx, |input, cx| input.select_all(window, cx));
        });
        cx.simulate_input("line 15");
        // "line 15" and "line 150" to "line 159": the caret is on 150
        assert_eq!(
            bar(&editor, cx),
            ("line 15".to_string(), "2/11".to_string())
        );
        assert_eq!(current_line(&editor, cx), 150, "typed again, back up to it");

        // closed and opened again on what it held: nothing moves
        cx.simulate_input("9");
        assert_eq!(current_line(&editor, cx), 159);
        cx.update(|window, cx| {
            let panel = crate::input::overlay::search_panel_of(&editor, cx).unwrap();
            panel.update(cx, |panel, cx| panel.hide_with_focus(false, window, cx));
            editor.update(cx, |state, cx| {
                state.set_cursor_position(Position::new(20, 0), window, cx)
            });
            window.draw(cx).clear(cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let was = cx.update(|_, cx| editor.read(cx).scroll_offset());
        cx.update(|window, cx| {
            editor.update(cx, |state, cx| state.open_search(false, cx));
            window.draw(cx).clear(cx);
        });
        assert_eq!(
            bar(&editor, cx),
            ("line 159".to_string(), "1/1".to_string())
        );
        assert_eq!(
            cx.update(|_, cx| editor.read(cx).scroll_offset()),
            was,
            "the bar opened on its last query and the view stayed"
        );
        // and the first Enter goes to that match, which is out of sight
        cx.update(|window, cx| {
            let panel = crate::input::overlay::search_panel_of(&editor, cx).unwrap();
            let input = panel.read(cx).search_input.clone();
            input.read(cx).focus_handle(cx).focus(window, cx);
            window.draw(cx).clear(cx);
        });
        cx.simulate_keystrokes("enter");
        assert_eq!(bar(&editor, cx).1, "1/1");
        assert_eq!(current_line(&editor, cx), 159);
    }

    #[test]
    fn test_search() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("Hello 世界 this is a Is test string."));
        matcher.update_query("Is", true);

        assert_eq!(matcher.len(), 3);
        let mut matches = matcher.clone();
        assert_eq!(matches.current_match_index(), 0);
        assert_eq!(matches.next(), Some(18..20));
        assert_eq!(matches.next(), Some(23..25));
        assert_eq!(matches.current_match_index(), 2);
        assert_eq!(matches.next(), Some(15..17));
        assert_eq!(matches.current_match_index(), 0);
        assert_eq!(matches.next_back(), Some(23..25));
        assert_eq!(matches.current_match_index(), 2);
        assert_eq!(matches.next_back(), Some(18..20));
        assert_eq!(matches.current_match_index(), 1);
        assert_eq!(matches.next_back(), Some(15..17));
        assert_eq!(matches.current_match_index(), 0);
        assert_eq!(matches.next_back(), Some(23..25));

        matcher.update_query("IS", false);
        assert_eq!(matcher.len(), 0);
        assert_eq!(matcher.next(), None);
        assert_eq!(matcher.next_back(), None);
    }

    #[test]
    fn test_search_label() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("Hello 世界 this is a Is test string."));
        matcher.update_query("Is", true);
        assert_eq!(matcher.label(), "1/3");
        matcher.next();
        assert_eq!(matcher.label(), "2/3");
        matcher.next();
        assert_eq!(matcher.label(), "3/3");
        matcher.next();
        assert_eq!(matcher.label(), "1/3");

        matcher.update_query("IS", false);
        assert_eq!(matcher.label(), "0/0");
    }

    #[test]
    fn test_select_range_start() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from(".....aaaaa.....aaaaa.....aaaaa"));
        matcher.update_query("aaaaa", false);
        matcher.update_cursor_by_offset(0);
        assert_eq!(matcher.current_match_index(), 0);

        matcher.update_cursor_by_offset(5);
        assert_eq!(matcher.current_match_index(), 0);

        matcher.update_cursor_by_offset(12);
        assert_eq!(matcher.current_match_index(), 1);

        matcher.update_cursor_by_offset(16);
        assert_eq!(matcher.current_match_index(), 1);

        matcher.update_cursor_by_offset(30);
        assert_eq!(matcher.current_match_index(), 2);

        matcher.update_cursor_by_offset(31);
        assert_eq!(matcher.current_match_index(), 2);
    }

    #[test]
    fn test_next_scroll_direction_returns_down_without_wrap() {
        assert!(matches!(
            next_scroll_direction(0, 1),
            Some(MoveDirection::Down)
        ));
    }

    #[test]
    fn test_next_scroll_direction_returns_none_on_wrap() {
        assert!(next_scroll_direction(2, 0).is_none());
    }

    #[test]
    fn test_next_scroll_direction_returns_none_for_single_match() {
        assert!(next_scroll_direction(0, 0).is_none());
    }

    #[test]
    fn test_prev_scroll_direction_returns_up_without_wrap() {
        assert!(matches!(
            prev_scroll_direction(2, 1),
            Some(MoveDirection::Up)
        ));
    }

    #[test]
    fn test_prev_scroll_direction_returns_none_on_wrap() {
        assert!(prev_scroll_direction(0, 2).is_none());
    }

    #[test]
    fn test_prev_scroll_direction_returns_none_for_single_match() {
        assert!(prev_scroll_direction(0, 0).is_none());
    }
}
