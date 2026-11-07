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
        .dismiss_with(EditorState::clear_diagnostic_popover)
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
}
