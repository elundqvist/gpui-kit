use std::rc::Rc;

use gpui::{
    Anchor, App, Context, DismissEvent, ElementId, Entity, Focusable, InteractiveElement,
    IntoElement, RenderOnce, SharedString, StyleRefinement, Styled, Window, prelude::FluentBuilder,
};

use crate::{Selectable, button::Button, menu::PopupMenu, popover::Popover};

/// A dropdown menu trait for buttons and other interactive elements
pub trait DropdownMenu: Styled + Selectable + InteractiveElement + IntoElement + 'static {
    /// Create a dropdown menu with the given items, anchored to the TopLeft corner
    fn dropdown_menu(
        self,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> DropdownMenuPopover<Self> {
        self.dropdown_menu_with_anchor(Anchor::TopLeft, f)
    }

    /// Create a dropdown menu with the given items, anchored to the given corner
    fn dropdown_menu_with_anchor(
        mut self,
        anchor: impl Into<Anchor>,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> DropdownMenuPopover<Self> {
        let style = self.style().clone();
        let id = self.interactivity().element_id.clone();

        DropdownMenuPopover::new(id.unwrap_or(0.into()), anchor, self, f).trigger_style(style)
    }
}

impl DropdownMenu for Button {}

#[derive(IntoElement)]
pub struct DropdownMenuPopover<T: Selectable + IntoElement + 'static> {
    id: ElementId,
    style: StyleRefinement,
    anchor: Anchor,
    trigger: T,
    builder: Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>,
    on_open_change: Option<Rc<dyn Fn(&bool, &mut Window, &mut App)>>,
}

impl<T> DropdownMenuPopover<T>
where
    T: Selectable + IntoElement + 'static,
{
    fn new(
        id: ElementId,
        anchor: impl Into<Anchor>,
        trigger: T,
        builder: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> Self {
        Self {
            id: SharedString::from(format!("dropdown-menu:{:?}", id)).into(),
            style: StyleRefinement::default(),
            anchor: anchor.into(),
            trigger,
            builder: Rc::new(builder),
            on_open_change: None,
        }
    }

    /// Set the anchor corner for the dropdown menu popover.
    pub fn anchor(mut self, anchor: impl Into<Anchor>) -> Self {
        self.anchor = anchor.into();
        self
    }

    /// Set the style refinement for the dropdown menu trigger.
    fn trigger_style(mut self, style: StyleRefinement) -> Self {
        self.style = style;
        self
    }

    /// Add a callback to be called when the menu opens or closes.
    ///
    /// The `&bool` parameter is the **new open state**.
    pub fn on_open_change(
        mut self,
        callback: impl Fn(&bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_open_change = Some(Rc::new(callback));
        self
    }
}

#[derive(Default)]
struct DropdownMenuState {
    menu: Option<Entity<PopupMenu>>,
}

impl<T> RenderOnce for DropdownMenuPopover<T>
where
    T: Selectable + IntoElement + 'static,
{
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let builder = self.builder.clone();
        let menu_state =
            window.use_keyed_state(self.id.clone(), cx, |_, _| DropdownMenuState::default());

        Popover::new(SharedString::from(format!("popover:{}", self.id)))
            .appearance(false)
            .overlay_closable(false)
            .trigger(self.trigger)
            .trigger_style(self.style)
            .anchor(self.anchor)
            .when_some(self.on_open_change, |this, callback| {
                this.on_open_change(move |open, window, cx| callback(open, window, cx))
            })
            .content(move |_, window, cx| {
                // Here is special logic to only create the PopupMenu once and reuse it.
                // Because this `content` will called in every time render, so we need to store the menu
                // in state to avoid recreating at every render.
                //
                // And we also need to rebuild the menu when it is dismissed, to rebuild menu items
                // dynamically for support `dropdown_menu` method, so we listen for DismissEvent below.
                let menu = match menu_state.read(cx).menu.clone() {
                    Some(menu) => menu,
                    None => {
                        let builder = builder.clone();
                        let menu = PopupMenu::build(window, cx, move |menu, window, cx| {
                            builder(menu, window, cx)
                        });
                        menu_state.update(cx, |state, _| {
                            state.menu = Some(menu.clone());
                        });
                        menu.focus_handle(cx).focus(window, cx);

                        // Listen for dismiss events from the PopupMenu to close the popover.
                        // Both states weakly: held, they held the menu, whose
                        // listener this is, and a menu whose trigger went
                        // while it was open, a drawer closed on it, was
                        // never let go (elundqvist/kvist#354).
                        let popover_state = cx.entity().downgrade();
                        window
                            .subscribe(&menu, cx, {
                                let menu_state = menu_state.downgrade();
                                move |_, _: &DismissEvent, window, cx| {
                                    _ = popover_state.update(cx, |state, cx| {
                                        state.dismiss(window, cx);
                                    });
                                    _ = menu_state.update(cx, |state, _| {
                                        state.menu = None;
                                    });
                                }
                            })
                            .detach();

                        menu.clone()
                    }
                };

                menu.clone()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::PopupMenuItem;
    use gpui::{ParentElement as _, Render, WeakEntity, div, point, px};
    use std::cell::RefCell;

    /// A button with a menu, there while `shown`, and the menu it last
    /// built.
    struct Shown {
        shown: bool,
        menu: Rc<RefCell<Option<WeakEntity<PopupMenu>>>>,
    }

    impl Render for Shown {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let menu = self.menu.clone();
            div().size_full().when(self.shown, |this| {
                this.child(Button::new("trigger").label("Open").dropdown_menu(
                    move |popup, _, cx| {
                        *menu.borrow_mut() = Some(cx.entity().downgrade());
                        popup.item(PopupMenuItem::new("Restart"))
                    },
                ))
            })
        }
    }

    /// A menu whose button goes while it is open, a drawer closed on it,
    /// is let go with the button's state, and its popover is shut: the
    /// listeners for its dismissal held the states that held the menu
    /// and themselves, so neither went, and the application went on
    /// taking a popup to be open (elundqvist/kvist#354).
    #[gpui::test]
    fn a_menu_whose_button_goes_while_it_is_open_is_let_go(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_base::GlobalState::init(cx);
            cx.set_global(crate::Theme::default());
            crate::init(cx);
        });
        let menu: Rc<RefCell<Option<WeakEntity<PopupMenu>>>> = Rc::default();
        let (view, cx) = cx.add_window_view({
            let menu = menu.clone();
            move |_, _| Shown { shown: true, menu }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_click(point(px(10.), px(10.)), Default::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let open = menu.borrow().clone().expect("the menu, built as it opened");
        assert!(open.upgrade().is_some());
        cx.update(|_, cx| assert!(gpui_base::GlobalState::is_in_deferred_context(cx)));

        view.update(cx, |view, cx| {
            view.shown = false;
            cx.notify();
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        assert!(open.upgrade().is_none(), "the menu was kept");
        cx.update(|_, cx| {
            assert!(
                !gpui_base::GlobalState::is_in_deferred_context(cx),
                "a popup was taken to be open"
            )
        });
    }
}
