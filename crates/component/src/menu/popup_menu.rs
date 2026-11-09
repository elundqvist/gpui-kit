use crate::ThemeStyled as _;
use crate::actions::{Cancel, Confirm, SelectDown, SelectUp};
use crate::actions::{SelectLeft, SelectRight};
use crate::menu::menu_item::MenuItemElement;
use crate::scroll::ScrollableElement;
use crate::{ActiveTheme, ElementExt as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use crate::{Side, Size, kbd::Kbd};
use gpui::{
    Action, Anchor, AnyElement, App, AppContext, Bounds, Context, DismissEvent, Edges, Entity,
    EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyBinding,
    ParentElement, Pixels, Render, Role, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, WeakEntity, Window, anchored, deferred, div, prelude::FluentBuilder, px, rems,
};
use gpui::{ClickEvent, Half, MouseDownEvent, OwnedMenuItem, Point, Subscription};

use std::rc::Rc;

const CONTEXT: &str = "PopupMenu";

/// The room a menu leaves between itself and the window's edges, as the
/// context menu is kept that far inside them.
const WINDOW_MARGIN: Pixels = px(8.);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", Confirm { secondary: false }, Some(CONTEXT)),
        KeyBinding::new("escape", Cancel, Some(CONTEXT)),
        KeyBinding::new("up", SelectUp, Some(CONTEXT)),
        KeyBinding::new("down", SelectDown, Some(CONTEXT)),
        KeyBinding::new("left", SelectLeft, Some(CONTEXT)),
        KeyBinding::new("right", SelectRight, Some(CONTEXT)),
    ]);
}

/// An menu item in a popup menu.
pub enum PopupMenuItem {
    /// A menu separator item.
    Separator,
    /// A non-interactive label item.
    Label(SharedString),
    /// A standard menu item.
    Item {
        icon: Option<Icon>,
        label: SharedString,
        disabled: bool,
        checked: bool,
        is_link: bool,
        action: Option<Box<dyn Action>>,
        // For link item
        handler: Option<Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>>,
    },
    /// A menu item with custom element render.
    ElementItem {
        icon: Option<Icon>,
        disabled: bool,
        checked: bool,
        action: Option<Box<dyn Action>>,
        render: Box<dyn Fn(&mut Window, &mut App) -> AnyElement + 'static>,
        handler: Option<Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>>,
    },
    /// A submenu item that opens another popup menu.
    Submenu {
        icon: Option<Icon>,
        label: SharedString,
        disabled: bool,
        menu: Entity<PopupMenu>,
    },
}

impl FluentBuilder for PopupMenuItem {}
impl PopupMenuItem {
    /// Create a new menu item with the given label.
    #[inline]
    pub fn new(label: impl Into<SharedString>) -> Self {
        PopupMenuItem::Item {
            icon: None,
            label: label.into(),
            disabled: false,
            checked: false,
            action: None,
            is_link: false,
            handler: None,
        }
    }

    /// Create a new menu item with custom element render.
    #[inline]
    pub fn element<F, E>(builder: F) -> Self
    where
        F: Fn(&mut Window, &mut App) -> E + 'static,
        E: IntoElement,
    {
        PopupMenuItem::ElementItem {
            icon: None,
            disabled: false,
            checked: false,
            action: None,
            render: Box::new(move |window, cx| builder(window, cx).into_any_element()),
            handler: None,
        }
    }

    /// Create a new submenu item that opens another popup menu.
    #[inline]
    pub fn submenu(label: impl Into<SharedString>, menu: Entity<PopupMenu>) -> Self {
        PopupMenuItem::Submenu {
            icon: None,
            label: label.into(),
            disabled: false,
            menu,
        }
    }

    /// Create a separator menu item.
    #[inline]
    pub fn separator() -> Self {
        PopupMenuItem::Separator
    }

    /// Creates a label menu item.
    #[inline]
    pub fn label(label: impl Into<SharedString>) -> Self {
        PopupMenuItem::Label(label.into())
    }

    /// Set the icon for the menu item.
    ///
    /// Only works for [`PopupMenuItem::Item`], [`PopupMenuItem::ElementItem`] and [`PopupMenuItem::Submenu`].
    pub fn icon(mut self, icon: impl Into<Icon>) -> Self {
        match &mut self {
            PopupMenuItem::Item { icon: i, .. } => {
                *i = Some(icon.into());
            }
            PopupMenuItem::ElementItem { icon: i, .. } => {
                *i = Some(icon.into());
            }
            PopupMenuItem::Submenu { icon: i, .. } => {
                *i = Some(icon.into());
            }
            _ => {}
        }
        self
    }

    /// Set the action for the menu item.
    ///
    /// Only works for [`PopupMenuItem::Item`] and [`PopupMenuItem::ElementItem`].
    pub fn action(mut self, action: Box<dyn Action>) -> Self {
        match &mut self {
            PopupMenuItem::Item { action: a, .. } => {
                *a = Some(action);
            }
            PopupMenuItem::ElementItem { action: a, .. } => {
                *a = Some(action);
            }
            _ => {}
        }
        self
    }

    /// Set the disabled state for the menu item.
    ///
    /// Only works for [`PopupMenuItem::Item`], [`PopupMenuItem::ElementItem`] and [`PopupMenuItem::Submenu`].
    pub fn disabled(mut self, disabled: bool) -> Self {
        match &mut self {
            PopupMenuItem::Item { disabled: d, .. } => {
                *d = disabled;
            }
            PopupMenuItem::ElementItem { disabled: d, .. } => {
                *d = disabled;
            }
            PopupMenuItem::Submenu { disabled: d, .. } => {
                *d = disabled;
            }
            _ => {}
        }
        self
    }

    /// Set checked state for the menu item.
    ///
    /// NOTE: If `check_side` is [`Side::Left`], the icon will replace with a check icon.
    pub fn checked(mut self, checked: bool) -> Self {
        match &mut self {
            PopupMenuItem::Item { checked: c, .. } => {
                *c = checked;
            }
            PopupMenuItem::ElementItem { checked: c, .. } => {
                *c = checked;
            }
            _ => {}
        }
        self
    }

    /// Add a click handler for the menu item.
    ///
    /// Only works for [`PopupMenuItem::Item`] and [`PopupMenuItem::ElementItem`].
    pub fn on_click<F>(mut self, handler: F) -> Self
    where
        F: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    {
        match &mut self {
            PopupMenuItem::Item { handler: h, .. } => {
                *h = Some(Rc::new(handler));
            }
            PopupMenuItem::ElementItem { handler: h, .. } => {
                *h = Some(Rc::new(handler));
            }
            _ => {}
        }
        self
    }

    /// Create a link menu item.
    #[inline]
    pub fn link(label: impl Into<SharedString>, href: impl Into<String>) -> Self {
        let href = href.into();
        PopupMenuItem::Item {
            icon: None,
            label: label.into(),
            disabled: false,
            checked: false,
            action: None,
            is_link: true,
            handler: Some(Rc::new(move |_, _, cx| cx.open_url(&href))),
        }
    }

    #[inline]
    fn is_clickable(&self) -> bool {
        !matches!(self, PopupMenuItem::Separator)
            && matches!(
                self,
                PopupMenuItem::Item {
                    disabled: false,
                    ..
                } | PopupMenuItem::ElementItem {
                    disabled: false,
                    ..
                } | PopupMenuItem::Submenu {
                    disabled: false,
                    ..
                }
            )
    }

    #[inline]
    fn is_separator(&self) -> bool {
        matches!(self, PopupMenuItem::Separator)
    }

    fn has_left_icon(&self, check_side: Side) -> bool {
        match self {
            PopupMenuItem::Item { icon, checked, .. } => {
                icon.is_some() || (check_side.is_left() && *checked)
            }
            PopupMenuItem::ElementItem { icon, checked, .. } => {
                icon.is_some() || (check_side.is_left() && *checked)
            }
            PopupMenuItem::Submenu { icon, .. } => icon.is_some(),
            _ => false,
        }
    }

    #[inline]
    fn is_checked(&self) -> bool {
        match self {
            PopupMenuItem::Item { checked, .. } => *checked,
            PopupMenuItem::ElementItem { checked, .. } => *checked,
            _ => false,
        }
    }

    fn a11y_label(&self) -> Option<SharedString> {
        match self {
            PopupMenuItem::Item { label, .. }
            | PopupMenuItem::Label(label)
            | PopupMenuItem::Submenu { label, .. } => Some(label.clone()),
            PopupMenuItem::Separator | PopupMenuItem::ElementItem { .. } => None,
        }
    }
}

pub struct PopupMenu {
    pub(crate) focus_handle: FocusHandle,
    pub(crate) menu_items: Vec<PopupMenuItem>,
    /// The focus handle of Entity to handle actions.
    pub(crate) action_context: Option<FocusHandle>,
    /// The focus to restore on dismiss. Unlike `action_context`, this does not
    /// change where actions are dispatched: they still bubble from the menu's
    /// own focus path (through the trigger element's ancestors).
    pub(crate) previous_focus_handle: Option<FocusHandle>,
    selected_index: Option<usize>,
    /// Whether the pointer resting on it lit the highlighted item, not the
    /// keyboard: `rebuild` keeps the one the keyboard lit by its label and
    /// the one the pointer lit where the pointer is.
    selected_by_pointer: bool,
    min_width: Option<Pixels>,
    max_width: Option<Pixels>,
    max_height: Option<Pixels>,
    bounds: Bounds<Pixels>,
    size: Size,
    check_side: Side,

    /// The parent menu of this menu, if this is a submenu
    parent_menu: Option<WeakEntity<Self>>,
    scrollable: bool,
    external_link_icon: bool,
    scroll_handle: ScrollHandle,
    /// The item to scroll to once the items have been laid out, and whether a frame has
    /// been drawn waiting for that (`PopupMenu::lit_checked`).
    scroll_to_lit: Option<(usize, bool)>,
    // This will update on render
    submenu_anchor: (Anchor, Pixels),

    /// Paint priority for this menu layer. The top-level menu starts at 1 and
    /// each nested submenu increments it, so deeper levels are always drawn on
    /// top of shallower ones. This fixes background content (e.g. the
    /// underlying list) bleeding through multi-level submenus, which happens
    /// when nested `anchored` popovers share the same paint order.
    ///
    /// The top-level menu relies on its container (e.g. `Popover`,
    /// `ContextMenu`) to `deferred`-draw it, and each submenu is deferred once
    /// in `render_item` with `priority + 1`. Keeping a single deferred layer
    /// per level matters because GPUI caps nested deferred depth (see
    /// `prepaint_deferred_draws`).
    priority: usize,

    _subscriptions: Vec<Subscription>,
}

impl PopupMenu {
    pub(crate) fn new(cx: &mut App) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            action_context: None,
            previous_focus_handle: None,
            parent_menu: None,
            menu_items: Vec::new(),
            selected_index: None,
            selected_by_pointer: false,
            min_width: None,
            max_width: None,
            max_height: None,
            check_side: Side::Left,
            bounds: Bounds::default(),
            scrollable: false,
            scroll_handle: ScrollHandle::default(),
            scroll_to_lit: None,
            external_link_icon: true,
            size: Size::default(),
            submenu_anchor: (Anchor::TopLeft, Pixels::ZERO),
            priority: gpui_base::POPUP_PRIORITY,
            _subscriptions: vec![],
        }
    }

    pub fn build(
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(Self, &mut Window, &mut Context<PopupMenu>) -> Self,
    ) -> Entity<Self> {
        cx.new(|cx| f(Self::new(cx), window, cx))
    }

    /// Set the focus handle of Entity to handle actions.
    ///
    /// When the menu is dismissed or before an action is triggered, the focus will be returned to this handle.
    ///
    /// Then the action will be dispatched to this handle.
    pub fn action_context(mut self, handle: FocusHandle) -> Self {
        self.action_context = Some(handle);
        self
    }

    pub(crate) fn set_action_context(
        &mut self,
        action_context: Option<FocusHandle>,
        cx: &mut Context<Self>,
    ) {
        self.action_context = action_context.clone();

        for item in &self.menu_items {
            if let PopupMenuItem::Submenu { menu, .. } = item {
                menu.update(cx, |menu, cx| {
                    menu.set_action_context(action_context.clone(), cx);
                });
            }
        }
    }

    /// Set the focus to restore when the menu is dismissed, without changing
    /// where actions are dispatched.
    pub(crate) fn set_previous_focus(
        &mut self,
        handle: Option<FocusHandle>,
        cx: &mut Context<Self>,
    ) {
        self.previous_focus_handle = handle.clone();

        for item in &self.menu_items {
            if let PopupMenuItem::Submenu { menu, .. } = item {
                menu.update(cx, |menu, cx| {
                    menu.set_previous_focus(handle.clone(), cx);
                });
            }
        }
    }

    /// Set min width of the popup menu, default is 120px
    pub fn min_w(mut self, width: impl Into<Pixels>) -> Self {
        self.min_width = Some(width.into());
        self
    }

    /// Set max width of the popup menu, default is 500px, and never wider
    /// than the window has room for.
    pub fn max_w(mut self, width: impl Into<Pixels>) -> Self {
        self.max_width = Some(width.into());
        self
    }

    /// Set max height of the popup menu, the rest scrolled; never taller
    /// than the window has room for, which is the default.
    pub fn max_h(mut self, height: impl Into<Pixels>) -> Self {
        self.max_height = Some(height.into());
        self
    }

    /// Keep the menu, where it sets no height of its own, to half the
    /// window's height and 450 px, the rest scrolled. Any menu scrolls
    /// where it is taller than the window has room for, its submenus drawn
    /// outside its scroll.
    pub fn scrollable(mut self, scrollable: bool) -> Self {
        self.scrollable = scrollable;
        self
    }

    /// Set the side to show check icon, default is `Side::Left`.
    pub fn check_side(mut self, side: Side) -> Self {
        self.check_side = side;
        self
    }

    /// Set the menu to show external link icon, default is true.
    pub fn external_link_icon(mut self, visible: bool) -> Self {
        self.external_link_icon = visible;
        self
    }

    /// Add Menu Item
    pub fn menu(self, label: impl Into<SharedString>, action: Box<dyn Action>) -> Self {
        self.menu_with_disabled(label, action, false)
    }

    /// Add Menu Item with enable state
    pub fn menu_with_enable(
        mut self,
        label: impl Into<SharedString>,
        action: Box<dyn Action>,
        enable: bool,
    ) -> Self {
        self.add_menu_item(label, None, action, !enable, false);
        self
    }

    /// Add Menu Item with disabled state
    pub fn menu_with_disabled(
        mut self,
        label: impl Into<SharedString>,
        action: Box<dyn Action>,
        disabled: bool,
    ) -> Self {
        self.add_menu_item(label, None, action, disabled, false);
        self
    }

    /// Add label
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.menu_items.push(PopupMenuItem::label(label.into()));
        self
    }

    /// Add Menu to open link
    pub fn link(self, label: impl Into<SharedString>, href: impl Into<String>) -> Self {
        self.link_with_disabled(label, href, false)
    }

    /// Add Menu to open link with disabled state
    pub fn link_with_disabled(
        mut self,
        label: impl Into<SharedString>,
        href: impl Into<String>,
        disabled: bool,
    ) -> Self {
        let href = href.into();
        self.menu_items
            .push(PopupMenuItem::link(label, href).disabled(disabled));
        self
    }

    /// Add Menu to open link
    pub fn link_with_icon(
        self,
        label: impl Into<SharedString>,
        icon: impl Into<Icon>,
        href: impl Into<String>,
    ) -> Self {
        self.link_with_icon_and_disabled(label, icon, href, false)
    }

    /// Add Menu to open link with icon and disabled state
    fn link_with_icon_and_disabled(
        mut self,
        label: impl Into<SharedString>,
        icon: impl Into<Icon>,
        href: impl Into<String>,
        disabled: bool,
    ) -> Self {
        let href = href.into();
        self.menu_items.push(
            PopupMenuItem::link(label, href)
                .icon(icon)
                .disabled(disabled),
        );
        self
    }

    /// Add Menu Item with Icon.
    pub fn menu_with_icon(
        self,
        label: impl Into<SharedString>,
        icon: impl Into<Icon>,
        action: Box<dyn Action>,
    ) -> Self {
        self.menu_with_icon_and_disabled(label, icon, action, false)
    }

    /// Add Menu Item with Icon and disabled state
    pub fn menu_with_icon_and_disabled(
        mut self,
        label: impl Into<SharedString>,
        icon: impl Into<Icon>,
        action: Box<dyn Action>,
        disabled: bool,
    ) -> Self {
        self.add_menu_item(label, Some(icon.into()), action, disabled, false);
        self
    }

    /// Add Menu Item with check icon
    pub fn menu_with_check(
        self,
        label: impl Into<SharedString>,
        checked: bool,
        action: Box<dyn Action>,
    ) -> Self {
        self.menu_with_check_and_disabled(label, checked, action, false)
    }

    /// Add Menu Item with check icon and disabled state
    pub fn menu_with_check_and_disabled(
        mut self,
        label: impl Into<SharedString>,
        checked: bool,
        action: Box<dyn Action>,
        disabled: bool,
    ) -> Self {
        self.add_menu_item(label, None, action, disabled, checked);
        self
    }

    /// Add Menu Item with custom element render.
    pub fn menu_element<F, E>(self, action: Box<dyn Action>, builder: F) -> Self
    where
        F: Fn(&mut Window, &mut App) -> E + 'static,
        E: IntoElement,
    {
        self.menu_element_with_check(false, action, builder)
    }

    /// Add Menu Item with custom element render with disabled state.
    pub fn menu_element_with_disabled<F, E>(
        self,
        action: Box<dyn Action>,
        disabled: bool,
        builder: F,
    ) -> Self
    where
        F: Fn(&mut Window, &mut App) -> E + 'static,
        E: IntoElement,
    {
        self.menu_element_with_check_and_disabled(false, action, disabled, builder)
    }

    /// Add Menu Item with custom element render with icon.
    pub fn menu_element_with_icon<F, E>(
        self,
        icon: impl Into<Icon>,
        action: Box<dyn Action>,
        builder: F,
    ) -> Self
    where
        F: Fn(&mut Window, &mut App) -> E + 'static,
        E: IntoElement,
    {
        self.menu_element_with_icon_and_disabled(icon, action, false, builder)
    }

    /// Add Menu Item with custom element render with check state
    pub fn menu_element_with_check<F, E>(
        self,
        checked: bool,
        action: Box<dyn Action>,
        builder: F,
    ) -> Self
    where
        F: Fn(&mut Window, &mut App) -> E + 'static,
        E: IntoElement,
    {
        self.menu_element_with_check_and_disabled(checked, action, false, builder)
    }

    /// Add Menu Item with custom element render with icon and disabled state
    fn menu_element_with_icon_and_disabled<F, E>(
        mut self,
        icon: impl Into<Icon>,
        action: Box<dyn Action>,
        disabled: bool,
        builder: F,
    ) -> Self
    where
        F: Fn(&mut Window, &mut App) -> E + 'static,
        E: IntoElement,
    {
        self.menu_items.push(
            PopupMenuItem::element(builder)
                .action(action)
                .icon(icon)
                .disabled(disabled),
        );
        self
    }

    /// Add Menu Item with custom element render with check state and disabled state
    fn menu_element_with_check_and_disabled<F, E>(
        mut self,
        checked: bool,
        action: Box<dyn Action>,
        disabled: bool,
        builder: F,
    ) -> Self
    where
        F: Fn(&mut Window, &mut App) -> E + 'static,
        E: IntoElement,
    {
        self.menu_items.push(
            PopupMenuItem::element(builder)
                .action(action)
                .checked(checked)
                .disabled(disabled),
        );
        self
    }

    /// Add a separator Menu Item
    pub fn separator(mut self) -> Self {
        if self.menu_items.is_empty() {
            return self;
        }

        if let Some(PopupMenuItem::Separator) = self.menu_items.last() {
            return self;
        }

        self.menu_items.push(PopupMenuItem::separator());
        self
    }

    /// Add a Submenu
    pub fn submenu(
        self,
        label: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> Self {
        self.submenu_with_icon(None, label, window, cx, f)
    }

    /// Add a Submenu item with icon
    pub fn submenu_with_icon(
        mut self,
        icon: Option<Icon>,
        label: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> Self {
        let submenu = PopupMenu::build(window, cx, f);
        let parent_menu = cx.entity().downgrade();
        let parent_priority = self.priority;
        submenu.update(cx, |view, _| {
            view.parent_menu = Some(parent_menu);
            view.priority = parent_priority + 1;
        });

        self.menu_items.push(
            PopupMenuItem::submenu(label, submenu).when_some(icon, |this, icon| this.icon(icon)),
        );
        self
    }

    /// Add menu item.
    pub fn item(mut self, item: impl Into<PopupMenuItem>) -> Self {
        let item: PopupMenuItem = item.into();
        self.menu_items.push(item);
        self
    }

    /// Light the first of the items added so far that is checked and can be
    /// chosen, as the menu opens, the one a select has chosen: the keyboard
    /// starts from it, as in VS Code's select and a native one, Down going on
    /// to the item after it, Up to the one before and Return keeping it,
    /// where the first Down lit the menu's first item (elundqvist/kvist#678).
    /// A menu with no such item opens with none lit. A menu with more items
    /// than it has room for opens scrolled to it (`scroll_to_lit`).
    pub fn lit_checked(mut self) -> Self {
        if let Some(ix) = self
            .menu_items
            .iter()
            .position(|item| item.is_clickable() && item.is_checked())
        {
            self.selected_index = Some(ix);
            self.selected_by_pointer = false;
            self.scroll_to_lit = Some((ix, false));
        }
        self
    }

    /// Whether the menu is drawn hidden this frame, as it waits to be laid out before it
    /// scrolls to the item lit as it opened, which it then does: a scroll asked of a menu
    /// not yet laid out has no room to go by and is lost, so a select of more options than
    /// fit opened at its top, the option chosen out of sight and no row lit
    /// (elundqvist/kvist#678). It waits one frame, drawn hidden, so the menu is first seen
    /// scrolled, and scrolls the next whatever it has.
    fn scroll_to_lit_item(&mut self, window: &mut Window) -> bool {
        match self.scroll_to_lit {
            Some((ix, waited)) if waited || self.scroll_handle.bounds().size.height > px(0.) => {
                self.scroll_handle.scroll_to_item(ix);
                self.scroll_to_lit = None;
                false
            }
            Some((ix, _)) => {
                self.scroll_to_lit = Some((ix, true));
                window.request_animation_frame();
                true
            }
            None => false,
        }
    }

    /// Replace all menu items by re-running a builder on this menu, keeping its
    /// identity (focus, parent menu, layer priority).
    ///
    /// For menus whose content arrives asynchronously after the menu is shown,
    /// e.g. swapping a "loading…" placeholder for the loaded items:
    ///
    /// ```ignore
    /// cx.spawn_in(window, async move |menu, cx| {
    ///     let items = fetch_items().await;
    ///     _ = menu.update_in(cx, |menu, window, cx| {
    ///         menu.rebuild(window, cx, |menu, _, _| {
    ///             items.into_iter().fold(menu, |menu, item| {
    ///                 menu.menu(item.label, Box::new(item.action))
    ///             })
    ///         });
    ///     });
    /// })
    /// .detach();
    /// ```
    ///
    /// An item the keyboard highlighted stays highlighted where the rebuilt
    /// menu still has an item of its label that can be chosen, wherever it
    /// now stands and scrolled into view, so a menu rebuilt while it is open
    /// keeps the keyboard's place in it; where it has none, nothing is
    /// highlighted. An item the pointer highlighted is the row under the
    /// pointer, whatever it now says, as a click there would take it.
    pub fn rebuild(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(Self, &mut Window, &mut Context<Self>) -> Self,
    ) {
        let kept = self.selected_index.and_then(|ix| {
            self.menu_items
                .get(ix)
                .and_then(|item| item.a11y_label())
                .map(|label| (ix, label))
        });
        let by_pointer = self.selected_by_pointer;
        let mut menu = std::mem::replace(self, Self::new(cx));
        menu.menu_items.clear();
        menu.selected_index = None;
        *self = f(menu, window, cx);
        self.selected_index = kept.and_then(|(ix, label)| {
            if by_pointer {
                self.menu_items
                    .get(ix)
                    .is_some_and(|item| item.is_clickable())
                    .then_some(ix)
            } else {
                self.clickable_index_of(ix, &label)
            }
        });
        self.selected_by_pointer = by_pointer && self.selected_index.is_some();
        self.scroll_to_lit = self
            .selected_index
            .filter(|_| !by_pointer)
            .map(|ix| (ix, false));
        cx.notify();
    }

    /// The index of an item labelled `label` that can be chosen: `was`, where
    /// the item there is one, else the first.
    fn clickable_index_of(&self, was: usize, label: &SharedString) -> Option<usize> {
        let labelled =
            |item: &PopupMenuItem| item.is_clickable() && item.a11y_label().as_ref() == Some(label);
        if self.menu_items.get(was).is_some_and(labelled) {
            return Some(was);
        }
        self.menu_items.iter().position(labelled)
    }

    fn add_menu_item(
        &mut self,
        label: impl Into<SharedString>,
        icon: Option<Icon>,
        action: Box<dyn Action>,
        disabled: bool,
        checked: bool,
    ) -> &mut Self {
        self.menu_items.push(
            PopupMenuItem::new(label)
                .when_some(icon, |item, icon| item.icon(icon))
                .disabled(disabled)
                .checked(checked)
                .action(action),
        );
        self
    }

    pub(super) fn with_menu_items<I>(
        mut self,
        items: impl IntoIterator<Item = I>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self
    where
        I: Into<OwnedMenuItem>,
    {
        for item in items {
            match item.into() {
                OwnedMenuItem::Action {
                    name,
                    action,
                    checked,
                    disabled,
                    ..
                } => {
                    self = self.menu_with_check_and_disabled(
                        name,
                        checked,
                        action.boxed_clone(),
                        disabled,
                    )
                }
                OwnedMenuItem::Separator => {
                    self = self.separator();
                }
                OwnedMenuItem::Submenu(submenu) => {
                    self = self.submenu(submenu.name, window, cx, move |menu, window, cx| {
                        menu.with_menu_items(submenu.items.clone(), window, cx)
                    })
                }
                OwnedMenuItem::SystemMenu(_) => {}
            }
        }

        if self.menu_items.len() > 20 {
            self.scrollable = true;
        }

        self
    }

    pub(crate) fn active_submenu(&self) -> Option<Entity<PopupMenu>> {
        if let Some(ix) = self.selected_index {
            if let Some(item) = self.menu_items.get(ix) {
                return match item {
                    PopupMenuItem::Submenu { menu, .. } => Some(menu.clone()),
                    _ => None,
                };
            }
        }

        None
    }

    pub fn is_empty(&self) -> bool {
        self.menu_items.is_empty()
    }

    fn clickable_menu_items(&self) -> impl Iterator<Item = (usize, &PopupMenuItem)> {
        self.menu_items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.is_clickable())
    }

    fn on_click(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        window.prevent_default();
        self.selected_index = Some(ix);
        self.confirm(&Confirm { secondary: false }, window, cx);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        match self.selected_index {
            Some(index) => {
                let item = self.menu_items.get(index);
                match item {
                    Some(PopupMenuItem::Item {
                        handler, action, ..
                    }) => {
                        if let Some(handler) = handler {
                            handler(&ClickEvent::default(), window, cx);
                        } else if let Some(action) = action.as_ref() {
                            self.dispatch_confirm_action(action, window, cx);
                        }

                        self.dismiss(&Cancel, window, cx)
                    }
                    Some(PopupMenuItem::ElementItem {
                        handler, action, ..
                    }) => {
                        if let Some(handler) = handler {
                            handler(&ClickEvent::default(), window, cx);
                        } else if let Some(action) = action.as_ref() {
                            self.dispatch_confirm_action(action, window, cx);
                        }
                        self.dismiss(&Cancel, window, cx)
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn dispatch_confirm_action(
        &self,
        action: &Box<dyn Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(context) = self.action_context.as_ref() {
            context.focus(window, cx);
        }

        window.dispatch_action(action.boxed_clone(), cx);
    }

    fn hover_item(&mut self, ix: usize, hovered: bool, is_submenu: bool, cx: &mut Context<Self>) {
        if hovered {
            self.selected_index = Some(ix);
            self.selected_by_pointer = true;
        } else if !is_submenu && self.selected_index == Some(ix) {
            // TODO: Better handle the submenu unselection when hover out
            self.selected_index = None;
        }

        cx.notify();
    }

    fn set_selected_index(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected_by_pointer = false;
        if self.selected_index != Some(ix) {
            self.selected_index = Some(ix);
            self.scroll_handle.scroll_to_item(ix);
            cx.notify();
        }
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let ix = self.selected_index.unwrap_or(0);

        if let Some((prev_ix, _)) = self
            .menu_items
            .iter()
            .enumerate()
            .rev()
            .find(|(i, item)| *i < ix && item.is_clickable())
        {
            self.set_selected_index(prev_ix, cx);
            return;
        }

        let last_clickable_ix = self.clickable_menu_items().last().map(|(ix, _)| ix);
        self.set_selected_index(last_clickable_ix.unwrap_or(0), cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        // the first item that can be chosen, past the one highlighted: a
        // label or a disabled item at the top took the first Down unseen
        let after = self.selected_index;
        let next = self
            .clickable_menu_items()
            .find(|(i, _)| after.is_none_or(|ix| *i > ix))
            .or_else(|| self.clickable_menu_items().next())
            .map(|(ix, _)| ix);
        self.set_selected_index(next.unwrap_or(0), cx);
    }

    fn select_left(&mut self, _: &SelectLeft, window: &mut Window, cx: &mut Context<Self>) {
        let handled = if matches!(self.submenu_anchor.0, Anchor::TopLeft | Anchor::BottomLeft) {
            self._unselect_submenu(window, cx)
        } else {
            self._select_submenu(window, cx)
        };

        if self.parent_side(cx).is_left() {
            self._focus_parent_menu(window, cx);
        }

        if handled {
            return;
        }

        // For parent AppMenuBar to handle.
        if self.parent_menu.is_none() {
            cx.propagate();
        }
    }

    fn select_right(&mut self, _: &SelectRight, window: &mut Window, cx: &mut Context<Self>) {
        let handled = if matches!(self.submenu_anchor.0, Anchor::TopLeft | Anchor::BottomLeft) {
            self._select_submenu(window, cx)
        } else {
            self._unselect_submenu(window, cx)
        };

        if self.parent_side(cx).is_right() {
            self._focus_parent_menu(window, cx);
        }

        if handled {
            return;
        }

        // For parent AppMenuBar to handle.
        if self.parent_menu.is_none() {
            cx.propagate();
        }
    }

    fn _select_submenu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(active_submenu) = self.active_submenu() {
            // Focus the submenu, so that can be handle the action.
            active_submenu.update(cx, |view, cx| {
                view.set_selected_index(0, cx);
                view.focus_handle.focus(window, cx);
            });
            cx.notify();
            return true;
        }

        return false;
    }

    fn _unselect_submenu(&mut self, _: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(active_submenu) = self.active_submenu() {
            active_submenu.update(cx, |view, cx| {
                view.selected_index = None;
                cx.notify();
            });
            return true;
        }

        return false;
    }

    fn _focus_parent_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(parent) = self.parent_menu.as_ref() else {
            return;
        };
        let Some(parent) = parent.upgrade() else {
            return;
        };

        self.selected_index = None;
        parent.update(cx, |view, cx| {
            view.focus_handle.focus(window, cx);
            cx.notify();
        });
    }

    fn parent_side(&self, cx: &App) -> Side {
        let Some(parent) = self.parent_menu.as_ref() else {
            return Side::Left;
        };

        let Some(parent) = parent.upgrade() else {
            return Side::Left;
        };

        match parent.read(cx).submenu_anchor.0 {
            Anchor::TopLeft | Anchor::BottomLeft => Side::Left,
            Anchor::TopRight | Anchor::BottomRight => Side::Right,
            // Center anchors are not used for submenu positioning, but we must cover them.
            _ => Side::Left,
        }
    }

    /// Dismiss the menu and the entire parent chain.
    ///
    /// The submenu is closed together with its parent, same as macOS menus.
    fn dismiss(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_index = None;
        cx.emit(DismissEvent);

        // Focus back to the previous focused handle, unless the item's click
        // handler has already moved focus elsewhere (e.g. opened a dialog and
        // focused its input) -- stealing focus back would break that.
        let focus_moved_away =
            window.focused(cx).is_some() && !self.focus_handle.contains_focused(window, cx);
        if !focus_moved_away {
            if let Some(handle) = self
                .previous_focus_handle
                .as_ref()
                .or(self.action_context.as_ref())
            {
                window.focus(handle, cx);
            }
        }

        let Some(parent_menu) = self.parent_menu.clone() else {
            return;
        };

        // Dismiss parent menu, when this menu is dismissed
        _ = parent_menu.update(cx, |view, cx| {
            view.dismiss(&Cancel, window, cx);
        });
    }

    fn handle_dismiss(
        &mut self,
        position: &Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Do not dismiss, if click inside the parent menu
        if let Some(parent) = self.parent_menu.as_ref() {
            if let Some(parent) = parent.upgrade() {
                if parent.read(cx).bounds.contains(position) {
                    return;
                }
            }
        }

        // Do not dismiss, if there have an active submenu, the click may be
        // inside the submenu, let the submenu to handle it.
        //
        // Otherwise the submenu will be dismissed before its item's `on_click`.
        if self.active_submenu().is_some() {
            return;
        }

        self.dismiss(&Cancel, window, cx);
    }

    fn on_mouse_down_out(
        &mut self,
        e: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_dismiss(&e.position, window, cx);
    }

    fn render_key_binding(
        &self,
        action: Option<Box<dyn Action>>,
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Kbd> {
        let action = action?;

        match self
            .action_context
            .as_ref()
            .or(self.previous_focus_handle.as_ref())
            .and_then(|handle| Kbd::binding_for_action_in(action.as_ref(), handle, window))
        {
            Some(kbd) => Some(kbd),
            // Fallback to App level key binding
            None => Kbd::binding_for_action(action.as_ref(), None, window),
        }
        .map(|this| {
            this.p_0()
                .flex_nowrap()
                .border_0()
                .bg(gpui::transparent_white())
        })
    }

    fn render_icon(
        has_icon: bool,
        checked: bool,
        icon: Option<Icon>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        if !has_icon {
            return None;
        }

        let icon = if let Some(icon) = icon {
            icon.clone()
        } else if checked {
            Icon::new(IconName::Check)
        } else {
            Icon::empty()
        };

        Some(icon.xsmall())
    }

    /// An item's label on one line, cut with an ellipsis where the menu is
    /// narrower than it: wrapped, it ran onto a second line its row has no
    /// height for (elundqvist/kvist#634).
    fn render_label(label: &SharedString) -> impl IntoElement {
        div()
            .debug_selector(|| format!("popup-menu-label:{label}"))
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .child(label.clone())
    }

    #[inline]
    fn max_width(&self) -> Pixels {
        self.max_width.unwrap_or(px(500.))
    }

    /// How tall and how wide the menu may be in a window whose viewport is
    /// `viewport` and whose drawn edge stands `inset` inside it (the shadow
    /// a window that draws its own decorations keeps, `Window::client_inset`):
    /// no more than the window has room for inside the margin a menu keeps
    /// from its edges (`WINDOW_MARGIN`), whatever the menu asks, so a menu
    /// taller than a small window scrolls and a menu wider than a narrow one
    /// cuts its labels short, where its last rows ran past the window's
    /// bottom and could not be reached (elundqvist/kvist#634). The inset is
    /// taken off each side as the menu's `anchored` takes it, which keeps
    /// the margin and the inset from the edges: without it, on Wayland, a
    /// menu as tall as its room stood the inset lower than its room, its
    /// last row past the window's foot. A scrollable menu with no height of
    /// its own keeps to half the window, and 450 px, as it did.
    ///
    /// A menu hung from a trigger keeps to the room of its popup, `popup`
    /// (`Positioner::content_room`), the side of the trigger with more room
    /// and no more, and scrolls: one taller than the room on both sides was
    /// held into the window over its own trigger (elundqvist/kvist#665).
    fn room(
        &self,
        viewport: gpui::Size<Pixels>,
        inset: Pixels,
        popup: Option<gpui::Size<Pixels>>,
    ) -> gpui::Size<Pixels> {
        let margin = WINDOW_MARGIN + inset;
        let room = gpui::Size {
            width: (viewport.width - margin * 2.).max(px(0.)),
            height: (viewport.height - margin * 2.).max(px(0.)),
        };
        let room = popup.map_or(room, |popup| gpui::Size {
            width: room.width.min(popup.width),
            height: room.height.min(popup.height),
        });
        let height = match self.max_height {
            Some(height) => height,
            None if self.scrollable => (viewport.height * 0.5).min(px(450.)),
            None => room.height,
        };
        gpui::Size {
            width: self.max_width().min(room.width),
            height: height.min(room.height),
        }
    }

    /// Calculate the anchor corner and left offset for child submenu.
    ///
    /// Against the viewport, not the window's bounds: a window zoomed
    /// (`Window::set_zoom`, elundqvist/kvist#563) is laid out in its size over
    /// the zoom, which the viewport is, while its bounds stay the display's.
    /// The same holds for the context menu's backdrop, the menu's height, the
    /// hover popover and the code action menu.
    fn update_submenu_menu_anchor(&mut self, window: &Window) {
        let bounds = self.bounds;
        let max_width = self.max_width();
        let (anchor, left) = if max_width + bounds.origin.x > window.viewport_size().width {
            (Anchor::TopRight, -px(16.))
        } else {
            (Anchor::TopLeft, bounds.size.width - px(8.))
        };

        let is_bottom_pos = bounds.origin.y + bounds.size.height > window.viewport_size().height;
        self.submenu_anchor = if is_bottom_pos {
            (anchor.other_side_along(gpui::Axis::Vertical), left)
        } else {
            (anchor, left)
        };
    }

    fn render_item(
        &self,
        ix: usize,
        item: &PopupMenuItem,
        options: RenderOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> MenuItemElement {
        let has_left_icon = options.has_left_icon;
        let is_left_check = options.check_side.is_left() && item.is_checked();
        let right_check_icon = if options.check_side.is_right() && item.is_checked() {
            Some(Icon::new(IconName::Check).xsmall())
        } else {
            None
        };

        let selected = self.selected_index == Some(ix);
        const EDGE_PADDING: Pixels = px(4.);
        const INNER_PADDING: Pixels = px(8.);

        let is_submenu = matches!(item, PopupMenuItem::Submenu { .. });

        let (item_height, radius) = match self.size {
            Size::Small => (px(20.), options.radius.half()),
            _ => (px(26.), options.radius),
        };

        // each row its own height in a menu taller than the room, which
        // scrolls it rather than squeeze its rows
        let this = MenuItemElement::new(ix)
            .flex_shrink_0()
            .relative()
            .text_sm()
            .py_0()
            .px(INNER_PADDING)
            .rounded(radius)
            .items_center()
            .selected(selected)
            .on_hover(cx.listener(move |this, hovered, _, cx| {
                this.hover_item(ix, *hovered, is_submenu, cx)
            }))
            .when_some(item.a11y_label(), |this, label| this.aria_label(label));

        match item {
            PopupMenuItem::Separator => this
                .h_auto()
                .p_0()
                .my_0p5()
                .mx_neg_1()
                .border_b(px(2.))
                .border_color(cx.theme().border)
                .disabled(true),
            PopupMenuItem::Label(label) => this.disabled(true).cursor_default().child(
                h_flex()
                    .cursor_default()
                    .items_center()
                    .gap_x_1()
                    .children(Self::render_icon(has_left_icon, false, None, window, cx))
                    .child(div().flex_1().child(label.clone())),
            ),
            PopupMenuItem::ElementItem {
                render,
                icon,
                disabled,
                ..
            } => this
                .when(!disabled, |this| {
                    this.on_click(
                        cx.listener(move |this, _, window, cx| this.on_click(ix, window, cx)),
                    )
                })
                .disabled(*disabled)
                .child(
                    h_flex()
                        .flex_1()
                        .min_h(item_height)
                        .items_center()
                        .gap_x_1()
                        .children(Self::render_icon(
                            has_left_icon,
                            is_left_check,
                            icon.clone(),
                            window,
                            cx,
                        ))
                        .child((render)(window, cx))
                        .children(right_check_icon.map(|icon| icon.ml_3())),
                ),
            PopupMenuItem::Item {
                icon,
                label,
                action,
                disabled,
                is_link,
                ..
            } => {
                let show_link_icon = *is_link && self.external_link_icon;
                let action = action.as_ref().map(|action| action.boxed_clone());
                let key = self.render_key_binding(action, window, cx);

                this.when(!disabled, |this| {
                    this.on_click(
                        cx.listener(move |this, _, window, cx| this.on_click(ix, window, cx)),
                    )
                })
                .disabled(*disabled)
                .h(item_height)
                .gap_x_1()
                .children(Self::render_icon(
                    has_left_icon,
                    is_left_check,
                    icon.clone(),
                    window,
                    cx,
                ))
                .child(
                    // no wider than the row, however long its label: as wide
                    // as its label, it pushed the key past the menu's edge
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_3()
                        .items_center()
                        .justify_between()
                        .when(!show_link_icon, |this| {
                            this.child(Self::render_label(label))
                        })
                        .children(right_check_icon)
                        .when(show_link_icon, |this| {
                            this.child(
                                h_flex()
                                    .w_full()
                                    .min_w_0()
                                    .justify_between()
                                    .gap_1p5()
                                    .child(Self::render_label(label))
                                    .child(
                                        Icon::new(IconName::ExternalLink)
                                            .xsmall()
                                            .text_color(cx.theme().muted_foreground),
                                    ),
                            )
                        })
                        .children(key),
                )
            }
            PopupMenuItem::Submenu {
                icon,
                label,
                menu,
                disabled,
            } => this
                .selected(selected)
                .disabled(*disabled)
                .items_start()
                .child(
                    h_flex()
                        .min_h(item_height)
                        .size_full()
                        .min_w_0()
                        .items_center()
                        .gap_x_1()
                        .children(Self::render_icon(
                            has_left_icon,
                            false,
                            icon.clone(),
                            window,
                            cx,
                        ))
                        .child(
                            // as the item's: its chevron stays in the row
                            h_flex()
                                .debug_selector(|| "popup-menu-submenu-row".into())
                                .flex_1()
                                .min_w_0()
                                .gap_2()
                                .items_center()
                                .justify_between()
                                .child(Self::render_label(label))
                                .child(
                                    Icon::new(IconName::ChevronRight)
                                        .xsmall()
                                        .text_color(cx.theme().muted_foreground),
                                ),
                        ),
                )
                .when(selected, |this| {
                    this.child({
                        let (anchor, left) = self.submenu_anchor;
                        let is_bottom_pos =
                            matches!(anchor, Anchor::BottomLeft | Anchor::BottomRight);
                        deferred(
                            anchored()
                                .anchor(anchor)
                                .child(
                                    div()
                                        .id("submenu")
                                        .occlude()
                                        .when(is_bottom_pos, |this| this.bottom_0())
                                        .when(!is_bottom_pos, |this| this.top_neg_1())
                                        .left(left)
                                        .child(menu.clone()),
                                )
                                .snap_to_window_with_margin(Edges::all(EDGE_PADDING)),
                        )
                        .with_priority(self.priority + 1)
                    })
                }),
        }
    }
}

impl FluentBuilder for PopupMenu {}
impl EventEmitter<DismissEvent> for PopupMenu {}
impl Focusable for PopupMenu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[derive(Clone, Copy)]
struct RenderOptions {
    has_left_icon: bool,
    check_side: Side,
    radius: Pixels,
}

impl Render for PopupMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_submenu_menu_anchor(window);
        let unplaced = self.scroll_to_lit_item(window);

        // Submenus attached via the public `item()` + `PopupMenuItem::submenu()`
        // path (from contexts that only have the menu value, e.g. a table
        // delegate's `context_menu`) have no parent wired at construction time.
        // Wire them here so the dismiss chain, click-outside checks and keyboard
        // navigation treat them the same as `submenu()`-built children.
        let parent = cx.entity().downgrade();
        let parent_priority = self.priority;
        for item in &self.menu_items {
            if let PopupMenuItem::Submenu { menu, .. } = item {
                if menu.read(cx).parent_menu.is_none() {
                    menu.update(cx, |menu, _| {
                        menu.parent_menu = Some(parent.clone());
                        menu.priority = parent_priority + 1;
                    });
                }
            }
        }

        let view = cx.entity().clone();
        let items_count = self.menu_items.len();

        // the items' room, inside the menu's own padding
        let pad = rems(0.25).to_pixels(window.rem_size());
        // a submenu hangs from its row, not from the popup's trigger
        let popup = gpui_base::Positioner::content_room(cx).filter(|_| self.parent_menu.is_none());
        let room = self
            .room(
                window.viewport_size(),
                window.client_inset().unwrap_or(px(0.)),
                popup,
            )
            .map(|v| (v - pad * 2.).max(px(0.)));

        let has_left_icon = self
            .menu_items
            .iter()
            .any(|item| item.has_left_icon(self.check_side));

        let options = RenderOptions {
            has_left_icon,
            check_side: self.check_side,
            radius: cx.theme().radius.min(px(8.)),
        };

        v_flex()
            .id("popup-menu")
            .role(Role::Menu)
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .on_mouse_down_out(cx.listener(Self::on_mouse_down_out))
            .popover_style(cx)
            .text_color(cx.theme().popover_foreground)
            .when(unplaced, |this| this.invisible())
            .relative()
            .occlude()
            // the room around the items is the menu's, not the scroll's:
            // a scroll's own padding made a menu that fits a few pixels
            // short of its rows, and scrollable by them
            .p_1()
            .child(
                v_flex()
                    .id("items")
                    .debug_selector(|| "popup-menu-items".into())
                    .gap_y_0p5()
                    .min_w(rems(8.))
                    .when_some(self.min_width, |this, min_width| this.min_w(min_width))
                    .max_w(room.width)
                    .max_h(room.height)
                    // as tall as its rows up to the room, whatever the box
                    // it is drawn in offers: a scroll is no floor, and a
                    // submenu hung from a row near the window's foot was
                    // a few pixels short of its rows and scrolled them
                    .flex_shrink_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .children(
                        self.menu_items
                            .iter()
                            .enumerate()
                            // Ignore last separator
                            .filter(|(ix, item)| !(*ix + 1 == items_count && item.is_separator()))
                            .map(|(ix, item)| self.render_item(ix, item, options, window, cx)),
                    ),
            )
            // the menu's whole box, its padding in it: a press there is in
            // the menu, and a submenu hangs from its edge
            .on_prepaint(move |bounds, _, cx| view.update(cx, |r, _| r.bounds = bounds))
            // shown only where the items are more than the room; a submenu
            // is drawn deferred, outside the items' clip
            .vertical_scrollbar(&self.scroll_handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn popup_menu_item_a11y_label_uses_visible_label(cx: &mut gpui::TestAppContext) {
        let submenu = cx.update(|cx| cx.new(|cx| PopupMenu::new(cx)));

        assert_eq!(PopupMenuItem::new("Open").a11y_label(), Some("Open".into()));
        assert_eq!(
            PopupMenuItem::link("Docs", "https://example.com").a11y_label(),
            Some("Docs".into())
        );
        assert_eq!(
            PopupMenuItem::label("Recent files").a11y_label(),
            Some("Recent files".into())
        );
        assert_eq!(
            PopupMenuItem::submenu("More", submenu).a11y_label(),
            Some("More".into())
        );
        assert_eq!(PopupMenuItem::separator().a11y_label(), None);
        assert_eq!(PopupMenuItem::element(|_, _| div()).a11y_label(), None);
    }

    fn items(menu: PopupMenu, labels: &[&'static str]) -> PopupMenu {
        labels.iter().fold(menu, |menu, label| match *label {
            "-" => menu.separator(),
            label if label.starts_with('~') => {
                menu.item(PopupMenuItem::new(&label[1..]).disabled(true))
            }
            label => menu.item(PopupMenuItem::new(label)),
        })
    }

    /// A select's menu opens on the option it has chosen, lit, so Down goes
    /// to the option after it and Up to the one before, where the first Down
    /// lit the first option (elundqvist/kvist#678); a menu not told to opens
    /// with none lit, and one with nothing checked that can be chosen too.
    #[gpui::test]
    fn a_menu_told_to_starts_from_its_checked_item(cx: &mut gpui::TestAppContext) {
        cx.update(crate::init);
        let options = |menu: PopupMenu| {
            ["GET", "POST", "PUT", "PATCH"].iter().fold(menu.label("Method"), |menu, m| {
                menu.item(PopupMenuItem::new(*m).checked(*m == "PUT"))
            })
        };
        let (menu, cx) = cx.add_window_view(|_, cx| options(PopupMenu::new(cx)).lit_checked());
        assert_eq!(menu.update(cx, |menu, _| menu.selected_index), Some(3));
        let key = |cx: &mut gpui::VisualTestContext, down: bool| {
            cx.update(|window, cx| {
                menu.update(cx, |menu, cx| {
                    if down {
                        menu.select_down(&SelectDown, window, cx);
                    } else {
                        menu.select_up(&SelectUp, window, cx);
                    }
                    menu.selected_index
                })
            })
        };
        assert_eq!(key(cx, true), Some(4), "Down from PUT is PATCH");
        assert_eq!(key(cx, false), Some(3));
        assert_eq!(key(cx, false), Some(2), "Up from PUT is POST");

        let (menu, cx) = cx.add_window_view(|_, cx| options(PopupMenu::new(cx)));
        assert_eq!(menu.update(cx, |menu, _| menu.selected_index), None);
        let (menu, cx) = cx.add_window_view(|_, cx| {
            PopupMenu::new(cx)
                .item(PopupMenuItem::new("A"))
                .item(PopupMenuItem::new("B").checked(true).disabled(true))
                .lit_checked()
        });
        assert_eq!(menu.update(cx, |menu, _| menu.selected_index), None);
    }

    /// A menu rebuilt while it is open, because what it lists changed, kept
    /// its items' identity but lost the highlighted one, so a user moving
    /// through it with the arrow keys was put back before its first item
    /// (elundqvist/kvist#316). The highlight follows the item's label to where
    /// it now stands, and goes where no item of that label can be chosen.
    #[gpui::test]
    fn rebuild_keeps_the_highlighted_item_by_its_label(cx: &mut gpui::TestAppContext) {
        cx.update(crate::init);
        let (menu, cx) =
            cx.add_window_view(|_, cx| items(PopupMenu::new(cx), &["Alpha", "Beta", "Gamma"]));
        let rebuild = |cx: &mut gpui::VisualTestContext, labels: &'static [&'static str]| {
            cx.update(|window, cx| {
                menu.update(cx, |menu, cx| {
                    menu.rebuild(window, cx, |menu, _, _| items(menu, labels));
                    menu.selected_index
                })
            })
        };

        menu.update(cx, |menu, cx| menu.set_selected_index(1, cx));
        assert_eq!(rebuild(cx, &["Alpha", "-", "Gamma", "Beta"]), Some(3));
        // a label twice: the one where it stood
        assert_eq!(rebuild(cx, &["Beta", "Alpha", "-", "Beta"]), Some(3));
        // one that can no longer be chosen, or is gone: nothing
        assert_eq!(rebuild(cx, &["Alpha", "~Beta", "Gamma"]), None);
        menu.update(cx, |menu, cx| menu.set_selected_index(2, cx));
        assert_eq!(rebuild(cx, &["Alpha", "Beta"]), None);
        // and with nothing highlighted, nothing is
        assert_eq!(rebuild(cx, &["Alpha", "Beta", "Gamma"]), None);
    }

    /// A menu that opens on a label, as the folder chip's on "Recent
    /// repositories", or on an item that cannot be chosen, takes its first
    /// Down to the first item that can, and Down at the last goes round to
    /// it: both went to the menu's first row, which showed nothing, so the
    /// first Down seemed lost (elundqvist/kvist#660).
    #[gpui::test]
    fn the_first_down_goes_to_the_first_item_that_can_be_chosen(cx: &mut gpui::TestAppContext) {
        cx.update(crate::init);
        let (menu, cx) = cx.add_window_view(|_, cx| {
            items(
                PopupMenu::new(cx).label("Recent"),
                &["~None", "Alpha", "Beta"],
            )
        });
        let down = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                menu.update(cx, |menu, cx| {
                    menu.select_down(&SelectDown, window, cx);
                    menu.selected_index
                })
            })
        };

        assert_eq!(down(cx), Some(2));
        assert_eq!(down(cx), Some(3));
        assert_eq!(down(cx), Some(2));
    }

    fn draw(cx: &mut gpui::VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
    }

    /// A window that shows a menu as a context menu does: at a point,
    /// 400 px down, kept inside the window's edges by its `anchored`, and
    /// as large as it is rather than as the window.
    struct Shown(Entity<PopupMenu>);

    impl Render for Shown {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                anchored()
                    .position(gpui::point(px(0.), px(400.)))
                    .snap_to_window_with_margin(px(8.))
                    .child(self.0.clone()),
            )
        }
    }

    /// `build`'s menu shown in a window of `size`.
    fn shown(
        cx: &mut gpui::TestAppContext,
        size: gpui::Size<Pixels>,
        build: impl FnOnce(&mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> (Entity<PopupMenu>, &mut gpui::VisualTestContext) {
        cx.update(crate::init);
        let (host, cx) = cx.add_window_view(|window, cx| Shown(cx.new(|cx| build(window, cx))));
        cx.simulate_resize(size);
        draw(cx);
        let menu = host.update(cx, |host, _| host.0.clone());
        (menu, cx)
    }

    /// A menu taller than its window keeps to the window, inside the margin
    /// a menu keeps from its edges, and scrolls: its last item, chosen with
    /// the keys, is scrolled into view. One wider than the window keeps to
    /// its width. A file's context menu of some 680 px ran past the bottom
    /// of a window 480 px high, its last rows out of reach
    /// (elundqvist/kvist#634). In a window that draws its own shadow the
    /// menu keeps that much further in, as its `anchored` does: on
    /// Wayland it stood the shadow lower than its room, its last row past
    /// the window's foot. Its bounds are its whole box, the padding in it.
    #[gpui::test]
    fn a_menu_keeps_to_a_small_window_and_scrolls(cx: &mut gpui::TestAppContext) {
        let (menu, cx) = shown(cx, gpui::size(px(240.), px(480.)), |_, cx| {
            let menu = PopupMenu::new(cx);
            (0..40).fold(menu, |menu, i| {
                menu.item(PopupMenuItem::new(format!(
                    "Item {i} of a menu with a long name"
                )))
            })
        });
        let bounds = menu.update(cx, |menu, _| menu.bounds);
        assert!(bounds.size.height <= px(464.), "{bounds:?}");
        assert!(bounds.size.height > px(400.), "{bounds:?}");
        assert!(bounds.size.width <= px(224.), "{bounds:?}");
        assert!(
            bounds.top() >= px(8.) && bounds.bottom() <= px(472.),
            "{bounds:?}"
        );
        // the menu's whole box, the items 4 px inside it on every side
        let items = cx.debug_bounds("popup-menu-items").expect("the items' box");
        assert_eq!(
            items.origin,
            bounds.origin + gpui::point(px(4.), px(4.)),
            "{items:?} in {bounds:?}"
        );
        assert_eq!(
            items.size + gpui::size(px(8.), px(8.)),
            bounds.size,
            "{items:?} in {bounds:?}"
        );

        // a window that draws its own shadow 20 px inside its edges
        cx.update(|window, _| window.set_client_inset(px(20.)));
        draw(cx);
        let inset = menu.update(cx, |menu, _| menu.bounds);
        assert!(inset.size.height <= px(424.), "{inset:?}");
        assert!(inset.size.height > px(360.), "{inset:?}");
        assert!(inset.size.width <= px(184.), "{inset:?}");
        assert!(
            inset.top() >= px(28.) && inset.bottom() <= px(452.),
            "past the window's foot: {inset:?}"
        );

        // each of the 40 rows its own 26 px and the gaps between them, not
        // squeezed into the room: the last is 600 px and more down
        menu.update(cx, |menu, cx| menu.set_selected_index(39, cx));
        draw(cx);
        let offset = menu.update(cx, |menu, _| menu.scroll_handle.offset());
        assert!(offset.y < px(-600.), "{offset:?}");
    }

    /// A menu lit on its checked item, with more items than fit, opens
    /// scrolled to it, and so does one rebuilt with the keyboard's item out
    /// of sight: asked before the items were laid out, the scroll was lost,
    /// and a select of 40 options with one near its end chosen opened at its
    /// top, no row lit to be seen (elundqvist/kvist#678).
    #[gpui::test]
    fn a_menu_lit_on_an_item_out_of_sight_opens_scrolled_to_it(cx: &mut gpui::TestAppContext) {
        let options = |menu: PopupMenu, checked: usize| {
            (0..40).fold(menu, |menu, i| {
                menu.item(PopupMenuItem::new(format!("Option {i}")).checked(i == checked))
            })
        };
        // opened in a window already drawn at its size
        cx.update(crate::init);
        let (host, cx) = cx.add_window_view(|_, cx| Shown(cx.new(|cx| PopupMenu::new(cx))));
        cx.simulate_resize(gpui::size(px(240.), px(480.)));
        draw(cx);
        let menu = host.update(cx, |host, cx| {
            host.0 = cx.new(|cx| options(PopupMenu::new(cx), 37).lit_checked());
            cx.notify();
            host.0.clone()
        });
        draw(cx);
        draw(cx);
        let in_sight = |cx: &mut gpui::VisualTestContext, ix: usize| {
            menu.update(cx, |menu, _| {
                let (offset, room) = (menu.scroll_handle.offset(), menu.scroll_handle.bounds());
                let item = menu.scroll_handle.bounds_for_item(ix).expect("the item laid out");
                let top = item.top() + offset.y;
                assert!(
                    top >= room.top() - px(0.5) && top + item.size.height <= room.bottom() + px(0.5),
                    "{item:?} scrolled by {offset:?} in {room:?}"
                );
                (menu.selected_index, offset.y)
            })
        };
        let (lit, offset) = in_sight(cx, 37);
        assert_eq!(lit, Some(37));
        assert!(offset < px(-600.), "{offset:?}");

        // scrolled back to the top, then rebuilt: the keyboard's item again
        menu.update(cx, |menu, _| menu.scroll_handle.set_offset(gpui::point(px(0.), px(0.))));
        cx.update(|window, cx| {
            menu.update(cx, |menu, cx| menu.rebuild(window, cx, |menu, _, _| options(menu, 37)))
        });
        draw(cx);
        assert_eq!(in_sight(cx, 37).0, Some(37));
    }

    /// A submenu at the foot of a menu that scrolls, scrolled to and opened,
    /// hangs outside the items' scroll, whose clip would cut it off, and
    /// takes a click: the submenus of a scrolling menu are drawn deferred,
    /// with no clip of the scroll's (elundqvist/kvist#634).
    #[gpui::test]
    fn a_submenu_of_a_menu_that_scrolls_hangs_outside_its_scroll(cx: &mut gpui::TestAppContext) {
        let clicked = Rc::new(std::cell::Cell::new(false));
        let (menu, cx) = shown(cx, gpui::size(px(640.), px(480.)), {
            let clicked = clicked.clone();
            move |window, cx| {
                let inner = PopupMenu::build(window, cx, move |menu, _, _| {
                    let clicked = clicked.clone();
                    menu.item(
                        PopupMenuItem::new("Inner").on_click(move |_, _, _| clicked.set(true)),
                    )
                });
                (0..39)
                    .fold(PopupMenu::new(cx), |menu, i| {
                        menu.item(PopupMenuItem::new(format!("Item {i}")))
                    })
                    .item(PopupMenuItem::submenu("More", inner))
            }
        });
        menu.update(cx, |menu, cx| menu.set_selected_index(39, cx));
        draw(cx);
        draw(cx);
        let offset = menu.update(cx, |menu, _| menu.scroll_handle.offset());
        assert!(
            offset.y < px(-600.),
            "the submenu's row was not scrolled to: {offset:?}"
        );
        // the menu's items 4 px inside its box, and their scroll's clip
        let bounds = menu.update(cx, |menu, _| menu.bounds);
        let clip = bounds.right() - px(4.);
        let inner = cx
            .debug_bounds("popup-menu-label:Inner")
            .expect("the submenu, drawn");
        assert!(
            inner.left() > clip,
            "{inner:?} inside the scroll of {bounds:?}"
        );
        assert!(
            inner.top() >= px(8.) && inner.bottom() <= px(472.),
            "{inner:?}"
        );

        cx.simulate_click(inner.center(), gpui::Modifiers::none());
        assert!(clicked.get(), "the submenu's item took no click");
    }

    /// A menu that fits keeps its own height and does not scroll, and a
    /// scrollable one with no height of its own keeps to half the window.
    /// With the menu's padding on its scroll, a menu that fits was 8 px
    /// short of its rows and scrolled by them.
    #[gpui::test]
    fn a_menu_that_fits_is_as_tall_as_its_items(cx: &mut gpui::TestAppContext) {
        let (menu, cx) = shown(cx, gpui::size(px(640.), px(480.)), |_, cx| {
            items(PopupMenu::new(cx), &["Alpha", "Beta", "Gamma"])
        });
        let few = menu.update(cx, |menu, _| menu.bounds.size.height);
        assert!(few < px(150.), "{few:?}");
        let scrolls = menu.update(cx, |menu, _| menu.scroll_handle.max_offset());
        assert_eq!(scrolls.y, px(0.), "{scrolls:?}");
        let room = |menu: &PopupMenu, inset: f32| {
            menu.room(gpui::size(px(640.), px(480.)), px(inset), None)
                .height
        };
        assert_eq!(menu.update(cx, |menu, _| room(menu, 0.)), px(464.));
        assert_eq!(menu.update(cx, |menu, _| room(menu, 20.)), px(424.));
        menu.update(cx, |menu, _| menu.scrollable = true);
        assert_eq!(menu.update(cx, |menu, _| room(menu, 0.)), px(240.));
        menu.update(cx, |menu, _| menu.max_height = Some(px(900.)));
        assert_eq!(menu.update(cx, |menu, _| room(menu, 0.)), px(464.));
    }

    /// A label longer than a narrow window has room for ends in an
    /// ellipsis on its one line, its row as high as any, and a submenu's
    /// chevron stays in its row: the boxes around the label were as wide as
    /// it, and pushed the chevron past the menu's edge.
    #[gpui::test]
    fn a_long_label_is_cut_short_in_its_row(cx: &mut gpui::TestAppContext) {
        const LONG: &str = "A label much longer than the room that a narrow window gives it";
        const SUB: &str = "A submenu label much longer than the room a narrow window gives it";
        let (menu, cx) = shown(cx, gpui::size(px(240.), px(480.)), |window, cx| {
            let sub = PopupMenu::build(window, cx, |menu, _, _| {
                menu.item(PopupMenuItem::new("Inner"))
            });
            PopupMenu::new(cx)
                .item(PopupMenuItem::new(LONG))
                .item(PopupMenuItem::submenu(SUB, sub))
        });
        let bounds = menu.update(cx, |menu, _| menu.bounds);
        assert!(bounds.size.width <= px(224.), "{bounds:?}");
        // two rows of 26 px, the gap between them and the padding round them
        assert!(bounds.size.height <= px(64.), "{bounds:?}");
        let inside = bounds.right() - px(4.) + px(0.5);
        let row = cx
            .debug_bounds("popup-menu-submenu-row")
            .expect("the submenu's row");
        assert!(row.right() <= inside, "{row:?} past {bounds:?}");
        for label in [LONG, SUB] {
            let at = cx
                .debug_bounds(format!("popup-menu-label:{label}").leak())
                .expect(label);
            assert!(at.right() <= inside, "{at:?} past {bounds:?}");
        }
        let sub = cx
            .debug_bounds(format!("popup-menu-label:{SUB}").leak())
            .unwrap();
        assert!(
            sub.right() < row.right() - px(8.),
            "no room left for the chevron: {sub:?} in {row:?}"
        );
    }

    /// The item the pointer rests on is the one a click takes, so where the
    /// pointer lit it the highlight stays on the row under the pointer as the
    /// items move, not on the label that moved away from it
    /// (elundqvist/kvist#316), and the pointer leaving that row still puts it
    /// out. Where the keyboard lit it last, the label is followed again.
    #[gpui::test]
    fn rebuild_keeps_the_pointer_s_highlight_where_the_pointer_is(cx: &mut gpui::TestAppContext) {
        cx.update(crate::init);
        let (menu, cx) =
            cx.add_window_view(|_, cx| items(PopupMenu::new(cx), &["Alpha", "Beta", "Gamma"]));
        let rebuild = |cx: &mut gpui::VisualTestContext, labels: &'static [&'static str]| {
            cx.update(|window, cx| {
                menu.update(cx, |menu, cx| {
                    menu.rebuild(window, cx, |menu, _, _| items(menu, labels));
                    menu.selected_index
                })
            })
        };

        menu.update(cx, |menu, cx| menu.hover_item(1, true, false, cx));
        assert_eq!(rebuild(cx, &["Restart", "Alpha", "Beta"]), Some(1));
        menu.update(cx, |menu, cx| menu.hover_item(1, false, false, cx));
        assert_eq!(menu.update(cx, |menu, _| menu.selected_index), None);
        // a row under the pointer that can no longer be chosen: nothing
        menu.update(cx, |menu, cx| menu.hover_item(1, true, false, cx));
        assert_eq!(rebuild(cx, &["Restart", "-", "Beta"]), None);
        // the pointer lit it, then the keyboard moved on: the label again
        assert_eq!(rebuild(cx, &["Alpha", "Beta", "Gamma"]), None);
        menu.update(cx, |menu, cx| {
            menu.hover_item(0, true, false, cx);
            menu.set_selected_index(1, cx);
        });
        assert_eq!(rebuild(cx, &["Restart", "Alpha", "Beta"]), Some(2));
    }

    /// A button `top` px down a window with a menu of 30 items, and the
    /// menu it last built.
    struct Tall {
        top: f32,
        menu: Rc<std::cell::RefCell<Option<WeakEntity<PopupMenu>>>>,
    }

    impl Render for Tall {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            use crate::menu::DropdownMenu as _;
            let menu = self.menu.clone();
            div().size_full().child(
                div().absolute().top(px(self.top)).left(px(20.)).child(
                    div().debug_selector(|| "tall-trigger".into()).child(
                        crate::button::Button::new("trigger")
                            .label("Open")
                            .dropdown_menu(move |popup, _, cx| {
                                *menu.borrow_mut() = Some(cx.entity().downgrade());
                                (0..30).fold(popup, |popup, i| {
                                    popup.item(PopupMenuItem::new(format!("Item {i}")))
                                })
                            }),
                    ),
                ),
            )
        }
    }

    /// A menu taller than the room on either side of its button goes on
    /// the side with more room, keeps to it and scrolls, as VS Code's
    /// does: it was held into the window over its own button, Settings'
    /// list of providers in a window 480 px high (elundqvist/kvist#665).
    #[gpui::test]
    fn a_menu_taller_than_the_room_on_both_sides_keeps_to_the_roomier(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            gpui_base::GlobalState::init(cx);
            cx.set_global(crate::Theme::default());
            crate::init(cx);
        });
        for (top, below) in [(150., true), (300., false)] {
            let menu: Rc<std::cell::RefCell<Option<WeakEntity<PopupMenu>>>> = Rc::default();
            let (_, cx) = cx.add_window_view({
                let menu = menu.clone();
                move |_, _| Tall { top, menu }
            });
            cx.simulate_resize(gpui::size(px(640.), px(480.)));
            let draw = |cx: &mut gpui::VisualTestContext| {
                cx.run_until_parked();
                cx.update(|window, cx| window.draw(cx).clear(cx));
            };
            draw(cx);
            let trigger = cx.debug_bounds("tall-trigger").expect("the button");
            cx.simulate_click(trigger.center(), Default::default());
            for _ in 0..3 {
                draw(cx);
            }
            let menu = menu
                .borrow()
                .clone()
                .and_then(|m| m.upgrade())
                .expect("the menu");
            let (bounds, scrolls) = menu.update(cx, |menu, _| {
                (menu.bounds, menu.scroll_handle.max_offset().y)
            });
            assert!(
                bounds.top() >= px(8.) && bounds.bottom() <= px(472.),
                "out of the window: {bounds:?}"
            );
            if below {
                assert!(
                    bounds.top() >= trigger.bottom(),
                    "{bounds:?} over {trigger:?}"
                );
            } else {
                assert!(
                    bounds.bottom() <= trigger.top(),
                    "{bounds:?} over {trigger:?}"
                );
            }
            assert!(scrolls > px(0.), "its list does not scroll: {bounds:?}");
        }
    }
}
