use gpui::{
    Anchor, AnyElement, App, Bounds, Div, ElementId, InteractiveElement, Interactivity,
    IntoElement, ParentElement, Pixels, Point, RenderOnce, Size, StatefulInteractiveElement,
    StyleRefinement, Styled, Window, deferred, div, px,
};

use crate::{Align, ElementExt as _, Placement, Positioner, StyledExt as _};

/// Distance kept between a popup and the window edge.
const WINDOW_MARGIN: Pixels = px(8.);

/// Deferred paint priority for interactive surfaces that must appear above dialogs.
pub const POPUP_PRIORITY: usize = 100;

#[derive(Default)]
struct PopupAnchorState {
    bounds: Bounds<Pixels>,
    captured: bool,
}

/// An unstyled trigger and anchored popup host.
///
/// `Popup` owns trigger measurement, anchor-point calculation, first-frame
/// synchronization, deferred rendering, and window-edge snapping. Callers own
/// open state, interaction, popup content, appearance, and motion.
#[derive(IntoElement)]
pub struct Popup {
    id: ElementId,
    base: gpui::Stateful<Div>,
    style: StyleRefinement,
    anchor: Anchor,
    offset: Pixels,
    trigger: AnyElement,
    content: Option<AnyElement>,
}

impl Popup {
    pub fn new(id: impl Into<ElementId>, trigger: impl IntoElement) -> Self {
        let id = id.into();
        Self {
            base: div().id(id.clone()),
            id,
            style: StyleRefinement::default(),
            anchor: Anchor::TopLeft,
            offset: px(0.),
            trigger: trigger.into_any_element(),
            content: None,
        }
    }

    pub fn anchor(mut self, anchor: impl Into<Anchor>) -> Self {
        self.anchor = anchor.into();
        self
    }

    /// The gap kept between the trigger and the popup, on whichever side
    /// of it the popup goes.
    pub fn offset(mut self, offset: Pixels) -> Self {
        self.offset = offset;
        self
    }

    pub fn content(mut self, content: impl IntoElement) -> Self {
        self.content = Some(content.into_any_element());
        self
    }

    /// The point of the trigger at `anchor`'s corner of a popup hung from
    /// it, a top anchor's below the trigger and a bottom one's above, its
    /// gap and its flip left out: a popup with a left or right centre
    /// anchor is hung from it, one with a top or bottom anchor goes where
    /// [`Self::placed`] says.
    pub fn resolved_corner(anchor: Anchor, trigger_bounds: Bounds<Pixels>) -> Point<Pixels> {
        match anchor {
            Anchor::TopLeft | Anchor::LeftCenter | Anchor::RightCenter => {
                trigger_bounds.bottom_left()
            }
            Anchor::TopCenter => trigger_bounds.bottom_center(),
            Anchor::TopRight => trigger_bounds.bottom_right(),
            Anchor::BottomLeft => trigger_bounds.origin,
            Anchor::BottomCenter => trigger_bounds.top_center(),
            Anchor::BottomRight => trigger_bounds.top_right(),
        }
    }

    /// Where the popup goes: on the side of the trigger its anchor names,
    /// below it for a top anchor and above for a bottom one, and on the
    /// other where it has no room there and that side has, as a menu
    /// does; held to the trigger's edge or centre as the anchor says.
    /// Hung from the corner it covered its own trigger in a short window.
    fn positioner(anchor: Anchor, trigger_bounds: Bounds<Pixels>, offset: Pixels) -> Positioner {
        let (placement, align) = match anchor {
            Anchor::TopLeft => (Placement::Bottom, Align::Start),
            Anchor::TopCenter => (Placement::Bottom, Align::Center),
            Anchor::TopRight => (Placement::Bottom, Align::End),
            Anchor::BottomLeft => (Placement::Top, Align::Start),
            Anchor::BottomCenter => (Placement::Top, Align::Center),
            Anchor::BottomRight => (Placement::Top, Align::End),
            Anchor::LeftCenter | Anchor::RightCenter => {
                return Positioner::corner(anchor, Self::resolved_corner(anchor, trigger_bounds))
                    .margin(WINDOW_MARGIN);
            }
        };
        Positioner::side(trigger_bounds)
            .placement(placement)
            .align(align)
            .offset(offset)
            .margin(WINDOW_MARGIN)
    }

    /// Where a popup of `popup_size` opened on `trigger_bounds` with
    /// `anchor` and a gap of `offset` goes in a window of `viewport_size`,
    /// as a `Popup` puts it: for a check of where a popup opens without
    /// drawing one.
    pub fn placed(
        anchor: Anchor,
        trigger_bounds: Bounds<Pixels>,
        offset: Pixels,
        popup_size: Size<Pixels>,
        viewport_size: Size<Pixels>,
    ) -> Bounds<Pixels> {
        Self::positioner(anchor, trigger_bounds, offset)
            .resolved(popup_size, viewport_size)
            .bounds
    }
}

impl Styled for Popup {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl InteractiveElement for Popup {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl StatefulInteractiveElement for Popup {}

impl RenderOnce for Popup {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state =
            window.use_keyed_state((self.id, "anchor"), cx, |_, _| PopupAnchorState::default());
        let anchor = self.anchor;
        let root = self
            .base
            .child(self.trigger)
            .on_prepaint({
                let state = state.clone();
                move |bounds, window, cx| {
                    let first = state.update(cx, |state, _| {
                        let first = !state.captured;
                        state.bounds = bounds;
                        state.captured = true;
                        first
                    });
                    if first {
                        window.request_animation_frame();
                    }
                }
            })
            .refine_style(&self.style);

        let Some(content) = self.content else {
            return root;
        };
        if !state.read(cx).captured {
            return root;
        }

        root.child(
            deferred(
                Self::positioner(anchor, state.read(cx).bounds, self.offset)
                    // The host blocks the mouse, so no caller has to remember:
                    // what a popup covers belongs to the popup.
                    .occlude()
                    .child(content),
            )
            .with_priority(POPUP_PRIORITY),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Render, prelude::FluentBuilder as _, px};
    use std::{cell::Cell, rc::Rc};

    #[test]
    fn a_top_anchor_hangs_below_the_trigger_and_a_bottom_one_stands_above() {
        let bounds = Bounds {
            origin: Point::new(px(100.), px(100.)),
            size: gpui::Size::new(px(200.), px(50.)),
        };
        assert_eq!(
            Popup::resolved_corner(Anchor::TopCenter, bounds),
            Point::new(px(200.), px(150.))
        );
        assert_eq!(
            Popup::resolved_corner(Anchor::BottomRight, bounds),
            Point::new(px(300.), px(100.))
        );
    }

    struct Harness;

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            Popup::new(
                "popup",
                div()
                    .debug_selector(|| "popup-trigger".into())
                    .size(px(100.)),
            )
            .content(
                div()
                    .debug_selector(|| "popup-content".into())
                    .size(px(20.)),
            )
        }
    }

    /// A caller that styles its own surface — a hover card, a dropdown — does
    /// not have to remember to block the mouse. The host does it, so the panel
    /// a popup covers stops reacting to a pointer that is over the popup.
    struct OcclusionHarness {
        background_hovered: Rc<Cell<bool>>,
        content_hovered: Rc<Cell<bool>>,
    }

    impl Render for OcclusionHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let background = self.background_hovered.clone();
            let content = self.content_hovered.clone();
            div()
                .relative()
                .size(px(200.))
                .child(
                    div()
                        .id("background")
                        .absolute()
                        .size_full()
                        .on_mouse_move(move |_, _, _| background.set(true)),
                )
                .child(
                    Popup::new("popup", div().size(px(100.))).content(
                        div()
                            .id("content")
                            .size(px(40.))
                            .on_mouse_move(move |_, _, _| content.set(true)),
                    ),
                )
        }
    }

    #[gpui::test]
    fn the_popup_surface_blocks_the_panel_it_covers(cx: &mut gpui::TestAppContext) {
        let background_hovered = Rc::new(Cell::new(false));
        let content_hovered = Rc::new(Cell::new(false));
        let (_, window) = cx.add_window_view({
            let background_hovered = background_hovered.clone();
            let content_hovered = content_hovered.clone();
            move |_, _| OcclusionHarness {
                background_hovered,
                content_hovered,
            }
        });
        window.update(|window, cx| window.draw(cx).clear(cx));
        window.update(|window, cx| window.draw(cx).clear(cx));

        window.simulate_mouse_move(
            gpui::point(px(20.), px(110.)),
            None,
            gpui::Modifiers::default(),
        );
        assert!(!background_hovered.get());
        // The surface blocks what is behind it, not its own content: the
        // hitbox goes in ahead of the children, never over them.
        assert!(content_hovered.get());

        // The same pointer outside the surface still reaches the panel, so the
        // assertion above is about occlusion and not a dead listener.
        window.simulate_mouse_move(
            gpui::point(px(150.), px(180.)),
            None,
            gpui::Modifiers::default(),
        );
        assert!(background_hovered.get());
    }

    #[gpui::test]
    fn trigger_capture_enables_deferred_content_on_the_next_frame(cx: &mut gpui::TestAppContext) {
        let (_, window) = cx.add_window_view(|_, _| Harness);
        window.update(|window, cx| window.draw(cx).clear(cx));
        window.update(|window, cx| window.draw(cx).clear(cx));

        assert_eq!(
            window.debug_bounds("popup-trigger").unwrap().size,
            gpui::Size::new(px(100.), px(100.))
        );
        assert_eq!(
            window.debug_bounds("popup-content").unwrap().size,
            gpui::Size::new(px(20.), px(20.))
        );
    }

    /// A popup with no room below its trigger, at the window's foot, goes
    /// above it, its gap kept on that side; with room it hangs below. Hung
    /// from the corner, it was held into the window over its own trigger,
    /// and a menu at a short window's foot covered the row it was opened
    /// from (elundqvist/kvist#640).
    struct FlipHarness {
        at_foot: bool,
    }

    impl Render for FlipHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .p(px(20.))
                .flex()
                .flex_col()
                .when(self.at_foot, |this| this.justify_end())
                .child(
                    Popup::new(
                        "popup",
                        div()
                            .debug_selector(|| "flip-trigger".into())
                            .size(px(100.)),
                    )
                    .offset(px(4.))
                    .content(div().debug_selector(|| "flip-content".into()).size(px(40.))),
                )
        }
    }

    #[gpui::test]
    fn a_popup_with_no_room_below_its_trigger_goes_above_it(cx: &mut gpui::TestAppContext) {
        for at_foot in [false, true] {
            let (_, window) = cx.add_window_view(move |_, _| FlipHarness { at_foot });
            window.update(|window, cx| window.draw(cx).clear(cx));
            window.update(|window, cx| window.draw(cx).clear(cx));

            let trigger = window.debug_bounds("flip-trigger").unwrap();
            let content = window.debug_bounds("flip-content").unwrap();
            assert_eq!(content.left(), trigger.left());
            if at_foot {
                assert_eq!(content.bottom(), trigger.top() - px(4.));
            } else {
                assert_eq!(content.top(), trigger.bottom() + px(4.));
            }
            // and where `placed` says it goes
            let viewport = window.update(|window, _| window.viewport_size());
            assert_eq!(
                Popup::placed(Anchor::TopLeft, trigger, px(4.), content.size, viewport),
                content
            );
        }
    }
}
