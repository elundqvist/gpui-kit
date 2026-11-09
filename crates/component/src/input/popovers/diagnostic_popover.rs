use std::rc::Rc;

use gpui::{
    App, AppContext as _, Context, Empty, Entity, IntoElement, Render, SharedString, Styled,
    WeakEntity, Window,
};

use crate::{
    highlighter::DiagnosticEntry,
    input::{
        EditorState,
        popovers::{Popover, plain_text_as_markdown, render_markdown},
    },
};

pub struct DiagnosticPopover {
    state: WeakEntity<EditorState>,
    pub(crate) diagnostic: Rc<DiagnosticEntry>,
}

impl DiagnosticPopover {
    pub fn new(
        diagnostic: &DiagnosticEntry,
        state: Entity<EditorState>,
        cx: &mut App,
    ) -> Entity<Self> {
        let diagnostic = Rc::new(diagnostic.clone());

        cx.new(|_| Self {
            diagnostic,
            state: state.downgrade(),
        })
    }
}

impl Render for DiagnosticPopover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(state) = self.state.upgrade() else {
            return Empty.into_any_element();
        };
        let message: SharedString = plain_text_as_markdown(&self.diagnostic.message).into();

        let (border, bg, fg) = (
            crate::highlighter::diagnostic_border(self.diagnostic.severity, cx),
            crate::highlighter::diagnostic_background(self.diagnostic.severity, cx),
            crate::highlighter::diagnostic_foreground(self.diagnostic.severity, cx),
        );

        Popover::new(
            "diagnostic-popover",
            state,
            self.diagnostic.range.clone(),
            move |window, cx| render_markdown("message", message.clone(), window, cx),
        )
        .problem()
        .px_1()
        .py_0p5()
        .bg(bg)
        .text_color(fg)
        .border_1()
        .border_color(border)
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use gpui::{
        AppContext as _, Context, Entity, IntoElement, Modifiers, MouseExitEvent,
        ParentElement as _, Render, Styled as _, VisualTestContext, Window, div, point, px,
    };
    use gpui_base::input::{Diagnostic, Position};

    use crate::input::{Editor, EditorState, RopeExt as _};

    /// An editor, and room beside it for the pointer to go to.
    struct Beside {
        state: Entity<EditorState>,
    }

    impl Render for Beside {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .child(
                    div()
                        .w(px(300.))
                        .child(Editor::new(&self.state).h(px(200.))),
                )
                .child(div().flex_1().h_full())
        }
    }

    /// A problem's popover goes as the pointer leaves the editor: put
    /// away with the hover's, it stayed up, as no move outside the editor
    /// reached the editor to put it away, and as it leaves the window
    /// (elundqvist/kvist#656).
    #[gpui::test]
    fn a_problems_popover_goes_as_the_pointer_leaves_the_editor(cx: &mut gpui::TestAppContext) {
        cx.update(crate::init);
        let (view, cx) = cx.add_window_view(|window, cx| Beside {
            state: cx.new(|cx| EditorState::new(window, cx).language("sql")),
        });
        let state = view.read_with(cx, |view, _| view.state.clone());
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_value("select a;\nselect b;\nselect c;", window, cx);
                let set = state.diagnostics_mut().unwrap();
                set.push(Diagnostic::new(
                    Position::new(1, 7)..Position::new(1, 8),
                    "b",
                ));
            })
        });
        let draw = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
        };
        draw(cx);
        let b = cx.update(|_, cx| {
            state.read_with(cx, |state, _| {
                let at = state.text().line_start_offset(1) + 7;
                state.range_to_bounds(&(at..at + 1)).expect("`b` is drawn")
            })
        });
        let shown = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| state.read(cx).diagnostic_popover().is_some())
        };

        cx.simulate_mouse_move(b.center(), None, Modifiers::none());
        draw(cx);
        assert!(shown(cx), "over `b`");
        cx.simulate_mouse_move(point(px(600.), b.center().y), None, Modifiers::none());
        draw(cx);
        assert!(!shown(cx), "beside the editor");

        // and as it leaves the window from over the problem, which sends
        // no move
        cx.simulate_mouse_move(b.center(), None, Modifiers::none());
        draw(cx);
        assert!(shown(cx), "over `b` again");
        cx.simulate_event(MouseExitEvent {
            position: b.center(),
            pressed_button: None,
            modifiers: Modifiers::none(),
        });
        draw(cx);
        assert!(!shown(cx), "out of the window");
    }

    /// A problem's popover stays up while the pointer goes on its way to
    /// it, across the words beside the problem on its row, and goes as the
    /// pointer turns away: it went at the first word with no problem, and
    /// its text could not be picked nor its links followed, where VS
    /// Code's hover stays while the pointer gets closer to it
    /// (elundqvist/kvist#666).
    #[gpui::test]
    fn a_problems_popover_stays_up_on_the_pointers_way_to_it(cx: &mut gpui::TestAppContext) {
        cx.update(crate::init);
        let (view, cx) = cx.add_window_view(|window, cx| Beside {
            state: cx.new(|cx| EditorState::new(window, cx).language("sql")),
        });
        let state = view.read_with(cx, |view, _| view.state.clone());
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                // the problem on the fifth line, with room for its
                // popover above it
                state.set_value(
                    "select 1;\nselect 2;\nselect 3;\nselect 4;\nselect b from u;\nselect c;",
                    window,
                    cx,
                );
                let set = state.diagnostics_mut().unwrap();
                set.push(Diagnostic::new(
                    Position::new(4, 7)..Position::new(4, 8),
                    "cannot find value `b` in this scope",
                ));
            })
        });
        let draw = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
        };
        draw(cx);
        let b = cx.update(|_, cx| {
            state.read_with(cx, |state, _| {
                let at = state.text().line_start_offset(4) + 7;
                state.range_to_bounds(&(at..at + 1)).expect("`b` is drawn")
            })
        });
        let shown = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| state.read(cx).diagnostic_popover().is_some())
        };

        cx.simulate_mouse_move(b.center(), None, Modifiers::none());
        draw(cx);
        assert!(shown(cx), "over `b`");
        // the popover hangs over the rows above, and the way up to its
        // right crosses `from u` on the problem's own row
        for step in 1..=4 {
            let at = point(
                b.right() + px(12. * step as f32),
                b.center().y - px(step as f32),
            );
            cx.simulate_mouse_move(at, None, Modifiers::none());
            draw(cx);
            assert!(shown(cx), "on the way, {step} words on");
        }
        // and away from it, down the editor
        cx.simulate_mouse_move(
            point(b.center().x, b.bottom() + px(30.)),
            None,
            Modifiers::none(),
        );
        draw(cx);
        assert!(!shown(cx), "turned away");
    }

    /// A hover for every word, as a language server has, long enough to
    /// stand over the words after it, counting the hovers asked for.
    struct Words(std::rc::Rc<std::cell::Cell<usize>>);

    impl crate::input::HoverProvider for Words {
        fn hover(
            &self,
            _: &crate::input::Rope,
            _: usize,
            _: &mut Window,
            _: &mut gpui::App,
        ) -> gpui::Task<anyhow::Result<Option<lsp_types::Hover>>> {
            self.0.set(self.0.get() + 1);
            gpui::Task::ready(Ok(Some(lsp_types::Hover {
                contents: lsp_types::HoverContents::Markup(lsp_types::MarkupContent {
                    kind: lsp_types::MarkupKind::PlainText,
                    value: "a word of the query, of which this hover says a great deal".into(),
                }),
                range: None,
            })))
        }
    }

    /// An editor with the query of the test above in it, a problem at
    /// `at` on its fifth line and a hover for every word, and the bounds
    /// of the `n` bytes at the column `col` of that line.
    fn hovered(
        cx: &mut gpui::TestAppContext,
        at: u32,
    ) -> (
        Entity<EditorState>,
        std::rc::Rc<std::cell::Cell<usize>>,
        impl Fn(&mut VisualTestContext, usize, usize) -> gpui::Bounds<gpui::Pixels>,
        &mut VisualTestContext,
    ) {
        cx.update(crate::init);
        let (view, cx) = cx.add_window_view(|window, cx| Beside {
            state: cx.new(|cx| EditorState::new(window, cx).language("sql")),
        });
        let state = view.read_with(cx, |view, _| view.state.clone());
        let asked = std::rc::Rc::new(std::cell::Cell::new(0));
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_value(
                    "select 1;\nselect 2;\nselect 3;\nselect 4;\nselect a, b from u;\nselect c;",
                    window,
                    cx,
                );
                state.lsp_mut().hover_provider = Some(std::rc::Rc::new(Words(asked.clone())));
                let set = state.diagnostics_mut().unwrap();
                set.push(Diagnostic::new(
                    Position::new(4, at)..Position::new(4, at + 1),
                    "cannot find value in this scope, which this problem says at length",
                ));
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| _ = window.draw(cx));
        let bytes = {
            let state = state.clone();
            move |cx: &mut VisualTestContext, col: usize, n: usize| {
                cx.update(|_, cx| {
                    state.read_with(cx, |state, _| {
                        let at = state.text().line_start_offset(4) + col;
                        state.range_to_bounds(&(at..at + n)).expect("drawn")
                    })
                })
            }
        };
        (state, asked, bytes, cx)
    }

    /// On its way to a problem's popover the pointer shows no hover for
    /// the words it crosses, and the hover asked for over the problem is
    /// not shown: the last word crossed opened its hover over the popover
    /// the pointer was going to (elundqvist/kvist#666).
    #[gpui::test]
    fn the_way_to_a_problems_popover_shows_no_hover(cx: &mut gpui::TestAppContext) {
        let (state, asked, bytes, cx) = hovered(cx, 7);
        let draw = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
        };
        let a = bytes(cx, 7, 1);
        cx.simulate_mouse_move(a.center(), None, Modifiers::none());
        draw(cx);
        assert!(cx.update(|_, cx| state.read(cx).diagnostic_popover().is_some()), "over `a`");
        let over_a = asked.get();
        for step in 1..=4 {
            let at = point(a.right() + px(12. * step as f32), a.center().y - px(step as f32));
            cx.simulate_mouse_move(at, None, Modifiers::none());
            draw(cx);
            assert!(cx.update(|_, cx| state.read(cx).diagnostic_popover().is_some()), "on the way, {step} words on");
        }
        assert_eq!(asked.get(), over_a, "no hover asked for the words crossed");
        cx.executor().advance_clock(std::time::Duration::from_secs(1));
        draw(cx);
        assert!(
            cx.update(|_, cx| state.read(cx).hover_popover().is_none()),
            "no hover shown over the problem's popover"
        );
    }

    /// On its way to the hover the pointer shows no problem's popover for
    /// a problem it crosses, which opened over the hover it was going to
    /// (elundqvist/kvist#666).
    #[gpui::test]
    fn the_way_to_the_hover_shows_no_problem(cx: &mut gpui::TestAppContext) {
        let (state, _, bytes, cx) = hovered(cx, 10);
        let draw = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
        };
        let a = bytes(cx, 7, 1);
        let b = bytes(cx, 10, 1);
        cx.simulate_mouse_move(a.center(), None, Modifiers::none());
        draw(cx);
        cx.executor().advance_clock(std::time::Duration::from_secs(1));
        draw(cx);
        assert!(cx.update(|_, cx| state.read(cx).hover_popover().is_some()), "the hover of `a`");
        assert!(cx.update(|_, cx| state.read(cx).diagnostic_popover().is_none()));
        // right and up, across `b` and its problem
        let steps = 4;
        for step in 1..=steps {
            let x = a.center().x + (b.center().x - a.center().x) * (step as f32 / 2.);
            let at = point(x, a.center().y - px(step as f32 * 0.5));
            cx.simulate_mouse_move(at, None, Modifiers::none());
            draw(cx);
            assert!(cx.update(|_, cx| state.read(cx).hover_popover().is_some()), "on the way, step {step}");
            assert!(
                cx.update(|_, cx| state.read(cx).diagnostic_popover().is_none()),
                "the problem of `b` crossed at step {step}"
            );
        }
    }
}
