//! Fallback popup menu for platforms without an OS-native popup (e.g. Linux).
//!
//! It renders gpui-component's drawn [`PopupMenu`] through an overlay held by
//! [`Root`]. Unlike a real native menu it is clipped to the window, but it keeps
//! the [`super::NativeMenu`] API working on every platform.

use gpui::{
    App, Context, DismissEvent, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Pixels,
    Point, Render, Subscription, Window, anchored, deferred, div, px,
};

use crate::menu::{PopupMenu, PopupMenuItem};
use crate::root::Root;

use super::NativeMenuItem;

/// Overlay held by [`Root`] that renders the active fallback popup menu, if any.
pub(crate) struct FallbackMenuOverlay {
    active: Option<ActiveMenu>,
}

struct ActiveMenu {
    menu: Entity<PopupMenu>,
    position: Point<Pixels>,
    /// What the menu's shower is told once it is gone
    /// (`NativeMenu::show_until_closed`).
    closed: Option<Closed>,
    _subscription: Subscription,
}

pub(super) type Closed = Box<dyn FnOnce(&mut App)>;

impl FallbackMenuOverlay {
    pub(crate) fn new() -> Self {
        Self { active: None }
    }

    /// Build a [`PopupMenu`] from `items` and show it anchored at `position`.
    ///
    /// `action_context` is the focus handle the selected action is dispatched
    /// to, matching the native backends (which dispatch to the window's focus).
    #[allow(dead_code)] // Only used where the native menu falls back (e.g. Linux).
    fn show(
        &mut self,
        items: Vec<NativeMenuItem>,
        position: Point<Pixels>,
        action_context: Option<FocusHandle>,
        closed: Closed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close(cx);
        let menu = build_popup(items, action_context, window, cx);

        let subscription = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.close(cx);
        });
        menu.focus_handle(cx).focus(window, cx);

        self.active = Some(ActiveMenu {
            menu,
            position,
            closed: Some(closed),
            _subscription: subscription,
        });
        cx.notify();
    }

    /// The active menu, if any, is gone, and its shower is told.
    fn close(&mut self, cx: &mut Context<Self>) {
        if let Some(mut active) = self.active.take() {
            cx.notify();
            if let Some(closed) = active.closed.take() {
                closed(cx);
            }
        }
    }
}

/// Recursively build a [`PopupMenu`] entity from native menu items.
#[allow(dead_code)] // Only used where the native menu falls back (e.g. Linux).
fn build_popup(
    items: Vec<NativeMenuItem>,
    action_context: Option<FocusHandle>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<PopupMenu> {
    PopupMenu::build(window, cx, move |mut menu, window, cx| {
        if let Some(handle) = action_context.clone() {
            menu = menu.action_context(handle);
        }
        for item in items {
            menu = match item {
                NativeMenuItem::Separator => menu.separator(),
                NativeMenuItem::Item {
                    label,
                    disabled,
                    checked,
                    icon: Some(icon),
                    action: Some(action),
                } => menu.item(
                    PopupMenuItem::new(label)
                        .icon(*icon)
                        .action(action)
                        .disabled(disabled)
                        .checked(checked),
                ),
                NativeMenuItem::Item {
                    label,
                    disabled,
                    checked,
                    icon: None,
                    action: Some(action),
                } => menu.menu_with_check_and_disabled(label, checked, action, disabled),
                NativeMenuItem::Item { action: None, .. } => menu,
                NativeMenuItem::Submenu {
                    label,
                    disabled,
                    items,
                } => {
                    let submenu = build_popup(items, action_context.clone(), window, cx);
                    menu.item(PopupMenuItem::submenu(label, submenu).disabled(disabled))
                }
            };
        }
        menu
    })
}

impl Render for FallbackMenuOverlay {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let mut root = div();
        if let Some(active) = self.active.as_ref() {
            root = root.child(
                deferred(
                    anchored()
                        .position(active.position)
                        .snap_to_window_with_margin(px(8.))
                        .child(active.menu.clone()),
                )
                .with_priority(gpui_base::POPUP_PRIORITY),
            );
        }
        root
    }
}

/// Show the fallback popup menu through [`Root`]'s overlay.
#[allow(dead_code)] // Only called where the native menu falls back (e.g. Linux).
pub(super) fn show(
    items: Vec<NativeMenuItem>,
    position: Point<Pixels>,
    closed: Closed,
    window: &mut Window,
    cx: &mut App,
) {
    let action_context = window.focused(cx);
    let Some(overlay) = Root::native_menu_overlay(window, cx) else {
        closed(cx);
        return;
    };
    overlay.update(cx, |overlay, cx| {
        overlay.show(items, position, action_context, closed, window, cx);
    });
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use gpui::{
        AppContext as _, Context, DismissEvent, IntoElement, Render, TestAppContext, Window,
        actions, div, point, px,
    };

    use super::*;
    use crate::native_menu::NativeMenu;

    actions!(fallback_menu_test, [Probe]);

    struct TestView;

    impl Render for TestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// `NativeMenu::show_until_closed` tells when the menu is gone: dismissed,
    /// shown over by another, or at once where it has nothing to show
    /// (elundqvist/kvist#623).
    #[gpui::test]
    fn a_menu_shown_until_closed_tells_when_it_is_gone(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (_root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| TestView);
            Root::new(view, window, cx)
        });
        let counter = || {
            let count = Rc::new(Cell::new(0usize));
            (count.clone(), move |_: &mut App| count.set(count.get() + 1))
        };
        let probe = || NativeMenu::new().menu("Probe", Box::new(Probe));
        let at = point(px(10.), px(10.));

        let (first, first_closed) = counter();
        cx.update(|window, cx| probe().show_until_closed(at, window, cx, first_closed));
        assert_eq!(first.get(), 0, "while it is up");

        let (second, second_closed) = counter();
        cx.update(|window, cx| probe().show_until_closed(at, window, cx, second_closed));
        assert_eq!(first.get(), 1, "another shown over it");
        assert_eq!(second.get(), 0);

        cx.update(|window, cx| {
            let overlay = Root::native_menu_overlay(window, cx).unwrap();
            let menu = overlay.read(cx).active.as_ref().unwrap().menu.clone();
            menu.update(cx, |_, cx| cx.emit(DismissEvent));
        });
        assert_eq!(first.get(), 1);
        assert_eq!(second.get(), 1, "dismissed");
        cx.update(|window, cx| {
            let overlay = Root::native_menu_overlay(window, cx).unwrap();
            assert!(overlay.read(cx).active.is_none());
        });

        let (third, third_closed) = counter();
        cx.update(|window, cx| NativeMenu::new().show_until_closed(at, window, cx, third_closed));
        assert_eq!(third.get(), 1, "nothing to show");
    }
}
