// Changed in elundqvist/gpui-kit from gpui-pre 0.3.3 as published to crates.io: FORK.md at the
// crate's root lists the changes.
use crate::{
    AnyElement, AnyEntity, AnyWeakEntity, App, Bounds, ContentMask, Context, Element, ElementId,
    Entity, EntityId, GlobalElementId, InspectorElementId, IntoElement, LayoutId, PaintIndex,
    Pixels, PrepaintStateIndex, Render, RenderOnce, Style, StyleRefinement, TextStyle, WeakEntity,
};
use crate::{Empty, Window};
use anyhow::Result;
use collections::FxHashSet;
use refineable::Refineable;
use std::{any::TypeId, fmt, ops::Range};

/// A dynamically-typed view handle that can be downcast to a specific `Entity<V>`.
///
/// This is the type-erased counterpart to [`ViewElement`]: it holds an entity plus
/// a function pointer to its render, and is itself a [`View`], so embedding it as an
/// element goes through the same [`ViewElement`] machinery as any other view.
#[derive(Clone, Debug)]
pub struct AnyView {
    entity: AnyEntity,
    render: fn(&AnyView, &mut Window, &mut App) -> AnyElement,
}

impl<V: Render> From<Entity<V>> for AnyView {
    fn from(value: Entity<V>) -> Self {
        AnyView {
            entity: value.into_any(),
            render: any_view::render::<V>,
        }
    }
}

impl AnyView {
    /// Embed this view as a cached [`ViewElement`] laid out at `style`.
    ///
    /// The rendered subtree is recycled from the previous frame unless
    /// [Context::notify] was called on the backing entity since it was rendered
    /// (or [Window::refresh] is called, which ignores caching).
    pub fn cached(self, style: StyleRefinement) -> ViewElement<AnyView> {
        ViewElement::new(self).cached(style)
    }

    /// Convert this to a weak handle.
    pub fn downgrade(&self) -> AnyWeakView {
        AnyWeakView {
            entity: self.entity.downgrade(),
            render: self.render,
        }
    }

    /// Convert this to a [Entity] of a specific type.
    /// If this handle does not contain a view of the specified type, returns itself in an `Err` variant.
    pub fn downcast<T: 'static>(self) -> Result<Entity<T>, Self> {
        match self.entity.downcast() {
            Ok(entity) => Ok(entity),
            Err(entity) => Err(Self {
                entity,
                render: self.render,
            }),
        }
    }

    /// Gets the [TypeId] of the underlying view.
    pub fn entity_type(&self) -> TypeId {
        self.entity.entity_type
    }

    /// The [`EntityId`] of this view.
    pub fn entity_id(&self) -> EntityId {
        self.entity.entity_id()
    }
}

impl PartialEq for AnyView {
    fn eq(&self, other: &Self) -> bool {
        self.entity == other.entity
    }
}

impl Eq for AnyView {}

/// `AnyView` is the type-erased [`View`]: its `render` is a function pointer rather
/// than a concrete type, but it participates in the reactive graph exactly like any
/// other view via [`ViewElement`].
impl View for AnyView {
    fn entity_id(&self) -> Option<EntityId> {
        Some(self.entity.entity_id())
    }

    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        (self.render)(&self, window, cx)
    }
}

impl<V: 'static + Render> IntoElement for Entity<V> {
    type Element = ViewElement<Entity<V>>;

    fn into_element(self) -> Self::Element {
        ViewElement::new(self)
    }
}

impl IntoElement for AnyView {
    type Element = ViewElement<AnyView>;

    fn into_element(self) -> Self::Element {
        ViewElement::new(self)
    }
}

/// A weak, dynamically-typed view handle.
pub struct AnyWeakView {
    entity: AnyWeakEntity,
    render: fn(&AnyView, &mut Window, &mut App) -> AnyElement,
}

impl AnyWeakView {
    /// Upgrade to a strong `AnyView` handle, if the view is still alive.
    pub fn upgrade(&self) -> Option<AnyView> {
        let entity = self.entity.upgrade()?;
        Some(AnyView {
            entity,
            render: self.render,
        })
    }
}

impl<V: 'static + Render> From<WeakEntity<V>> for AnyWeakView {
    fn from(view: WeakEntity<V>) -> Self {
        AnyWeakView {
            entity: view.into(),
            render: any_view::render::<V>,
        }
    }
}

impl PartialEq for AnyWeakView {
    fn eq(&self, other: &Self) -> bool {
        self.entity == other.entity
    }
}

impl std::fmt::Debug for AnyWeakView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnyWeakView")
            .field("entity_id", &self.entity.entity_id)
            .finish_non_exhaustive()
    }
}

mod any_view {
    use crate::{AnyElement, AnyView, App, IntoElement, Render, Window};

    pub(crate) fn render<V: 'static + Render>(
        view: &AnyView,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let view = view.clone().downcast::<V>().unwrap();
        // Record the view's Render type name so the accessibility debug dump can
        // attribute nodes to the view that produced them.
        #[cfg(debug_assertions)]
        window
            .a11y
            .view_type_names
            .insert(view.entity_id(), std::any::type_name::<V>());
        view.update(cx, |view, cx| view.render(window, cx).into_any_element())
    }
}

/// A renderable that participates in GPUI's reactive graph — the unifying model
/// behind [`Render`] and [`RenderOnce`].
///
/// When `entity_id()` returns `Some`, that id becomes the view's identity: it gets
/// a unique element-id space (so internal `use_state` / `.id(..)` never collide
/// across siblings) and `cx.notify()` on that entity re-renders only this view's
/// subtree. `None` behaves like a stateless component.
///
/// You rarely implement `View` directly. `Entity<T: Render>` and any `T: RenderOnce`
/// get a blanket impl below; implement it by hand only when a component needs both
/// parent-supplied props *and* a backing entity for identity.
pub trait View: 'static + Sized {
    /// This view's identity, if it has one. A view typically holds the backing
    /// entity as a field and returns its [`EntityId`] here.
    ///
    /// The id becomes this view's [`ElementId`], so two views keyed on the same
    /// entity must not be rendered at the same position in the element tree
    /// (e.g. as siblings under the same parent): their internal element state
    /// (`use_state`, scroll offsets, etc.) would silently collide. Nesting is
    /// fine — the id is scoped by the parent path.
    fn entity_id(&self) -> Option<EntityId>;

    /// Render this view into an element tree, consuming `self`.
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement;
}

/// A stateless component (`RenderOnce`) is a `View` with no identity.
impl<T: RenderOnce> View for T {
    fn entity_id(&self) -> Option<EntityId> {
        None
    }

    #[inline]
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        RenderOnce::render(self, window, cx)
    }
}

/// An entity that renders itself (`Render`) is a `View` keyed on its own id.
impl<T: Render> View for Entity<T> {
    fn entity_id(&self) -> Option<EntityId> {
        Some(Entity::entity_id(self))
    }

    #[inline]
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        self.update(cx, |this, cx| {
            Render::render(this, window, cx).into_any_element()
        })
    }
}

impl<T: Render> Entity<T> {
    /// Embed this entity as a cached [`ViewElement`] laid out at `style`.
    ///
    /// The rendered subtree is reused until the entity is notified (or the
    /// cached bounds / text style change). Caching requires a definite size:
    /// a cached view is laid out from `style` and is *not* measured from its
    /// contents. Use [`ViewElement::new`] (or `.child(entity)`) for the
    /// uncached case.
    #[track_caller]
    pub fn cached(self, style: StyleRefinement) -> ViewElement<Entity<T>> {
        ViewElement::new(self).cached(style)
    }
}

/// The element type for [`View`] implementations. Wraps a `View` and hooks it
/// into layout, prepaint, and paint. Constructed via [`ViewElement::new`].
#[doc(hidden)]
pub struct ViewElement<V: View> {
    view: Option<V>,
    entity_id: Option<EntityId>,
    cached_style: Option<StyleRefinement>,
    #[cfg(debug_assertions)]
    source: &'static core::panic::Location<'static>,
}

impl<V: View> ViewElement<V> {
    /// Wrap a [`View`] as an element.
    #[track_caller]
    pub fn new(view: V) -> Self {
        let entity_id = view.entity_id();
        ViewElement {
            entity_id,
            cached_style: None,
            view: Some(view),
            #[cfg(debug_assertions)]
            source: core::panic::Location::caller(),
        }
    }

    /// Enable caching of this view's rendered subtree, laid out at `style`.
    /// The composer supplies the layout style because caching skips rendering
    /// the contents to measure them.
    ///
    /// Crate-private on purpose: caching is only sound for entity-backed views,
    /// where [`Context::notify`] is the contract that busts the cache. A stateless
    /// view has no such contract, so a frozen subtree could never be invalidated.
    /// Reach this through [`Entity::cached`] or [`AnyView::cached`], which are
    /// entity-backed by construction.
    pub(crate) fn cached(mut self, style: StyleRefinement) -> Self {
        self.cached_style = Some(style);
        self
    }
}

impl<V: View> IntoElement for ViewElement<V> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

pub(crate) struct ViewElementState {
    prepaint_range: Range<PrepaintStateIndex>,
    paint_range: Range<PaintIndex>,
    /// The frames the two ranges index, by the window's count of its draws
    /// (`Window::draws`): a view is taken from the frame drawn last only
    /// where both its ranges index that frame. The paint's is 0, no frame,
    /// until the view is painted: a view prepainted and not painted, under
    /// an element that is hidden, has no paint range to take.
    prepaint_drawn: u64,
    paint_drawn: u64,
    cache_key: ViewElementCacheKey,
    accessed_entities: FxHashSet<EntityId>,
}

impl ViewElementState {
    /// A cached view inside a part of the frame drawn last that is taken
    /// into the frame being drawn (`Window::reuse_prepaint`) goes with it:
    /// its range is moved to where that part lands, `from` being where the
    /// part began in the frame drawn last and `to` where it begins now, so
    /// that the view can be taken from this frame in the next, inside a
    /// parent drawn anew (elundqvist/kvist#611). A range that does not
    /// index the frame drawn last is left, and the view drawn anew.
    pub(crate) fn carry_prepaint(&mut self, from: &PrepaintStateIndex, to: &PrepaintStateIndex, drawn: u64) {
        if self.prepaint_drawn + 1 == drawn {
            self.prepaint_range = self.prepaint_range.start.moved(from, to)..self.prepaint_range.end.moved(from, to);
            self.prepaint_drawn = drawn;
        }
    }

    /// The paint's range, as `carry_prepaint` moves the prepaint's
    /// (`Window::reuse_paint`).
    pub(crate) fn carry_paint(&mut self, from: &PaintIndex, to: &PaintIndex, drawn: u64) {
        if self.paint_drawn + 1 == drawn {
            self.paint_range = self.paint_range.start.moved(from, to)..self.paint_range.end.moved(from, to);
            self.paint_drawn = drawn;
        }
    }
}

struct ViewElementCacheKey {
    bounds: Bounds<Pixels>,
    content_mask: ContentMask<Pixels>,
    text_style: TextStyle,
}

impl<V: View> Element for ViewElement<V> {
    type RequestLayoutState = Option<AnyElement>;
    type PrepaintState = Option<AnyElement>;

    fn id(&self) -> Option<ElementId> {
        self.entity_id.map(ElementId::View)
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        #[cfg(debug_assertions)]
        return Some(self.source);

        #[cfg(not(debug_assertions))]
        return None;
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        if let Some(entity_id) = self.entity_id {
            // Stateful path: create a reactive boundary.
            window.with_rendered_view(entity_id, |window| {
                let caching_disabled = window.is_inspector_picking(cx);
                match self.cached_style.as_ref() {
                    Some(style) if !caching_disabled => {
                        let mut root_style = Style::default();
                        root_style.refine(style);
                        let layout_id = window.request_layout(root_style, None, cx);
                        (layout_id, None)
                    }
                    _ => {
                        let mut element = self
                            .view
                            .take()
                            .unwrap()
                            .render(window, cx)
                            .into_any_element();
                        let layout_id = element.request_layout(window, cx);
                        (layout_id, Some(element))
                    }
                }
            })
        } else {
            // Stateless path: isolate subtree via type name (no entity identity).
            window.with_id(
                ElementId::Name(std::any::type_name::<V>().into()),
                |window| {
                    let mut element = self
                        .view
                        .take()
                        .unwrap()
                        .render(window, cx)
                        .into_any_element();
                    let layout_id = element.request_layout(window, cx);
                    (layout_id, Some(element))
                },
            )
        }
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        element: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        if let Some(entity_id) = self.entity_id {
            // Stateful path.
            window.set_view_id(entity_id);
            window.with_rendered_view(entity_id, |window| {
                if let Some(mut element) = element.take() {
                    element.prepaint(window, cx);
                    return Some(element);
                }

                let global_id = global_id.unwrap();
                window.with_view_state(global_id, |element_state, window| {
                    let content_mask = window.content_mask();
                    let text_style = window.text_style();

                    if let Some(mut element_state) = element_state
                        && element_state.cache_key.bounds == bounds
                        && element_state.cache_key.content_mask == content_mask
                        && element_state.cache_key.text_style == text_style
                        && !window.dirty_views.contains(&entity_id)
                        && !window.refreshing
                        && element_state.prepaint_drawn + 1 == window.draws
                        && element_state.paint_drawn + 1 == window.draws
                    {
                        let prepaint_start = window.prepaint_index();
                        window.reuse_view_element_states(global_id);
                        window.reuse_prepaint(element_state.prepaint_range.clone());
                        cx.entities
                            .extend_accessed(&element_state.accessed_entities);
                        let prepaint_end = window.prepaint_index();
                        element_state.prepaint_range = prepaint_start..prepaint_end;
                        element_state.prepaint_drawn = window.draws;

                        return (None, element_state);
                    }

                    // drawn anew, it draws anew only those of the cached
                    // views inside it that changed: the rest are taken
                    // from the frame drawn last, where upstream had the
                    // window refresh them all (elundqvist/kvist#611)
                    let prepaint_start = window.prepaint_index();
                    let (mut element, accessed_entities) = cx.detect_accessed_entities(|cx| {
                        window.with_view_element_states(Some(global_id), |window| {
                            let mut element = self
                                .view
                                .take()
                                .unwrap()
                                .render(window, cx)
                                .into_any_element();
                            element.layout_as_root(bounds.size.into(), window, cx);
                            element.prepaint_at(bounds.origin, window, cx);
                            element
                        })
                    });

                    let prepaint_end = window.prepaint_index();

                    (
                        Some(element),
                        ViewElementState {
                            accessed_entities,
                            prepaint_range: prepaint_start..prepaint_end,
                            paint_range: PaintIndex::default()..PaintIndex::default(),
                            prepaint_drawn: window.draws,
                            paint_drawn: 0,
                            cache_key: ViewElementCacheKey {
                                bounds,
                                content_mask,
                                text_style,
                            },
                        },
                    )
                })
            })
        } else {
            // Stateless path: just prepaint the element.
            window.with_id(
                ElementId::Name(std::any::type_name::<V>().into()),
                |window| {
                    element.as_mut().unwrap().prepaint(window, cx);
                },
            );
            Some(element.take().unwrap())
        }
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        element: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(entity_id) = self.entity_id {
            // Stateful path.
            window.with_rendered_view(entity_id, |window| {
                let caching_disabled = window.is_inspector_picking(cx);
                if self.cached_style.is_some() && !caching_disabled {
                    let global_id = global_id.unwrap();
                    window.with_view_state(global_id, |element_state, window| {
                        let mut element_state = element_state.unwrap();

                        let paint_start = window.paint_index();

                        if let Some(element) = element {
                            window.with_view_element_states(Some(global_id), |window| {
                                element.paint(window, cx)
                            });
                        } else {
                            debug_assert_eq!(element_state.paint_drawn + 1, window.draws);
                            window.reuse_paint(element_state.paint_range.clone());
                        }

                        let paint_end = window.paint_index();
                        element_state.paint_range = paint_start..paint_end;
                        element_state.paint_drawn = window.draws;

                        ((), element_state)
                    })
                } else {
                    element.as_mut().unwrap().paint(window, cx);
                }
            });
        } else {
            // Stateless path: just paint the element.
            window.with_id(
                ElementId::Name(std::any::type_name::<V>().into()),
                |window| {
                    element.as_mut().unwrap().paint(window, cx);
                },
            );
        }
    }
}

/// A view that renders nothing
pub struct EmptyView;

impl Render for EmptyView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        AnyElement, App, Background, Bounds, Context, Element, ElementId, Entity, GlobalElementId,
        InspectorElementId, LayoutId, Modifiers, Pixels, Style, StyleRefinement, TestAppContext,
        Window, deferred, div, point, prelude::*, px, relative, rgb,
    };
    use std::{
        cell::{Cell, RefCell},
        rc::{Rc, Weak},
    };

    const INNER: u32 = 0x123456;

    /// A cached view: a box of its own colour that counts how often it was
    /// drawn and clicked.
    struct Inner {
        drawn: Rc<Cell<usize>>,
        clicks: Rc<Cell<usize>>,
    }

    impl Render for Inner {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.drawn.set(self.drawn.get() + 1);
            let clicks = self.clicks.clone();
            div()
                .id("inner")
                .size_full()
                .bg(rgb(INNER))
                .on_click(move |_, _, _| clicks.set(clicks.get() + 1))
        }
    }

    /// A cached view around it, as Kvist docks a drawer whose parts are
    /// cached views of their own.
    struct Outer {
        inner: Entity<Inner>,
        drawn: Rc<Cell<usize>>,
    }

    impl Render for Outer {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.drawn.set(self.drawn.get() + 1);
            div()
                .size_full()
                .child(self.inner.clone().cached(StyleRefinement::default().size_full()))
        }
    }

    /// The window's root: `before` boxes that take clicks, drawn ahead of
    /// the cached view and away from it, so that as their count changes the
    /// view's part of the frame begins elsewhere in it, in the same place
    /// on screen.
    struct Root {
        outer: Entity<Outer>,
        before: usize,
    }

    impl Render for Root {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .relative()
                .size_full()
                .child(
                    div().absolute().top(px(200.)).left_0().w(px(100.)).children(
                        (0..self.before).map(|i| {
                            div().id(i).h(px(10.)).w_full().bg(rgb(0xff0000)).on_click(|_, _, _| {})
                        }),
                    ),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .w(px(100.))
                        .h(px(100.))
                        .child(self.outer.clone().cached(StyleRefinement::default().size_full())),
                )
        }
    }

    struct Counts {
        outer: Rc<Cell<usize>>,
        inner: Rc<Cell<usize>>,
        clicks: Rc<Cell<usize>>,
    }

    fn open(cx: &mut TestAppContext) -> (Entity<Root>, &mut crate::VisualTestContext, Counts) {
        let counts = Counts {
            outer: Rc::default(),
            inner: Rc::default(),
            clicks: Rc::default(),
        };
        let (outer_drawn, inner_drawn, clicks) =
            (counts.outer.clone(), counts.inner.clone(), counts.clicks.clone());
        let (root, cx) = cx.add_window_view(move |_, cx| {
            let inner = cx.new(|_| Inner {
                drawn: inner_drawn,
                clicks,
            });
            let outer = cx.new(|_| Outer {
                inner,
                drawn: outer_drawn,
            });
            Root { outer, before: 0 }
        });
        (root, cx, counts)
    }

    fn quads_of_inner(cx: &mut crate::VisualTestContext) -> usize {
        cx.update(|window, _| {
            window
                .rendered_frame
                .scene
                .quads
                .iter()
                .filter(|q| q.background == Background::from(rgb(INNER)))
                .count()
        })
    }

    /// A cached view drawn again draws again only those of the cached views
    /// inside it that changed: upstream had the window refresh every one of
    /// them, so a drawer's parts were laid out with each of its frames
    /// (elundqvist/kvist#611).
    #[gpui::test]
    fn a_cached_view_inside_one_drawn_again_is_taken_from_the_frame_before(cx: &mut TestAppContext) {
        let (root, cx, counts) = open(cx);
        assert_eq!((counts.outer.get(), counts.inner.get()), (1, 1));

        let outer = root.read_with(cx, |root, _| root.outer.clone());
        outer.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!((counts.outer.get(), counts.inner.get()), (2, 1));
        assert_eq!(quads_of_inner(cx), 1);

        let inner = outer.read_with(cx, |outer, _| outer.inner.clone());
        inner.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!((counts.outer.get(), counts.inner.get()), (3, 2));

        // the whole window drawn anew draws every cached view anew
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert_eq!((counts.outer.get(), counts.inner.get()), (4, 3));
    }

    /// A cached view taken whole from the frame before carries the ranges
    /// of the cached views inside it to where its part lands in the new
    /// frame: taken from there in the next, inside it drawn anew, the inner
    /// view still has its box and takes its clicks, where it took what lay
    /// at its old place, the boxes drawn ahead of it now.
    #[gpui::test]
    fn a_cached_view_inside_one_taken_whole_keeps_its_part_of_the_frame(cx: &mut TestAppContext) {
        let (root, cx, counts) = open(cx);
        let outer = root.read_with(cx, |root, _| root.outer.clone());

        // the root drawn anew with boxes ahead of the views, which are taken
        // whole from the frame before, and further on in this one
        for before in [3, 1, 6] {
            root.update(cx, |root, cx| {
                root.before = before;
                cx.notify();
            });
            cx.run_until_parked();
            assert_eq!((counts.outer.get(), counts.inner.get()), (1, 1));
        }

        // the outer view drawn anew, the inner taken from the frame before
        outer.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!((counts.outer.get(), counts.inner.get()), (2, 1));
        assert_eq!(quads_of_inner(cx), 1);
        cx.simulate_click(point(px(50.), px(50.)), Modifiers::default());
        assert_eq!(counts.clicks.get(), 1);

        // and again, with the boxes moved once more in between; the click
        // drew the window whole, every view anew
        let drawn = (counts.outer.get(), counts.inner.get());
        root.update(cx, |root, cx| {
            root.before = 2;
            cx.notify();
        });
        cx.run_until_parked();
        outer.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!((counts.outer.get(), counts.inner.get()), (drawn.0 + 1, drawn.1));
        assert_eq!(quads_of_inner(cx), 1);
        cx.simulate_click(point(px(50.), px(50.)), Modifiers::default());
        assert_eq!(counts.clicks.get(), 2);
    }

    /// A view prepainted and not painted, in a div that is hidden, has no
    /// part of the frame to take for its paint: shown again, it is drawn
    /// anew, and draws its box, where taken from the frame it was last
    /// prepainted in it drew nothing, or took what lay at the place of a
    /// part of another frame.
    struct Hideable {
        inner: Entity<Inner>,
        shown: bool,
    }

    impl Render for Hideable {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().relative().size_full().child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w(px(100.))
                    .h(px(100.))
                    .when(!self.shown, |div| div.invisible())
                    .child(self.inner.clone().cached(StyleRefinement::default().size_full())),
            )
        }
    }

    fn show(hideable: &Entity<Hideable>, shown: bool, cx: &mut crate::VisualTestContext) {
        hideable.update(cx, |hideable, cx| {
            hideable.shown = shown;
            cx.notify();
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn a_cached_view_shown_again_after_it_was_hidden_is_drawn_anew(cx: &mut TestAppContext) {
        let (drawn, clicks) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let (inner_drawn, inner_clicks) = (drawn.clone(), clicks.clone());
        // hidden when first drawn: prepainted, never painted
        let (hideable, cx) = cx.add_window_view(move |_, cx| Hideable {
            inner: cx.new(|_| Inner {
                drawn: inner_drawn,
                clicks: inner_clicks,
            }),
            shown: false,
        });
        assert_eq!(quads_of_inner(cx), 0);

        show(&hideable, true, cx);
        assert_eq!(quads_of_inner(cx), 1);
        cx.simulate_click(point(px(50.), px(50.)), Modifiers::default());
        assert_eq!(clicks.get(), 1);

        // painted, then prepainted taken whole and not painted, then shown
        show(&hideable, false, cx);
        assert_eq!(quads_of_inner(cx), 0);
        show(&hideable, true, cx);
        assert_eq!(quads_of_inner(cx), 1);
        cx.simulate_click(point(px(50.), px(50.)), Modifiers::default());
        assert_eq!(clicks.get(), 2);
        assert!(drawn.get() >= 3);
    }

    /// An element that prepaints a cached view twice, the first try given up
    /// as `List` gives up one to scroll, and the second kept.
    struct Retried {
        view: Entity<Inner>,
    }

    impl IntoElement for Retried {
        type Element = Self;

        fn into_element(self) -> Self {
            self
        }
    }

    impl Element for Retried {
        type RequestLayoutState = ();
        type PrepaintState = AnyElement;

        fn id(&self) -> Option<ElementId> {
            None
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
        ) -> (LayoutId, ()) {
            let mut style = Style::default();
            style.size.width = relative(1.).into();
            style.size.height = relative(1.).into();
            (window.request_layout(style, None, cx), ())
        }

        fn prepaint(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            bounds: Bounds<Pixels>,
            _: &mut (),
            window: &mut Window,
            cx: &mut App,
        ) -> AnyElement {
            let prepaint = |window: &mut Window, cx: &mut App| {
                let mut view = self
                    .view
                    .clone()
                    .cached(StyleRefinement::default().size_full())
                    .into_any_element();
                view.layout_as_root(bounds.size.into(), window, cx);
                view.prepaint_at(bounds.origin, window, cx);
                view
            };
            let _ = window.transact(|window| {
                prepaint(window, cx);
                Err::<(), ()>(())
            });
            prepaint(window, cx)
        }

        fn paint(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            _: Bounds<Pixels>,
            _: &mut (),
            view: &mut AnyElement,
            window: &mut Window,
            cx: &mut App,
        ) {
            view.paint(window, cx);
        }
    }

    struct RetryRoot {
        inner: Entity<Inner>,
        before: usize,
    }

    impl Render for RetryRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .relative()
                .size_full()
                .child(
                    div().absolute().top(px(200.)).left_0().w(px(100.)).children(
                        (0..self.before).map(|i| {
                            div().id(i).h(px(10.)).w_full().bg(rgb(0xff0000)).on_click(|_, _, _| {})
                        }),
                    ),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .w(px(100.))
                        .h(px(100.))
                        .child(Retried {
                            view: self.inner.clone(),
                        }),
                )
        }
    }

    /// A prepaint begun again in `Window::transact` draws a cached view
    /// anew, where the view's ranges index the try that was given up and not
    /// the frame drawn last: taken from them, it would have taken what lay at
    /// those places of the frame before, or nothing there.
    #[gpui::test]
    fn a_cached_view_prepainted_again_in_a_transaction_is_not_taken_from_the_try_given_up(
        cx: &mut TestAppContext,
    ) {
        let (drawn, clicks) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let (inner_drawn, inner_clicks) = (drawn.clone(), clicks.clone());
        let (root, cx) = cx.add_window_view(move |_, cx| RetryRoot {
            inner: cx.new(|_| Inner {
                drawn: inner_drawn,
                clicks: inner_clicks,
            }),
            before: 0,
        });
        assert_eq!(quads_of_inner(cx), 1);

        // the boxes ahead of the view change in number, so that its ranges
        // in a try are not those it had in the frame before
        for (i, before) in [3, 1, 6, 6].into_iter().enumerate() {
            let was = drawn.get();
            root.update(cx, |root, cx| {
                root.before = before;
                cx.notify();
            });
            cx.run_until_parked();
            // the first try takes the view from the frame before, the
            // second draws it anew
            assert_eq!(drawn.get(), was + 1, "round {i}");
            assert_eq!(quads_of_inner(cx), 1, "round {i}");
            cx.simulate_click(point(px(50.), px(50.)), Modifiers::default());
            assert_eq!(clicks.get(), i + 1, "round {i}");
        }
    }
    /// An element that keeps a state of its own from its prepaint on, and
    /// counts how often it had to make one.
    struct Kept {
        made: Rc<Cell<usize>>,
        alive: Rc<RefCell<Weak<()>>>,
    }

    impl IntoElement for Kept {
        type Element = Self;

        fn into_element(self) -> Self {
            self
        }
    }

    impl Element for Kept {
        type RequestLayoutState = ();
        type PrepaintState = ();

        fn id(&self) -> Option<ElementId> {
            Some("kept".into())
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
        ) -> (LayoutId, ()) {
            let mut style = Style::default();
            style.size.width = px(10.).into();
            style.size.height = px(10.).into();
            (window.request_layout(style, None, cx), ())
        }

        fn prepaint(
            &mut self,
            id: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            _: Bounds<Pixels>,
            _: &mut (),
            window: &mut Window,
            _: &mut App,
        ) {
            window.with_element_state(id.unwrap(), |state: Option<Rc<()>>, _| {
                let state = state.unwrap_or_else(|| {
                    self.made.set(self.made.get() + 1);
                    let state = Rc::new(());
                    *self.alive.borrow_mut() = Rc::downgrade(&state);
                    state
                });
                ((), state)
            })
        }

        fn paint(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            _: Bounds<Pixels>,
            _: &mut (),
            _: &mut (),
            _: &mut Window,
            _: &mut App,
        ) {
        }
    }

    /// A cached view that draws a `Kept`, in its tree or deferred.
    struct Keeper {
        made: Rc<Cell<usize>>,
        alive: Rc<RefCell<Weak<()>>>,
        deferred: bool,
        shown: bool,
    }

    impl Render for Keeper {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let kept = Kept {
                made: self.made.clone(),
                alive: self.alive.clone(),
            };
            div()
                .size_full()
                .when(self.shown && !self.deferred, |div| div.child(kept))
                .when(self.shown && self.deferred, |div| {
                    div.child(deferred(Kept {
                        made: self.made.clone(),
                        alive: self.alive.clone(),
                    }))
                })
        }
    }

    /// A cached view around it.
    struct AroundKeeper {
        keeper: Entity<Keeper>,
    }

    impl Render for AroundKeeper {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                self.keeper
                    .clone()
                    .cached(StyleRefinement::default().size_full()),
            )
        }
    }

    struct KeeperRoot {
        around: Entity<AroundKeeper>,
        before: usize,
    }

    impl Render for KeeperRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .relative()
                .size_full()
                .child(
                    div()
                        .absolute()
                        .top(px(200.))
                        .left_0()
                        .w(px(100.))
                        .children((0..self.before).map(|i| div().id(i).h(px(10.)).w_full())),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .w(px(100.))
                        .h(px(100.))
                        .child(
                            self.around
                                .clone()
                                .cached(StyleRefinement::default().size_full()),
                        ),
                )
        }
    }

    /// The element states of a cached view are taken whole into a frame
    /// that takes the view whole, its own, or its parent's that holds it,
    /// where gpui moved them one by one (elundqvist/kvist#620): an element
    /// inside it, in its tree or deferred, keeps its state through frames
    /// that take the view whole and finds it once the view is drawn anew,
    /// and the state goes once the view drawn anew no longer draws it.
    fn kept_across_reused_views(deferred: bool, cx: &mut TestAppContext) {
        let made = Rc::new(Cell::new(0));
        let alive = Rc::new(RefCell::new(Weak::new()));
        let (keeper_made, keeper_alive) = (made.clone(), alive.clone());
        let (root, cx) = cx.add_window_view(move |_, cx| {
            let keeper = cx.new(|_| Keeper {
                made: keeper_made,
                alive: keeper_alive,
                deferred,
                shown: true,
            });
            KeeperRoot {
                around: cx.new(|_| AroundKeeper { keeper }),
                before: 0,
            }
        });
        assert_eq!(made.get(), 1);
        let around = root.read_with(cx, |root, _| root.around.clone());
        let keeper = around.read_with(cx, |around, _| around.keeper.clone());
        // the root drawn anew, both views taken whole, further on each time
        for before in [3, 1, 6] {
            root.update(cx, |root, cx| {
                root.before = before;
                cx.notify();
            });
            cx.run_until_parked();
        }
        // the view around it drawn anew, the keeper taken whole inside it
        around.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        root.update(cx, |root, cx| {
            root.before = 2;
            cx.notify();
        });
        cx.run_until_parked();
        // and the keeper drawn anew finds the state it had
        keeper.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(made.get(), 1, "deferred: {deferred}");
        assert!(alive.borrow().upgrade().is_some(), "deferred: {deferred}");

        // the whole window drawn anew keeps it too
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert_eq!(made.get(), 1, "deferred: {deferred}");

        // drawn anew without it, the keeper lets it go
        keeper.update(cx, |keeper, cx| {
            keeper.shown = false;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(alive.borrow().upgrade().is_none(), "deferred: {deferred}");
        keeper.update(cx, |keeper, cx| {
            keeper.shown = true;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(made.get(), 2, "deferred: {deferred}");
    }

    #[gpui::test]
    fn an_element_inside_a_cached_view_keeps_its_state_while_the_view_is_taken_whole(
        cx: &mut TestAppContext,
    ) {
        kept_across_reused_views(false, cx);
    }

    #[gpui::test]
    fn a_deferred_element_inside_a_cached_view_keeps_its_state_while_the_view_is_taken_whole(
        cx: &mut TestAppContext,
    ) {
        kept_across_reused_views(true, cx);
    }
}
