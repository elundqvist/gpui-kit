//! A text input field that allows the user to enter text.
//!
//! Based on the `Input` example from the `gpui` crate.
//! https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/input.rs
use gpui::TextAlign;
use gpui::{
    Action, App, AppContext, Bounds, ClipboardItem, Context, Edges, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyBinding,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _,
    Pixels, Point, Render, ScrollHandle, ScrollWheelEvent, SharedString, Styled as _, Subscription,
    UTF16Selection, Window, actions, div, point, prelude::FluentBuilder as _, px,
};
use ropey::{Rope, RopeSlice};
use serde::Deserialize;
use std::borrow::Cow;
use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use sum_tree::Bias;
use unicode_segmentation::*;

use super::{
    DiagnosticSet, DisplayMap, InputContextMenuCapabilities, InputEditorStyle,
    InputHighlighterFactory, MASK_CHAR, MaskPattern, NativeMenu, NumberStep, WrappingIndent,
    blink_cursor::BlinkCursor,
    change::Change,
    element::{EditorScrollbar, EditorScrollbarSnapshot, TextElement},
    kind::InputModeKind,
    mask_pattern::normalize_number_input,
    mode::LayoutMode,
    undo_manager::{EditIntent, UndoManager},
};
use crate::actions::{SelectDown, SelectLeft, SelectRight, SelectUp};
use crate::input::blink_cursor::CURSOR_WIDTH;
use crate::input::movement::MoveDirection;
use crate::input::{
    InputExtras as _, Position, RopeExt as _, Selection, element::RIGHT_MARGIN, layout::LastLayout,
};
use crate::{AutoScroll, StepAction};

/// A value read off the UI thread for
/// [`InputBaseState::set_prepared_value`]: the rope of a text, and its
/// lines as an input that does not soft-wrap lays them out. Making one
/// reads the whole text, which for a large one takes longer than a frame;
/// handing it to the input does not.
pub struct PreparedValue {
    text: Rope,
    lines: super::display_map::PreparedLines,
}

impl PreparedValue {
    /// Read `text`, on whatever thread the caller likes.
    pub fn new(text: &str) -> Self {
        let text = Rope::from(text);
        let lines = super::display_map::PreparedLines::of(&text);
        Self { text, lines }
    }
}

/// How the application holds a window's keyboard input back while a paste
/// waits for the clipboard, and gives it back once the paste has landed.
///
/// A paste reads the clipboard on the drawing thread unless one is set
/// ([`PasteInputHold::set`]): read off it, the keys typed after the paste
/// would come before it. The gpui this crate builds on has no way to hold
/// a window's input; one that has, such as elundqvist/gpui-kit's
/// `Window::hold_input` (elundqvist/kvist#401), is handed in here:
/// `hold` holds the window's input and gives back what releases it, and
/// `release` is handed that once the paste is done; when the window has
/// gone first, it is dropped instead. One whose window hands it the keys
/// pressed while it holds (`Window::hold_input_with_keys`) is made with
/// [`PasteInputHold::with_keys`], and a paste that has waited
/// [`PASTE_WAIT_SHOWN_AFTER`] is then given up with Escape.
#[derive(Clone)]
pub struct PasteInputHold {
    hold: Rc<dyn Fn(&mut Window, &mut App, PasteKeyDown) -> Box<dyn std::any::Any>>,
    release: Rc<dyn Fn(Box<dyn std::any::Any>, &mut Window, &mut App)>,
    landing: Option<Rc<dyn Fn(&mut Window, &mut App)>>,
}

impl gpui::Global for PasteInputHold {}

/// What a paste that waits for the clipboard looks at the keys pressed
/// meanwhile with ([`PasteInputHold::with_keys`]): it gives back whether
/// it took the key, which then goes no further.
pub type PasteKeyDown = Box<dyn FnMut(&KeyDownEvent, &mut Window, &mut App) -> bool>;

/// How long a paste waits for the clipboard before its window says that
/// its input waits ([`paste_waits`]) and Escape gives it up: a read of
/// what another program holds takes some 70 ms under Kvist's harness, and
/// the four seconds a read waits for an owner that does not answer went
/// by with nothing to show for them (elundqvist/kvist#435).
pub const PASTE_WAIT_SHOWN_AFTER: std::time::Duration = std::time::Duration::from_millis(300);

/// How long a window says that a paste's clipboard could not be read
/// ([`paste_unanswered`]): where the program that holds it did not answer,
/// or stopped handing it over, the wait ended with nothing pasted and
/// nothing to say why (elundqvist/kvist#435).
pub const PASTE_UNANSWERED_SHOWN_FOR: std::time::Duration = std::time::Duration::from_secs(3);

impl PasteInputHold {
    /// A hold made of `hold`, which holds a window's input and gives back
    /// what releases it, an `H`, and `release`, which is handed that `H`
    /// and releases it.
    pub fn new<H: 'static>(
        hold: impl Fn(&mut Window, &mut App) -> H + 'static,
        release: impl Fn(H, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self::with_keys(move |window, cx, _| hold(window, cx), release)
    }

    /// A hold whose window hands the keys pressed while it holds to what
    /// `hold` is handed, a [`PasteKeyDown`], before they wait, as
    /// elundqvist/gpui-kit's `Window::hold_input_with_keys` does: a paste
    /// that has waited [`PASTE_WAIT_SHOWN_AFTER`] takes an Escape, which
    /// gives it up, its window's input let go at once, where it waited for
    /// the four seconds a read gives an owner that does not answer
    /// (elundqvist/kvist#435). An Escape before then waits with the rest,
    /// and where the paste is given up the window drops it, and a paste
    /// key pressed again, with the paste (`Window::hold_input_with_keys`).
    pub fn with_keys<H: 'static>(
        hold: impl Fn(&mut Window, &mut App, PasteKeyDown) -> H + 'static,
        release: impl Fn(H, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            hold: Rc::new(move |window, cx, keys| Box::new(hold(window, cx, keys))),
            release: Rc::new(move |held, window, cx| {
                // what `hold` gave back, an `H`, and nothing else
                if let Ok(held) = held.downcast::<H>() {
                    release(*held, window, cx);
                }
            }),
            landing: None,
        }
    }

    /// Calls `landing` as a paste that waited for the clipboard lands, just
    /// before it is made: an application that times what a key costs the
    /// drawing thread starts the paste's work there, and not at the key
    /// whose read the drawing thread did not wait for.
    pub fn on_landing(mut self, landing: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.landing = Some(Rc::new(landing));
        self
    }

    /// Make this the way every input's paste holds its window's input.
    pub fn set(self, cx: &mut App) {
        cx.set_global(self);
    }
}

/// Read the clipboard for a paste into `view`, as an input's paste reads
/// it, and hand `paste` what it holds.
///
/// With a way to hold the window's input back ([`PasteInputHold`]), the
/// clipboard is read as the platform reads it when it can wait, which on
/// X11 is off the drawing thread when another program owns it: a paste of
/// 1.4 MB from another program held the frame about 35 ms while the owner
/// handed it over (elundqvist/kvist#401). What is at hand, the app's own
/// copy, is handed over at once; otherwise `paste` is called once the read
/// has come, the window's input held back until then, so that what is
/// typed and clicked meanwhile lands after the paste, in the order it
/// came, and not at all if `view` has gone. Without a hold it is read
/// here, as before. A view that pastes into something of its own, a
/// terminal or a line edited in place, reads it this way as an input does
/// (elundqvist/kvist#433).
pub fn read_clipboard_for_paste<V: 'static>(
    view: &mut V,
    window: &mut Window,
    cx: &mut Context<V>,
    paste: impl FnOnce(&mut V, Option<ClipboardItem>, &mut Window, &mut Context<V>) + 'static,
) {
    let Some(hold) = cx.try_global::<PasteInputHold>().cloned() else {
        let item = cx.read_from_clipboard();
        paste(view, item, window, cx);
        return;
    };
    let read = cx.read_from_clipboard_async();
    paste_when_read(view, read, hold, window, cx, paste);
}

/// Hand `paste` what `read` gives: at once when it has it, as for the
/// app's own copy, and otherwise once it has come, the window's input held
/// back until then. One that has waited [`PASTE_WAIT_SHOWN_AFTER`] is
/// shown ([`paste_waits`]), and an Escape then gives it up, where the hold
/// hands it the keys ([`PasteInputHold::with_keys`]).
fn paste_when_read<V: 'static>(
    view: &mut V,
    mut read: gpui::Task<Result<Option<ClipboardItem>, gpui::ClipboardReadError>>,
    hold: PasteInputHold,
    window: &mut Window,
    cx: &mut Context<V>,
    paste: impl FnOnce(&mut V, Option<ClipboardItem>, &mut Window, &mut Context<V>) + 'static,
) {
    if let Some(read) = futures::FutureExt::now_or_never(&mut read) {
        paste(view, read.ok().flatten(), window, cx);
        return;
    }
    let wait = Rc::new(PasteWait {
        held: std::cell::RefCell::new(None),
        shown: Cell::new(false),
        release: hold.release.clone(),
    });
    let escape: PasteKeyDown = Box::new({
        let wait = wait.clone();
        move |key_down, window, cx| {
            let keystroke = &key_down.keystroke;
            keystroke.key == "escape"
                && !keystroke.modifiers.modified()
                && wait.shown.get()
                && wait.end(window, cx)
        }
    });
    let held = (hold.hold)(window, cx, escape);
    *wait.held.borrow_mut() = Some(held);
    // a paste that waits again says no more that the last went unanswered
    if paste_unanswered(window, cx) {
        let id = window.window_handle().window_id();
        cx.global_mut::<PasteUnanswered>().0.remove(&id);
        window.refresh();
    }
    window
        .spawn(cx, {
            let wait = wait.clone();
            async move |cx| {
                cx.background_executor().timer(PASTE_WAIT_SHOWN_AFTER).await;
                cx.update(|window, cx| wait.show(window, cx)).ok();
            }
        })
        .detach();
    cx.spawn_in(window, async move |view, cx| {
        let read = read.await;
        cx.update(|window, cx| {
            // given up with Escape: the input has gone on without it
            if !wait.waiting() {
                return;
            }
            // not read: on X11 the program that holds it did not answer in
            // time, or stopped handing it over
            let unanswered = read.is_err();
            if let Some(landing) = &hold.landing {
                landing(window, cx);
            }
            view.update(cx, |view, cx| paste(view, read.ok().flatten(), window, cx))
                .ok();
            wait.end(window, cx);
            if unanswered {
                say_unanswered(window, cx);
            }
        })
        .ok();
    })
    .detach();
}

/// The windows that say a paste's clipboard could not be read
/// ([`paste_unanswered`]), each with the number of the time it began to,
/// and the number the next time takes.
#[derive(Default)]
struct PasteUnanswered(std::collections::HashMap<gpui::WindowId, u64>, u64);

impl gpui::Global for PasteUnanswered {}

/// Have `window` say, for [`PASTE_UNANSWERED_SHOWN_FOR`], that a paste's
/// clipboard could not be read.
fn say_unanswered(window: &mut Window, cx: &mut App) {
    let id = window.window_handle().window_id();
    let unanswered = cx.default_global::<PasteUnanswered>();
    unanswered.1 += 1;
    let time = unanswered.1;
    unanswered.0.insert(id, time);
    window.refresh();
    window
        .spawn(cx, async move |cx| {
            cx.background_executor()
                .timer(PASTE_UNANSWERED_SHOWN_FOR)
                .await;
            cx.update(|window, cx| {
                let unanswered = &mut cx.default_global::<PasteUnanswered>().0;
                if unanswered.get(&id) == Some(&time) {
                    unanswered.remove(&id);
                    window.refresh();
                }
            })
            .ok();
        })
        .detach();
}

/// Whether `window` says that the clipboard a paste in it waited for could
/// not be read, for [`PASTE_UNANSWERED_SHOWN_FOR`] after: the program that
/// holds it did not answer, or stopped handing it over, and nothing was
/// pasted, where the wait ended with nothing to say why
/// (elundqvist/kvist#435). The window is drawn again as that begins and as
/// it ends, and a paste that waits again ends it.
pub fn paste_unanswered(window: &Window, cx: &App) -> bool {
    cx.try_global::<PasteUnanswered>()
        .is_some_and(|unanswered| {
            unanswered
                .0
                .contains_key(&window.window_handle().window_id())
        })
}

/// A paste waiting for the clipboard: what holds its window's input, until
/// it lands or is given up, and whether its window says that it waits.
struct PasteWait {
    held: std::cell::RefCell<Option<Box<dyn std::any::Any>>>,
    shown: Cell<bool>,
    release: Rc<dyn Fn(Box<dyn std::any::Any>, &mut Window, &mut App)>,
}

/// The windows that say a paste waits in them ([`paste_waits`]), with how
/// many do.
#[derive(Default)]
struct PasteWaitsShown(std::collections::HashMap<gpui::WindowId, usize>);

impl gpui::Global for PasteWaitsShown {}

impl PasteWait {
    fn waiting(&self) -> bool {
        self.held.borrow().is_some()
    }

    /// It has waited long enough for its window to say so.
    fn show(&self, window: &mut Window, cx: &mut App) {
        if !self.waiting() || self.shown.replace(true) {
            return;
        }
        let id = window.window_handle().window_id();
        *cx.default_global::<PasteWaitsShown>()
            .0
            .entry(id)
            .or_default() += 1;
        window.refresh();
    }

    /// It has landed or been given up: its window's input goes on. Whether
    /// it was still waiting.
    fn end(&self, window: &mut Window, cx: &mut App) -> bool {
        let Some(held) = self.held.borrow_mut().take() else {
            return false;
        };
        if self.shown.get() {
            let id = window.window_handle().window_id();
            let shown = &mut cx.default_global::<PasteWaitsShown>().0;
            if let Some(count) = shown.get_mut(&id) {
                *count -= 1;
                if *count == 0 {
                    shown.remove(&id);
                }
            }
            window.refresh();
        }
        (self.release)(held, window, cx);
        true
    }
}

/// Whether a paste in `window` has waited for the clipboard long enough,
/// [`PASTE_WAIT_SHOWN_AFTER`], for the window to say that its input waits,
/// and that Escape gives the paste up (elundqvist/kvist#435). The window
/// is drawn again as that begins and as it ends.
pub fn paste_waits(window: &Window, cx: &App) -> bool {
    cx.try_global::<PasteWaitsShown>()
        .is_some_and(|shown| shown.0.contains_key(&window.window_handle().window_id()))
}

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = input, no_json)]
pub struct Enter {
    /// Is confirm with secondary.
    pub secondary: bool,
    /// Whether the Shift modifier was held when Enter was pressed.
    pub shift: bool,
}

impl Enter {
    /// Returns true if `action` is a primary `Enter` action (`secondary: false`),
    /// regardless of whether Shift was held.
    pub fn is_primary(action: &dyn Action) -> bool {
        action.partial_eq(&Enter {
            secondary: false,
            shift: false,
        }) || action.partial_eq(&Enter {
            secondary: false,
            shift: true,
        })
    }
}

actions!(
    input,
    [
        Backspace,
        Delete,
        DeleteToBeginningOfLine,
        DeleteToEndOfLine,
        DeleteToPreviousWordStart,
        DeleteToNextWordEnd,
        Indent,
        Outdent,
        IndentInline,
        OutdentInline,
        MoveUp,
        MoveDown,
        MoveLeft,
        MoveRight,
        MoveHome,
        MoveEnd,
        MovePageUp,
        MovePageDown,
        SelectAll,
        SelectToStartOfLine,
        SelectToEndOfLine,
        SelectToStart,
        SelectToEnd,
        SelectToPreviousWordStart,
        SelectToNextWordEnd,
        ShowCharacterPalette,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        MoveToStartOfLine,
        MoveToEndOfLine,
        MoveToStart,
        MoveToEnd,
        MoveToPreviousWord,
        MoveToNextWord,
        Escape,
        ToggleCodeActions,
        Search,
        Replace,
        GoToDefinition,
    ]
);

#[derive(Clone)]
pub enum InputEvent {
    Change,
    PressEnter { secondary: bool, shift: bool },
    Focus,
    Blur,
}

pub(super) const CONTEXT: &str = "Input";

pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("shift-backspace", Backspace, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("delete", Delete, Some(CONTEXT)),
        KeyBinding::new("shift-delete", Delete, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-backspace", DeleteToBeginningOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-delete", DeleteToEndOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-backspace", DeleteToPreviousWordStart, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-backspace", DeleteToPreviousWordStart, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-delete", DeleteToNextWordEnd, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-delete", DeleteToNextWordEnd, Some(CONTEXT)),
        KeyBinding::new(
            "enter",
            Enter {
                secondary: false,
                shift: false,
            },
            Some(CONTEXT),
        ),
        KeyBinding::new(
            "shift-enter",
            Enter {
                secondary: false,
                shift: true,
            },
            Some(CONTEXT),
        ),
        KeyBinding::new(
            "secondary-enter",
            Enter {
                secondary: true,
                shift: false,
            },
            Some(CONTEXT),
        ),
        KeyBinding::new("escape", Escape, Some(CONTEXT)),
        KeyBinding::new("up", MoveUp, Some(CONTEXT)),
        KeyBinding::new("down", MoveDown, Some(CONTEXT)),
        KeyBinding::new("left", MoveLeft, Some(CONTEXT)),
        KeyBinding::new("right", MoveRight, Some(CONTEXT)),
        KeyBinding::new("pageup", MovePageUp, Some(CONTEXT)),
        KeyBinding::new("pagedown", MovePageDown, Some(CONTEXT)),
        KeyBinding::new("tab", IndentInline, Some(CONTEXT)),
        KeyBinding::new("shift-tab", OutdentInline, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-]", Indent, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-]", Indent, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-[", Outdent, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-[", Outdent, Some(CONTEXT)),
        KeyBinding::new("shift-left", SelectLeft, Some(CONTEXT)),
        KeyBinding::new("shift-right", SelectRight, Some(CONTEXT)),
        KeyBinding::new("shift-up", SelectUp, Some(CONTEXT)),
        KeyBinding::new("shift-down", SelectDown, Some(CONTEXT)),
        KeyBinding::new("home", MoveHome, Some(CONTEXT)),
        KeyBinding::new("end", MoveEnd, Some(CONTEXT)),
        KeyBinding::new("shift-home", SelectToStartOfLine, Some(CONTEXT)),
        KeyBinding::new("shift-end", SelectToEndOfLine, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-home", MoveToStart, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-end", MoveToEnd, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-home", SelectToStart, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-end", SelectToEnd, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-shift-a", SelectToStartOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-shift-e", SelectToEndOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("shift-cmd-left", SelectToStartOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("shift-cmd-right", SelectToEndOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-shift-left", SelectToPreviousWordStart, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-left", SelectToPreviousWordStart, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-shift-right", SelectToNextWordEnd, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-right", SelectToNextWordEnd, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-a", SelectAll, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-a", SelectAll, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-c", Copy, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-c", Copy, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-x", Cut, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-x", Cut, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-v", Paste, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-v", Paste, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-a", MoveHome, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-left", MoveHome, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-e", MoveEnd, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-right", MoveEnd, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-z", Undo, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-z", Redo, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-up", MoveToStart, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-down", MoveToEnd, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-left", MoveToPreviousWord, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-right", MoveToNextWord, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-left", MoveToPreviousWord, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-right", MoveToNextWord, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-up", SelectToStart, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-down", SelectToEnd, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-z", Undo, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-y", Redo, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-.", ToggleCodeActions, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-.", ToggleCodeActions, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-f", Search, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-f", Search, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-f", Replace, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-h", Replace, Some(CONTEXT)),
    ]);
}

/// The shared text-editing engine behind [`crate::input::InputState`],
/// [`crate::input::TextareaState`] and [`crate::input::EditorState`].
///
/// `M` is the mode marker: it carries no data and only decides which methods
/// exist, so an ordinary input cannot reach the editor's language features.
///
/// The three states are type aliases of this one, which is why this name is
/// public: an alias is only as usable as the type behind it, so hiding this
/// would leave `InputState` unable to do anything. Prefer naming the aliases
/// — write `InputState`, not `InputBaseState<InputMode>`.
pub struct InputBaseState<M: InputModeKind> {
    /// State only this mode needs. See [`InputModeKind::Extras`].
    pub(crate) extras: M::Extras,
    pub(super) focus_handle: FocusHandle,
    pub(super) mode: LayoutMode,
    pub(super) text: Rope,
    pub(super) display_map: DisplayMap,
    pub(super) undo_manager: UndoManager,
    pub(super) search_session: super::SearchSession,
    pub(super) searchable: bool,
    pub(super) replaceable: bool,
    pub(super) soft_wrap: bool,
    pub(super) wrapping_indent: WrappingIndent,
    pub(super) scroll_beyond_last_line: Option<usize>,
    pub(super) cursor_surrounding_lines: Option<usize>,
    pub(super) blink_cursor: Entity<BlinkCursor>,
    pub(super) loading: bool,
    /// Range in UTF-8 length for the selected text.
    ///
    /// - "Hello 世界💝" = 16
    /// - "💝" = 4
    pub(super) selected_range: Selection,
    /// Range for save the selected word, use to keep word range when drag move.
    pub(super) selected_word_range: Option<Selection>,
    pub(super) selection_reversed: bool,
    /// The marked range is the temporary insert text on IME typing.
    pub(super) ime_marked_range: Option<Selection>,
    pub(super) last_layout: Option<LastLayout>,
    pub(super) last_cursor: Option<usize>,
    /// The input container bounds
    pub(super) input_bounds: Bounds<Pixels>,
    /// The text bounds
    pub(super) last_bounds: Option<Bounds<Pixels>>,
    pub(super) last_selected_range: Option<Selection>,
    pub(super) selecting: bool,
    pub(crate) disabled: bool,
    pub(crate) readonly: bool,
    pub(crate) text_align: TextAlign,
    pub(super) masked: bool,
    pub(super) clean_on_escape: bool,
    pub(super) submit_on_enter: bool,
    pub(super) show_whitespaces: bool,
    /// This flag tells the renderer to prefer the end of the current visual line.
    pub(crate) cursor_line_end_affinity: bool,
    pub(super) pattern: Option<regex::Regex>,
    pub(super) validate: Option<Box<dyn Fn(&str, &mut App) -> bool + 'static>>,
    /// The step strategy for [`super::NumberInput`] to increment/decrement.
    /// See [`Self::step`] and [`Self::step_by`].
    pub(crate) number_step: Option<NumberStep>,
    /// The minimum value for [`super::NumberInput`]. See [`Self::min`].
    pub(crate) number_min: Option<f64>,
    /// The maximum value for [`super::NumberInput`]. See [`Self::max`].
    pub(crate) number_max: Option<f64>,
    pub(crate) scroll_handle: ScrollHandle,
    /// The deferred scroll offset to apply on next layout.
    pub(crate) deferred_scroll_offset: Option<Point<Pixels>>,
    /// The deferred offset's x is the layout's to choose, by its follow of
    /// the caret: the text has changed since the last layout, which the
    /// column would be read from. See [`Self::scroll_to_edited`].
    pub(crate) deferred_scroll_follows_caret: bool,
    /// The size of the scrollable content.
    pub(crate) scroll_size: gpui::Size<Pixels>,
    pub(super) editor_scrollbar_snapshot: Cell<Option<EditorScrollbarSnapshot>>,
    /// Where the caret is painted in the frame being drawn: set as the text
    /// is laid out. See [`Self::painted_caret`].
    pub(super) painted_caret: Cell<Option<PaintedCaret>>,
    /// The `⋯` painted after each folded line in view in the last frame:
    /// the fold's first line and where it was painted.
    pub(super) fold_placeholders: Vec<(usize, Bounds<Pixels>)>,
    pub(super) editor_paddings: Edges<Pixels>,
    /// Room in a code editor's gutter the caller paints in itself, between
    /// the line numbers and the fold icons. See [`Self::gutter_extra`].
    pub(super) gutter_extra: Pixels,
    /// The style this state paints with: what was projected onto it, with
    /// every colour left unset resolved from the palette that is current. It
    /// is rebuilt at the top of every render, which is what keeps it current
    /// when the palette changes after the state was built.
    pub(super) editor_style: InputEditorStyle,
    /// What a consumer projected, kept verbatim so that resolution never
    /// consumes its own output: resolving in place would fill the unset
    /// colours once and then never see them as unset again, which is the same
    /// freeze in a different place.
    projected_editor_style: InputEditorStyle,

    /// The mask pattern for formatting the input text
    pub(crate) mask_pattern: MaskPattern,
    /// Whether the `mask_pattern` was explicitly set (via [`Self::mask_pattern`]
    /// or [`Self::set_mask_pattern`]), to let [`super::NumberInput`] only apply
    /// its default mask when the user has not made an explicit choice.
    pub(super) mask_pattern_set: bool,
    pub(super) placeholder: SharedString,

    /// Diagnostic currently requested by pointer hover; applications render it.
    pub(super) diagnostic_popover: Option<Rc<crate::input::DiagnosticEntry>>,

    context_menu_handler: Option<
        Rc<dyn Fn(NativeMenu, InputContextMenuCapabilities, Point<Pixels>, &mut Window, &mut App)>,
    >,
    pending_context_menu: Option<(Point<Pixels>, usize)>,

    /// Whether the context menu that shows on right-click is enabled.
    ///
    pub(super) enable_context_menu: bool,

    /// A flag to indicate if we are currently inserting a completion item.
    pub(super) completion_inserting: bool,
    pub(super) overlay_action_handler: Option<
        Rc<
            dyn Fn(
                super::InputOverlayKind,
                Box<dyn Action>,
                &mut Window,
                &mut Context<InputBaseState<M>>,
            ) -> bool,
        >,
    >,

    /// A flag to indicate if we have a pending update to the text.
    ///
    /// If true, will call some update (for example LSP, Syntax Highlight) before render.
    _pending_update: bool,
    /// A flag to indicate if we should ignore the next completion event.
    pub(super) silent_replace_text: bool,
    /// A flag to indicate if we should emit InputEvents.
    pub(super) emit_events: bool,

    /// To remember the horizontal column (x-coordinate) of the cursor position for keep column for move up/down.
    ///
    /// The first element is the x-coordinate (Pixels), preferred to use this.
    /// The second element is the column (usize), fallback to use this.
    pub(super) preferred_column: Option<(Pixels, usize)>,
    _subscriptions: Vec<Subscription>,

    pub(super) auto_scroll: AutoScroll,
}

/// Read-only styling data exposed to presentation facades.
///
/// The fields are private and read through the methods below, so that a new
/// one can be added without breaking the facades.
#[derive(Clone)]
pub struct InputPresentation {
    focus_handle: FocusHandle,
    disabled: bool,
    readonly: bool,
    loading: bool,
    masked: bool,
    multi_line: bool,
    code_editor: bool,
    text_align: TextAlign,
    placeholder: SharedString,
    mask_placeholder: Option<String>,
}

impl InputPresentation {
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    pub fn is_readonly(&self) -> bool {
        self.readonly
    }

    /// Returns true if the user is allowed to change the text.
    ///
    /// See also: [`InputBaseState::is_editable`].
    pub fn is_editable(&self) -> bool {
        !self.disabled && !self.readonly
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn is_masked(&self) -> bool {
        self.masked
    }

    pub fn is_multi_line(&self) -> bool {
        self.multi_line
    }

    pub fn is_code_editor(&self) -> bool {
        self.code_editor
    }

    pub fn text_align(&self) -> TextAlign {
        self.text_align
    }

    pub fn placeholder(&self) -> &SharedString {
        &self.placeholder
    }

    /// The placeholder derived from the mask pattern, e.g.: `(___) ___-____`.
    pub fn mask_placeholder(&self) -> Option<&str> {
        self.mask_placeholder.as_deref()
    }
}

impl<M: InputModeKind> EventEmitter<InputEvent> for InputBaseState<M> {}

/// Where an input paints its caret in the frame being drawn, in window
/// coordinates. See [`InputBaseState::painted_caret`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaintedCaret {
    /// The caret as it is painted: as tall as most of its line, and
    /// centred in it.
    pub bounds: Bounds<Pixels>,
    pub line_height: Pixels,
    /// The input's own bounds, its gutter with its text, as laid out in
    /// the same frame.
    pub input_bounds: Bounds<Pixels>,
}

impl PaintedCaret {
    /// The line the caret is on, from its top to its bottom.
    pub fn line(&self) -> Bounds<Pixels> {
        let top = self.bounds.origin.y - (self.line_height - self.bounds.size.height) / 2.;
        Bounds::new(
            gpui::point(self.bounds.origin.x, top),
            gpui::size(self.bounds.size.width, self.line_height),
        )
    }
}

impl<M: InputModeKind> InputBaseState<M> {
    /// Where the caret is painted in the frame being drawn, in window
    /// coordinates, and the input's own bounds in that frame.
    ///
    /// The input lays its text out before anything painted over it is
    /// placed, so a deferred element (a completion menu, a popover) that
    /// reads this in its prepaint finds the caret where it is drawn now.
    /// [`Self::cursor_layout`] is kept from the last paint, which after an
    /// edit is the frame before, and something placed by it is a frame
    /// behind the caret. `None` before the first layout, and while the
    /// caret is not laid out.
    pub fn painted_caret(&self) -> Option<PaintedCaret> {
        self.painted_caret.get()
    }

    #[doc(hidden)]
    pub fn cursor_layout(&self) -> Option<(Bounds<Pixels>, Pixels)> {
        let layout = self.last_layout.as_ref()?;
        Some((layout.cursor_bounds?, layout.line_height))
    }

    pub fn input_bounds(&self) -> Bounds<Pixels> {
        self.input_bounds
    }

    pub fn text_bounds(&self) -> Option<Bounds<Pixels>> {
        self.last_bounds
    }

    /// Where the `⋯` after folded `line` was painted in the last frame, in
    /// window coordinates: none when the line is not folded, or not in view.
    /// What a caller draws after a folded line's end goes after this.
    pub fn fold_placeholder_bounds(&self, line: usize) -> Option<Bounds<Pixels>> {
        self.fold_placeholders
            .iter()
            .find(|(folded, _)| *folded == line)
            .map(|(_, bounds)| *bounds)
    }

    pub fn diagnostic_popover(&self) -> Option<Rc<crate::input::DiagnosticEntry>> {
        self.diagnostic_popover.clone()
    }

    pub fn presentation(&self) -> InputPresentation {
        InputPresentation {
            focus_handle: self.focus_handle.clone(),
            disabled: self.disabled,
            readonly: self.readonly,
            loading: self.loading,
            masked: self.masked,
            multi_line: self.is_multi_line(),
            code_editor: self.is_code_editor(),
            text_align: self.text_align,
            placeholder: self.placeholder.clone(),
            mask_placeholder: self.mask_pattern.placeholder(),
        }
    }

    /// Whether this input spans more than one line.
    ///
    /// Answered by the mode marker, which is fixed when the state is built.
    /// [`LayoutMode`] holds the row counts and growth policy, not the kind.
    #[inline]
    /// Whether this input paints scrollbars.
    ///
    /// Only a multi-line input can scroll: a single-line input keeps its
    /// caret in view by moving its own offset, and never has a viewport a
    /// user could drag. Adding the editor scrollbar to every input put a
    /// thumb inside every text field, which is a control the field does not
    /// have.
    pub(crate) fn shows_scrollbar(&self) -> bool {
        self.is_multi_line()
    }

    pub fn is_multi_line(&self) -> bool {
        M::MULTI_LINE
    }

    /// Whether this input is a single-line text field. See [`Self::is_multi_line`].
    #[inline]
    pub fn is_single_line(&self) -> bool {
        !M::MULTI_LINE
    }

    /// Whether this input is a source-code editor.
    #[inline]
    pub fn is_code_editor(&self) -> bool {
        M::CODE_EDITOR
    }

    /// Whether the user is allowed to copy the selection out.
    ///
    /// A masked input keeps its value out of the clipboard.
    pub fn is_copyable(&self) -> bool {
        !self.selected_range.is_empty() && !self.masked
    }

    pub fn context_menu_capabilities(&self) -> InputContextMenuCapabilities {
        let (go_to_definition, code_actions) = self.extras.context_menu_capabilities();
        InputContextMenuCapabilities::new()
            .disabled(self.disabled)
            .readonly(self.readonly)
            .code_editor(self.is_code_editor())
            .selection(!self.selected_range.is_empty())
            .masked(self.masked)
            .go_to_definition(go_to_definition)
            .code_actions(code_actions)
    }

    pub fn set_text_align(&mut self, text_align: TextAlign, cx: &mut Context<Self>) {
        if !self.is_single_line() || self.text_align == text_align {
            return;
        }

        self.text_align = text_align;
        cx.notify();
    }

    /// Flip the password mask.
    ///
    /// Setting the mask is a single-line method, but flipping it stays here:
    /// the reveal button is rendered from the generic path, and it can only be
    /// switched on through [`crate::input::InputState`] anyway.
    pub fn toggle_masked(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.masked = !self.masked;
        cx.notify();
    }

    pub fn on_context_menu(
        &mut self,
        handler: Rc<
            dyn Fn(NativeMenu, InputContextMenuCapabilities, Point<Pixels>, &mut Window, &mut App),
        >,
    ) {
        self.context_menu_handler = Some(handler);
    }

    /// Build the engine. Each mode's own `new` sets its layout on top of this.
    fn new_in_mode(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle().tab_stop(true);
        let blink_cursor = cx.new(|_| BlinkCursor::new());
        let undo_manager = UndoManager::new();

        let _subscriptions = vec![
            // Observe the blink cursor to repaint the view when it changes.
            cx.observe(&blink_cursor, |_, _, cx| cx.notify()),
            // Blink the cursor when the window is active, pause when it's not.
            cx.observe_window_activation(window, |input, window, cx| {
                if window.is_window_active() {
                    let focus_handle = input.focus_handle.clone();
                    if focus_handle.is_focused(window) {
                        input.blink_cursor.update(cx, |blink_cursor, cx| {
                            blink_cursor.start(cx);
                        });
                    }
                }
            }),
            cx.on_focus(&focus_handle, window, Self::on_focus),
            cx.on_blur(&focus_handle, window, Self::on_blur),
        ];

        let text_style = window.text_style();

        Self {
            extras: M::Extras::default(),
            focus_handle: focus_handle.clone(),
            text: "".into(),
            display_map: DisplayMap::new(text_style.font(), window.rem_size(), None),
            search_session: super::SearchSession::default(),
            searchable: false,
            replaceable: true,
            soft_wrap: true,
            wrapping_indent: WrappingIndent::default(),
            scroll_beyond_last_line: None,
            cursor_surrounding_lines: None,
            blink_cursor,
            undo_manager,
            selected_range: Selection::default(),
            selected_word_range: None,
            selection_reversed: false,
            ime_marked_range: None,
            input_bounds: Bounds::default(),
            selecting: false,
            disabled: false,
            readonly: false,
            text_align: TextAlign::Left,
            masked: false,
            clean_on_escape: false,
            submit_on_enter: false,
            show_whitespaces: false,
            loading: false,
            pattern: None,
            validate: None,
            number_step: Some(NumberStep::Fixed(1.)),
            number_min: None,
            number_max: None,
            mode: LayoutMode::default(),
            last_layout: None,
            last_bounds: None,
            last_selected_range: None,
            last_cursor: None,
            scroll_handle: ScrollHandle::new(),
            scroll_size: gpui::size(px(0.), px(0.)),
            editor_scrollbar_snapshot: Cell::new(None),
            painted_caret: Cell::new(None),
            fold_placeholders: Vec::new(),
            editor_paddings: Edges::default(),
            gutter_extra: px(0.),
            deferred_scroll_offset: None,
            deferred_scroll_follows_caret: false,
            preferred_column: None,
            placeholder: SharedString::default(),
            mask_pattern: MaskPattern::default(),
            mask_pattern_set: false,
            editor_style: InputEditorStyle::default(),
            projected_editor_style: InputEditorStyle::default(),
            diagnostic_popover: None,
            context_menu_handler: None,
            pending_context_menu: None,
            enable_context_menu: true,
            completion_inserting: false,
            overlay_action_handler: None,
            silent_replace_text: false,
            emit_events: true,
            _subscriptions,
            _pending_update: false,
            cursor_line_end_affinity: false,
            auto_scroll: AutoScroll::default(),
        }
    }

    /// Sets whether the context menu that shows on right-click is enabled.
    ///
    /// The context menu is enabled by default.
    /// This value is ignored if a custom context menu builder is defined on the input.
    pub fn context_menu(mut self, enable: bool) -> Self {
        self.enable_context_menu = enable;
        self
    }

    pub fn set_context_menu_enabled(&mut self, enabled: bool) {
        self.enable_context_menu = enabled;
    }

    /// Set whether search UI allows replacement, default is true.
    #[doc(hidden)]
    pub fn replaceable(mut self, allow: bool) -> Self {
        self.replaceable = allow;
        self
    }

    /// Set placeholder
    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// Set highlighter language for for [`LayoutMode::CodeEditor`] mode.
    ///
    /// The highlighter for the new language is made at the next frame.
    pub fn set_highlighter(
        &mut self,
        new_language: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        match &mut self.mode {
            LayoutMode::CodeEditor {
                language,
                highlighter,
                ..
            } => {
                *language = new_language.into();
                *highlighter.borrow_mut() = None;
                // The next frame makes the new highlighter and parses the
                // text, so the editor keeps drawing its colours and
                // diagnostics without waiting for an edit.
                self._pending_update = true;
            }
            _ => {}
        }
        cx.notify();
    }

    fn reset_highlighter(&mut self, cx: &mut Context<Self>) {
        match &mut self.mode {
            LayoutMode::CodeEditor { highlighter, .. } => {
                *highlighter.borrow_mut() = None;
            }
            _ => {}
        }
        cx.notify();
    }

    /// Install the parser/highlighter adapter used by code-editor mode.
    pub fn set_highlighter_factory(
        &mut self,
        factory: InputHighlighterFactory,
        cx: &mut Context<Self>,
    ) {
        self.mode.set_highlighter_factory(factory);
        self._pending_update = true;
        cx.notify();
    }

    /// Install a default adapter without replacing an application-provided one.
    pub fn ensure_highlighter_factory(&mut self, factory: InputHighlighterFactory) {
        self.mode.ensure_highlighter_factory(factory);
    }

    pub fn set_editor_style(&mut self, style: InputEditorStyle) {
        self.editor_style = style.clone();
        self.projected_editor_style = style;
    }

    /// Set presentation padding for multi-line text and its scrollbar layout.
    #[doc(hidden)]
    pub fn set_editor_paddings(&mut self, paddings: Edges<Pixels>) {
        self.editor_paddings = paddings;
    }

    pub fn apply_highlighter_fold_candidates(
        &mut self,
        candidates: Vec<crate::input::FoldRange>,
        cx: &mut Context<Self>,
    ) {
        if self.mode.is_folding() {
            self.display_map.set_fold_candidates(candidates);
        }
        cx.notify();
    }

    /// Whether a fold candidate the highlighter gave starts at `line`: what
    /// a test of a highlighter that hands them later reads.
    #[doc(hidden)]
    pub fn is_fold_candidate(&self, line: usize) -> bool {
        self.display_map.is_fold_candidate(line)
    }

    #[inline]
    pub fn diagnostics(&self) -> Option<&DiagnosticSet> {
        self.mode.diagnostics()
    }

    #[inline]
    pub fn diagnostics_mut(&mut self) -> Option<&mut DiagnosticSet> {
        self.mode.diagnostics_mut()
    }

    /// Set placeholder
    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    /// Find which line and sub-line the given offset belongs to, along with the position within that sub-line.
    ///
    /// Returns:
    ///
    /// - The index of the line (zero-based) containing the offset.
    /// - The index of the sub-line (zero-based) within the line containing the offset.
    /// - The position of the offset.
    pub(super) fn line_and_position_for_offset(
        &self,
        offset: usize,
    ) -> (usize, usize, Option<Point<Pixels>>) {
        let Some(last_layout) = &self.last_layout else {
            return (0, 0, None);
        };
        let line_height = last_layout.line_height;

        let mut y_offset = last_layout.visible_top;
        for (vi, line) in last_layout.lines.iter().enumerate() {
            let prev_lines_offset = last_layout.visible_line_byte_offsets[vi];
            // before this line and after the one before it: on a line a
            // fold hides, or above the lines laid out. It has no place, where
            // it was answered with this line's start, so that what belonged
            // to every line a fold hid was drawn in one place on the line
            // that closes the fold
            if offset < prev_lines_offset {
                break;
            }
            let local_offset = offset - prev_lines_offset;
            if let Some(pos) = line.position_for_index(local_offset, last_layout, false) {
                let sub_line_index = (pos.y / line_height) as usize;
                let adjusted_pos = point(pos.x + last_layout.line_number_width, pos.y + y_offset);
                return (vi, sub_line_index, Some(adjusted_pos));
            }

            y_offset += line.size(line_height).height;
        }
        (0, 0, None)
    }

    /// Set the text of the input field.
    ///
    /// For single-line inputs the caret is placed at the end of the text while
    /// the view is scrolled back to the start, so a long value shows its
    /// beginning instead of its tail (matching HTML `<input>`). Multi-line
    /// inputs reset the selection to `0..0`.
    pub fn set_value(
        &mut self,
        value: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.undo_manager.set_ignoring(true);
        self.emit_events = false;
        self.replace_text(value, window, cx);
        self.undo_manager.set_ignoring(false);
        self.emit_events = true;

        self.reset_selection();
        self.reset_lsp_state();
        self.reset_scroll_to_start();

        self.undo_manager.clear();
        cx.notify();
    }

    /// Set the text as [`set_value`](Self::set_value) does, from a value
    /// read off the UI thread ([`PreparedValue`]): nothing here reads the
    /// whole text, so a large text takes no longer than a small one where
    /// the input does not soft-wrap. One that wraps lays every line out
    /// again here, as `set_value` does. The value is taken as it is, not
    /// normalized as `set_value` normalizes a single line or a number, so
    /// it is for a multi-line input with no mask. The text and the lines
    /// it replaces are freed on a background thread.
    ///
    /// `set_value` built the rope, laid out every line and then laid them
    /// all out again for the first frame, a pass over the whole text each:
    /// on a 20 MB text, some half a second of the UI thread.
    pub fn set_prepared_value(&mut self, value: PreparedValue, cx: &mut Context<Self>) {
        let PreparedValue { text, lines } = value;
        let replaced = self.display_map.set_prepared_text(&text, lines, cx);
        let old_text = std::mem::replace(&mut self.text, text);
        M::reset_annotations(self);
        if let Some(diagnostics) = self.mode.diagnostics_mut() {
            diagnostics.reset(&self.text)
        }
        // what else holds the old text lets it go here, so that the
        // background drop below is the last and frees it
        self.update_search(cx);
        self.reset_highlighter(cx);
        cx.background_spawn(async move { drop((replaced, old_text)) })
            .detach();
        self.ime_marked_range = None;
        self.preferred_column = None;
        if self.is_multi_line() {
            self.mode.update_auto_grow(&self.display_map);
        }
        self.reset_selection();
        self.reset_lsp_state();
        self.reset_scroll_to_start();
        self.undo_manager.clear();
        cx.notify();
    }

    /// Replace the entire text content while preserving undo history.
    ///
    /// Unlike [`set_value`](Self::set_value), this method records the
    /// replacement in the undo stack, allowing the user to undo/redo
    /// the change. The selection is placed at the end of the new text
    /// for single-line inputs, or cleared (0..0) for multi-line inputs.
    ///
    /// Use this when programmatically replacing the full text but the
    /// user should still be able to undo the operation — e.g. formatting.
    pub fn replace_all(
        &mut self,
        text: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_text(text, window, cx);
        self.reset_selection();
        self.reset_lsp_state();
        self.reset_scroll_to_start();

        cx.notify();
    }

    /// Perform `f` with the user-facing edit restrictions lifted.
    ///
    /// The `disabled` and `readonly` modes only reject the changes made by the
    /// user, the programmatic APIs must always be able to update the text.
    fn with_edits_allowed(&mut self, f: impl FnOnce(&mut Self)) {
        let (was_disabled, was_readonly) = (self.disabled, self.readonly);
        (self.disabled, self.readonly) = (false, false);
        f(self);
        (self.disabled, self.readonly) = (was_disabled, was_readonly);
    }

    /// Insert text at the current cursor position.
    ///
    /// And the cursor will be moved to the end of inserted text.
    pub fn insert(
        &mut self,
        text: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text: SharedString = text.into();
        self.with_edits_allowed(|this| {
            this.undo_manager.pending_intent = Some(EditIntent::Atomic);
            let range_utf16 = this.range_to_utf16(&(this.cursor()..this.cursor()));
            this.replace_text_in_range_silent(Some(range_utf16), &text, window, cx);
            this.selected_range = (this.selected_range.end..this.selected_range.end).into();
        });
    }

    /// Replace text at the current cursor position.
    ///
    /// And the cursor will be moved to the end of replaced text.
    pub fn replace(
        &mut self,
        text: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text: SharedString = text.into();
        self.with_edits_allowed(|this| {
            this.undo_manager.pending_intent = Some(EditIntent::Atomic);
            this.replace_text_in_range_silent(None, &text, window, cx);
            this.selected_range = (this.selected_range.end..this.selected_range.end).into();
        });
    }

    /// Replace the text in `range`, UTF-8 byte offsets, with `text`, as one
    /// step of undo, and move the cursor to the end of what was put in.
    ///
    /// Unlike selecting `range` and calling [`Self::replace`], the selection
    /// is left alone until the edit is made, so undoing it puts back the
    /// selection as it was before the call rather than selecting the range:
    /// an edit that reaches far from the cursor, such as a completion that
    /// adds an import at the top, is taken back with the cursor where it was.
    pub fn replace_range(
        &mut self,
        range: Range<usize>,
        text: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text: SharedString = text.into();
        self.with_edits_allowed(|this| {
            this.undo_manager.pending_intent = Some(EditIntent::Atomic);
            let range = this.text.clip_offset(range.start, Bias::Left)
                ..this.text.clip_offset(range.end, Bias::Right);
            let range_utf16 = this.range_to_utf16(&range);
            this.replace_text_in_range_silent(Some(range_utf16), &text, window, cx);
            this.selected_range = (this.selected_range.end..this.selected_range.end).into();
        });
    }

    fn replace_text(
        &mut self,
        text: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text: SharedString = text.into();
        self.with_edits_allowed(|this| {
            this.undo_manager.pending_intent = Some(EditIntent::Atomic);
            let range = 0..this.text.chars().map(|c| c.len_utf16()).sum();
            this.replace_text_in_range_silent(Some(range), &text, window, cx);
            this.reset_highlighter(cx);
            // a text set whole has none of the old one's problems, as
            // `set_prepared_value`'s has not: moved by an edit over the
            // whole, every one was taken with it, and whoever hands them
            // finds none held and hands them again (elundqvist/kvist#370)
            if let Some(diagnostics) = this.mode.diagnostics_mut() {
                diagnostics.reset(&this.text)
            }
        });
    }

    fn reset_selection(&mut self) {
        // For single-line inputs the caret is placed at the end of the text
        // (matching HTML `<input>`); multi-line inputs reset the selection to
        // `0..0`.
        if self.is_single_line() {
            let end = self.text.len();
            self.selected_range = (end..end).into();
        } else {
            self.selected_range.clear();
        }
    }

    fn reset_lsp_state(&mut self) {
        if self.is_code_editor() {
            self._pending_update = true;
            M::reset_language_features(self);
        }
    }

    fn reset_scroll_to_start(&mut self) {
        // Move scroll to the start. For single-line the caret is at the end, so
        // override the cursor-follow scroll for the next painted frame to keep
        // the start visible; the deferred offset is consumed during that paint.
        self.scroll_handle.set_offset(point(px(0.), px(0.)));
        if self.is_single_line() {
            self.deferred_scroll_offset = Some(point(px(0.), px(0.)));
            self.deferred_scroll_follows_caret = false;
        }
    }

    /// Set with disabled mode.
    ///
    /// See also: [`Self::set_disabled`].
    #[allow(unused)]
    pub(crate) fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn set_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        if self.disabled == disabled {
            return;
        }

        self.disabled = disabled;
        cx.notify();
    }

    /// Set with read-only mode.
    ///
    /// Unlike [`Self::disabled`], a read-only input keeps the normal appearance,
    /// focus, cursor, selection and copy behavior, it only rejects any change
    /// of the text made by the user.
    ///
    /// See also: [`Self::set_readonly`].
    #[allow(unused)]
    pub(crate) fn readonly(mut self, readonly: bool) -> Self {
        self.readonly = readonly;
        self
    }

    pub fn set_readonly(&mut self, readonly: bool, cx: &mut Context<Self>) {
        if self.readonly == readonly {
            return;
        }

        self.readonly = readonly;
        if readonly {
            self.search_session.replace_mode = false;
        }
        cx.notify();
    }

    /// Returns true if the user is allowed to change the text.
    ///
    /// This is false when the input is `disabled` or `readonly`, the programmatic
    /// APIs (e.g.: [`Self::set_value`], [`Self::insert`]) are not limited by this.
    pub fn is_editable(&self) -> bool {
        !self.disabled && !self.readonly
    }

    /// Set true to clear the input by pressing Escape key.
    pub fn clean_on_escape(mut self) -> Self {
        self.clean_on_escape = true;
        self
    }

    pub fn set_clean_on_escape(&mut self, clean: bool) {
        self.clean_on_escape = clean;
    }

    /// Set true to treat `Enter` as a submit action in multi-line mode,
    /// while `Shift+Enter` inserts a newline.
    ///
    /// Default is `false` (both `Enter` and `Shift+Enter` insert a newline).
    #[doc(hidden)]
    pub fn submit_on_enter(mut self, submit: bool) -> Self {
        self.submit_on_enter = submit;
        self
    }

    pub fn set_submit_on_enter(&mut self, submit: bool, cx: &mut Context<Self>) {
        self.submit_on_enter = submit;
        cx.notify();
    }

    /// Set whether to show whitespace characters.
    #[doc(hidden)]
    pub fn show_whitespaces(mut self, show: bool) -> Self {
        self.show_whitespaces = show;
        self
    }

    /// Update whether to show whitespace characters.
    pub fn set_show_whitespaces(&mut self, show: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.show_whitespaces = show;
        cx.notify();
    }

    /// Empty rows reserved below the last line of content ("scroll
    /// beyond last line"), code-editor mode only. Mirrors VSCode's
    /// `editor.scrollBeyondLastLine` / Zed's `scroll_beyond_last_line`.
    ///
    /// - `None` (default): half the viewport, floored at
    ///   [`BOTTOM_MARGIN_ROWS`] line-heights.
    /// - `Some(0)`: no trailing space; the cursor sits flush with the
    ///   last row at scroll-max.
    /// - `Some(n)`: exactly `n` rows.
    pub fn scroll_beyond_last_line(mut self, rows: Option<usize>) -> Self {
        self.scroll_beyond_last_line = rows;
        self
    }

    /// Update [`Self::scroll_beyond_last_line`] after construction.
    pub fn set_scroll_beyond_last_line(
        &mut self,
        rows: Option<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.scroll_beyond_last_line == rows {
            return;
        }
        self.scroll_beyond_last_line = rows;
        cx.notify();
    }

    /// Minimum number of lines the cursor is kept clear of the viewport's
    /// top/bottom edge before auto-scroll engages. Mirrors VSCode's
    /// `editor.cursorSurroundingLines` / Zed's `vertical_scroll_margin`.
    /// Orthogonal to [`Self::scroll_beyond_last_line`], which sizes the
    /// empty region; this controls the cursor's resting distance from the
    /// edge.
    ///
    /// - `None` (default): [`BOTTOM_MARGIN_ROWS`] lines, falling back to
    ///   one line on small viewports.
    /// - `Some(n)`: exactly `n` lines, clamped to half the viewport.
    pub fn cursor_surrounding_lines(mut self, lines: Option<usize>) -> Self {
        self.cursor_surrounding_lines = lines;
        self
    }

    /// Update [`Self::cursor_surrounding_lines`] after construction.
    pub fn set_cursor_surrounding_lines(
        &mut self,
        lines: Option<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.cursor_surrounding_lines == lines {
            return;
        }
        self.cursor_surrounding_lines = lines;
        cx.notify();
    }

    /// Set the default value of the input field.
    pub fn default_value(mut self, value: impl Into<SharedString>) -> Self {
        let text: SharedString = value.into();
        self.text = Rope::from(self.normalize_input(&text).as_ref());
        if let Some(diagnostics) = self.mode.diagnostics_mut() {
            diagnostics.reset(&self.text)
        }
        // Note: We can't call display_map.set_text here because it needs cx.
        // The text will be set during prepare_if_need in element.rs
        self._pending_update = true;
        self
    }

    /// Return the value of the input field as an owned string.
    ///
    /// The string is materialized on each call. See [`Self::text`] for the
    /// [`Rope`] the state owns, which is borrowed and costs nothing to read.
    pub fn value(&self) -> SharedString {
        SharedString::new(self.text.to_string())
    }

    /// Return the portion of the value within the input field that
    /// is selected by the user, as an owned string.
    ///
    /// The string is materialized on each call. See [`Self::selected_text`]
    /// for the same selection borrowed out of the [`Rope`] the state owns.
    pub fn selected_value(&self) -> SharedString {
        SharedString::new(self.selected_text().to_string())
    }

    /// Return the value without mask.
    pub fn unmask_value(&self) -> SharedString {
        self.mask_pattern.unmask(&self.text.to_string()).into()
    }

    /// Kept so existing render paths keep compiling.
    ///
    /// Configuration used to be collected by a facade and applied here; the
    /// state now configures itself, so this does nothing and can be deleted at
    /// the call site.
    #[doc(hidden)]
    pub fn prepare(&mut self, _: &mut Window, _: &mut Context<Self>) {}

    /// Return the text [`Rope`] of the input field.
    ///
    /// Borrowed from the state, so reading even a large document copies
    /// nothing. See [`Self::value`] when an owned string is wanted.
    pub fn text(&self) -> &Rope {
        &self.text
    }

    /// Return the (0-based) [`Position`] of the cursor.
    pub fn cursor_position(&self) -> Position {
        let offset = self.cursor();
        self.text.offset_to_position(offset)
    }

    /// Set (0-based) [`Position`] of the cursor.
    ///
    /// This will move the cursor to the specified line and column, and update the selection range.
    pub fn set_cursor_position(
        &mut self,
        position: impl Into<Position>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position: Position = position.into();
        let offset = self.text.position_to_offset(&position);

        self.move_to(offset, None, cx);
        self.update_preferred_column();
        self.focus(window, cx);
    }

    /// Focus the input field.
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
        self.blink_cursor.update(cx, |cursor, cx| {
            cursor.start(cx);
        });
    }

    /// Refresh the input, so the next render re-runs syntax highlighting and
    /// the LSP providers, not just a redraw.
    ///
    /// Assigning the `lsp` providers (or other render-affecting state) at
    /// runtime does not take effect until the text next changes. Call this
    /// afterwards to force the refresh on the next render.
    ///
    /// ```ignore
    /// input.update(cx, |state, cx| {
    ///     state.extras.lsp.hover_provider = Some(provider);
    ///     state.refresh(cx);
    /// });
    /// ```
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self._pending_update = true;
        cx.notify();
    }

    pub(super) fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.undo_manager.break_transaction_coalescing();
        self.select_to(self.previous_boundary(self.cursor()), cx);
    }

    pub(super) fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.undo_manager.break_transaction_coalescing();
        self.select_to(self.next_boundary(self.cursor()), cx);
    }

    pub(super) fn select_up(&mut self, _: &SelectUp, window: &mut Window, cx: &mut Context<Self>) {
        self.select_vertical(-1, window, cx);
    }

    pub(super) fn select_down(&mut self, _: &SelectDown, window: &mut Window, cx: &mut Context<Self>) {
        self.select_vertical(1, window, cx);
    }

    pub(super) fn on_action_select_all(
        &mut self,
        _: &SelectAll,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_all(window, cx);
    }

    pub(super) fn select_to_start(
        &mut self,
        _: &SelectToStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.undo_manager.break_transaction_coalescing();
        self.select_to(0, cx);
    }

    pub(super) fn select_to_end(
        &mut self,
        _: &SelectToEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.undo_manager.break_transaction_coalescing();
        let end = self.text.len();
        self.select_to(end, cx);
    }

    pub(super) fn select_to_start_of_line(
        &mut self,
        _: &SelectToStartOfLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.undo_manager.break_transaction_coalescing();
        let offset = self.start_of_line();
        self.select_to(offset, cx);
    }

    pub(super) fn select_to_end_of_line(
        &mut self,
        _: &SelectToEndOfLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.undo_manager.break_transaction_coalescing();
        let offset = self.end_of_line();
        // Mirrors MoveEnd: the caret belongs at the end of the visual row it is on.
        self.select_to_with_affinity(offset, true, cx);
    }

    pub(super) fn select_to_previous_word(
        &mut self,
        _: &SelectToPreviousWordStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.undo_manager.break_transaction_coalescing();
        let offset = self.previous_start_of_word();
        self.select_to(offset, cx);
    }

    pub(super) fn select_to_next_word(
        &mut self,
        _: &SelectToNextWordEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.undo_manager.break_transaction_coalescing();
        let offset = self.next_end_of_word();
        self.select_to(offset, cx);
    }

    /// Return the start offset of the previous word.
    pub(super) fn previous_start_of_word(&mut self) -> usize {
        if self.masked {
            // The mask replaces every character, so the displayed text has no
            // word boundaries to move or delete by. Collapse the word to the
            // whole text.
            return 0;
        }

        let offset = self.selected_range.start;
        let offset = self.offset_from_utf16(self.offset_to_utf16(offset));
        // FIXME: Avoid to_string
        let left_part = self.text.slice(0..offset).to_string();

        UnicodeSegmentation::split_word_bound_indices(left_part.as_str())
            .rfind(|(_, s)| !s.trim_start().is_empty())
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// Return the next end offset of the next word.
    pub(super) fn next_end_of_word(&mut self) -> usize {
        if self.masked {
            // See `previous_start_of_word`.
            return self.text.len();
        }

        let offset = self.cursor();
        let offset = self.offset_from_utf16(self.offset_to_utf16(offset));
        let right_part = self.text.slice(offset..self.text.len()).to_string();

        UnicodeSegmentation::split_word_bound_indices(right_part.as_str())
            .find(|(_, s)| !s.trim_start().is_empty())
            .map(|(i, s)| offset + i + s.len())
            .unwrap_or(self.text.len())
    }

    /// Get start of line byte offset of cursor.
    ///
    /// When soft wrap is active, first press goes to visual line start,
    /// second press (already at visual start) goes to logical line start.
    pub(super) fn start_of_line(&self) -> usize {
        if self.is_single_line() {
            return 0;
        }

        let row = self.text.offset_to_point(self.cursor()).row;
        let logical_start = self.text.line_start_offset(row);

        if self.soft_wrap && self.is_code_editor() {
            let wrap_point = self.display_map.offset_to_wrap_display_point_with_affinity(
                self.cursor(),
                self.cursor_line_end_affinity,
            );
            if let Some(line) = self.display_map.line(row)
                && let Some(range) = line.wrapped_lines.get(wrap_point.local_row)
            {
                let visual_start = logical_start + range.start;
                if self.cursor() != visual_start {
                    return visual_start;
                }
            }
        }

        logical_start
    }

    /// Get end of line byte offset of cursor.
    ///
    /// When soft wrap is active, first press goes to visual line end,
    /// second press (already at visual end) goes to logical line end.
    pub(super) fn end_of_line(&self) -> usize {
        if self.is_single_line() {
            return self.text.len();
        }

        let row = self.text.offset_to_point(self.cursor()).row;
        let logical_start = self.text.line_start_offset(row);
        let logical_end = self.text.line_end_offset(row);

        if self.soft_wrap && self.is_code_editor() {
            // Use the row the caret is drawn on: at a wrap boundary the raw offset would name
            // the next row, and a second End press would keep walking down instead of falling
            // through to the logical line end.
            let wrap_point = self.display_map.offset_to_wrap_display_point_with_affinity(
                self.cursor(),
                self.cursor_line_end_affinity,
            );
            if let Some(line) = self.display_map.line(row)
                && let Some(range) = line.wrapped_lines.get(wrap_point.local_row)
            {
                let visual_end = logical_start + range.end;
                if self.cursor() != visual_end {
                    return visual_end;
                }
            }
        }

        logical_end
    }

    /// Get start line of selection start or end (The min value).
    ///
    /// This is means is always get the first line of selection.
    pub(super) fn start_of_line_of_selection(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        if self.is_single_line() {
            return 0;
        }

        let mut offset =
            self.previous_boundary(self.selected_range.start.min(self.selected_range.end));
        if self.text.char_at(offset) == Some('\r') {
            offset += 1;
        }

        let line = self
            .text_for_range(self.range_to_utf16(&(0..offset + 1)), &mut None, window, cx)
            .unwrap_or_default()
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        line
    }

    /// Get indent string of next line.
    ///
    /// To get current and next line indent, to return more depth one.
    pub(super) fn indent_of_next_line(&mut self) -> String {
        if self.is_single_line() {
            return "".into();
        }

        let mut current_indent = String::new();
        let mut next_indent = String::new();
        let current_line_start_pos = self.start_of_line();
        let next_line_start_pos = self.end_of_line();
        for c in self.text.slice(current_line_start_pos..).chars() {
            if !c.is_whitespace() {
                break;
            }
            if c == '\n' || c == '\r' {
                break;
            }
            current_indent.push(c);
        }

        for c in self.text.slice(next_line_start_pos..).chars() {
            if !c.is_whitespace() {
                break;
            }
            if c == '\n' || c == '\r' {
                break;
            }
            next_indent.push(c);
        }

        if next_indent.len() > current_indent.len() {
            return next_indent;
        } else {
            return current_indent;
        }
    }

    pub(super) fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        let intent = if self.selected_range.is_empty() {
            self.select_to(self.backspace_boundary(self.cursor()), cx);
            EditIntent::Backspace
        } else {
            EditIntent::Atomic
        };
        self.undo_manager.pending_intent = Some(intent);
        self.edit_text_in_range(None, "", window, cx);
        self.pause_blink_cursor(cx);
    }

    pub(super) fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        let intent = if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor()), cx);
            EditIntent::DeleteForward
        } else {
            EditIntent::Atomic
        };
        self.undo_manager.pending_intent = Some(intent);
        self.edit_text_in_range(None, "", window, cx);
        self.pause_blink_cursor(cx);
    }

    pub(super) fn delete_to_beginning_of_line(
        &mut self,
        _: &DeleteToBeginningOfLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selected_range.is_empty() {
            self.edit_text_in_range(None, "", window, cx);
            self.pause_blink_cursor(cx);
            return;
        }

        let mut offset = self.start_of_line();
        if offset == self.cursor() {
            offset = self.offset_before(offset);
        }
        self.replace_text_in_range_silent(
            Some(self.range_to_utf16(&(offset..self.cursor()))),
            "",
            window,
            cx,
        );
        self.reveal_typed_caret(cx);
        self.pause_blink_cursor(cx);
    }

    pub(super) fn delete_to_end_of_line(
        &mut self,
        _: &DeleteToEndOfLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selected_range.is_empty() {
            self.edit_text_in_range(None, "", window, cx);
            self.pause_blink_cursor(cx);
            return;
        }

        let mut offset = self.end_of_line();
        if offset == self.cursor() {
            offset = self.offset_after(offset);
        }
        self.replace_text_in_range_silent(
            Some(self.range_to_utf16(&(self.cursor()..offset))),
            "",
            window,
            cx,
        );
        self.reveal_typed_caret(cx);
        self.pause_blink_cursor(cx);
    }

    pub(super) fn delete_previous_word(
        &mut self,
        _: &DeleteToPreviousWordStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selected_range.is_empty() {
            self.edit_text_in_range(None, "", window, cx);
            self.pause_blink_cursor(cx);
            return;
        }

        let offset = self.previous_start_of_word();
        self.replace_text_in_range_silent(
            Some(self.range_to_utf16(&(offset..self.cursor()))),
            "",
            window,
            cx,
        );
        self.reveal_typed_caret(cx);
        self.pause_blink_cursor(cx);
    }

    pub(super) fn delete_next_word(
        &mut self,
        _: &DeleteToNextWordEnd,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selected_range.is_empty() {
            self.edit_text_in_range(None, "", window, cx);
            self.pause_blink_cursor(cx);
            return;
        }

        let offset = self.next_end_of_word();
        self.replace_text_in_range_silent(
            Some(self.range_to_utf16(&(self.cursor()..offset))),
            "",
            window,
            cx,
        );
        self.reveal_typed_caret(cx);
        self.pause_blink_cursor(cx);
    }

    pub(super) fn enter(&mut self, action: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        if M::handle_context_menu_action(self, Box::new(action.clone()), window, cx) {
            return;
        }

        // Clear inline completion on enter (user chose not to accept it)
        if M::has_inline_completion(self) {
            M::clear_inline_completion(self, cx);
        }

        // In multi-line mode with `submit_on_enter` enabled, a plain `Enter`
        // (without Shift) is treated as submit: propagate the action and emit
        // PressEnter without inserting a newline. `Shift+Enter` still inserts
        // a newline.
        let insert_newline = self.is_multi_line() && (!self.submit_on_enter || action.shift);

        if insert_newline {
            // Get current line indent
            let indent = if self.is_code_editor() {
                self.indent_of_next_line()
            } else {
                "".to_string()
            };

            // Add newline and indent
            let new_line_text = format!("\n{}", indent);
            self.replace_text_in_range_silent(None, &new_line_text, window, cx);
            self.reveal_typed_caret(cx);
            self.pause_blink_cursor(cx);
        } else {
            // Single line input or submit-on-enter: just emit the event
            // (e.g.: in a dialog to confirm, or a chat textarea to send).
            self.undo_manager.break_transaction_coalescing();
            cx.propagate();
        }

        cx.emit(InputEvent::PressEnter {
            secondary: action.secondary,
            shift: action.shift,
        });
    }

    pub fn clean(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text("", window, cx);
        self.selected_range = (0..0).into();
        self.scroll_to(0, None, cx);
    }

    pub(super) fn escape(&mut self, action: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        if M::handle_context_menu_action(self, Box::new(action.clone()), window, cx) {
            return;
        }

        // Clear inline completion on escape
        if M::has_inline_completion(self) {
            M::clear_inline_completion(self, cx);
            return; // Consume the escape, don't propagate
        }

        if self.ime_marked_range.is_some() {
            self.unmark_text(window, cx);
        }

        if self.clean_on_escape {
            return self.clean(window, cx);
        }

        cx.propagate();
    }

    /// Show the right-click context menu as a native OS menu.
    pub(crate) fn handle_right_click_menu(
        &mut self,
        position: Point<Pixels>,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }
        if crate::GlobalState::is_in_deferred_context(cx) {
            return;
        }

        if !self.selected_range.contains(offset) {
            self.move_to(offset, None, cx);
        }

        if self.is_code_editor() {
            M::on_hover_definition(self, offset, window, cx);
        }

        if let Some(handler) = self.context_menu_handler.clone() {
            let capabilities = self.context_menu_capabilities();
            cx.defer_in(window, move |_, window, cx| {
                handler(NativeMenu::new(), capabilities, position, window, cx);
            });
        }
    }

    pub(super) fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.undo_manager.break_transaction_coalescing();
        // Input has its own text selection; suppress the window-level text
        // selection (Root) so it does not start a drag from here.
        crate::global_state::GlobalState::suppress_text_selection(cx);

        // Clear inline completion on any mouse interaction
        M::clear_inline_completion(self, cx);

        // If there have IME marked range and is empty (Means pressed Esc to abort IME typing)
        // Clear the marked range.
        if let Some(ime_marked_range) = &self.ime_marked_range {
            if ime_marked_range.len() == 0 {
                self.ime_marked_range = None;
            }
        }

        self.selecting = true;
        let (offset, line_end_affinity) = self.index_for_mouse_position(event.position);

        if M::on_click(self, event, offset, window, cx) {
            return;
        }

        // Triple click to select line
        if event.button == MouseButton::Left && event.click_count >= 3 {
            self.select_line(offset, window, cx);
            return;
        }

        // Double click to select word
        if event.button == MouseButton::Left && event.click_count == 2 {
            self.select_word(offset, window, cx);
            return;
        }

        // Show Mouse context menu
        if event.button == MouseButton::Right {
            if self.enable_context_menu {
                if !self.selected_range.contains(offset) {
                    self.move_to(offset, None, cx);
                }
                self.pending_context_menu = Some((event.position, offset));
            }
            return;
        }

        if event.modifiers.shift {
            self.select_to_with_affinity(offset, line_end_affinity, cx);
        } else {
            self.move_to_with_affinity(offset, None, line_end_affinity, cx)
        }
    }

    pub(super) fn on_mouse_up(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button == MouseButton::Right {
            if let Some((position, offset)) = self.pending_context_menu.take() {
                self.handle_right_click_menu(position, offset, window, cx);
            }
        }
        if self.selected_range.is_empty() {
            self.selection_reversed = false;
        }
        self.selecting = false;
        self.selected_word_range = None;
        self.auto_scroll.stop();
    }

    pub(super) fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Check if mouse is within bounds. `last_bounds` carries the scroll
        // offset in its origin (see `index_for_mouse_position`), so testing
        // against it loses the part of the view the scroll moved up: hover
        // and Cmd-hover stopped working below it, and everywhere once the
        // file was scrolled further than its own height. `input_bounds` is
        // the same rectangle as painted, without the offset.
        let within_bounds = self.input_bounds.contains(&event.position);

        if !within_bounds {
            // Clear hover when mouse leaves the input
            M::clear_hover_state(self, cx);
            return;
        }

        // Show diagnostic popover on mouse move
        let (offset, _) = self.index_for_mouse_position(event.position);
        M::on_mouse_move(self, offset, event, window, cx);

        if self.is_code_editor() {
            if let Some(diagnostic) = self
                .mode
                .diagnostics()
                .and_then(|set| set.for_offset(offset))
            {
                self.diagnostic_popover = Some(Rc::new(diagnostic.clone()));
                cx.notify();
            } else {
                self.diagnostic_popover = None;
            }
        }
    }

    pub(super) fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_height = self
            .last_layout
            .as_ref()
            .map(|layout| layout.line_height)
            .unwrap_or(window.line_height());
        let delta = event.delta.pixel_delta(line_height);

        let old_offset = self.scroll_handle.offset();
        self.update_scroll_offset(Some(old_offset + delta), cx);

        // Only stop propagation if the offset actually changed
        if self.scroll_handle.offset() != old_offset {
            cx.stop_propagation();
        }

        // put away by the wheel whether it scrolled or not
        if self.diagnostic_popover.take().is_some() {
            cx.notify();
        }
    }

    /// Scroll to `offset`, or stay, kept to the text's extent; told to
    /// whatever observes the state only where the offset changed, as the
    /// paint persists it in every frame (elundqvist/kvist#602).
    pub(super) fn update_scroll_offset(
        &mut self,
        offset: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let mut offset = offset.unwrap_or(self.scroll_handle.offset());
        // In addition to left alignment, a cursor position will be reserved on the right side
        let safe_x_offset = if self.text_align == TextAlign::Left {
            px(0.)
        } else {
            -CURSOR_WIDTH
        };

        let safe_y_range =
            (-self.scroll_size.height + self.input_bounds.size.height).min(px(0.0))..px(0.);
        let safe_x_range = (-self.scroll_size.width + self.input_bounds.size.width + safe_x_offset)
            .min(safe_x_offset)..px(0.);

        offset.y = if self.is_single_line() {
            px(0.)
        } else {
            offset.y.clamp(safe_y_range.start, safe_y_range.end)
        };
        offset.x = offset.x.clamp(safe_x_range.start, safe_x_range.end);
        if self.scroll_handle.offset() == offset {
            return;
        }
        self.scroll_handle.set_offset(offset);
        cx.notify();
    }

    /// Scroll to make the given offset visible.
    ///
    /// If `direction` is Some, will keep edges at the same side.
    pub(crate) fn scroll_to(
        &mut self,
        offset: usize,
        direction: Option<MoveDirection>,
        cx: &mut Context<Self>,
    ) {
        let Some(last_layout) = self.last_layout.as_ref() else {
            return;
        };
        let Some(bounds) = self.last_bounds.as_ref() else {
            return;
        };

        let mut scroll_offset = self.scroll_handle.offset();
        let was_offset = scroll_offset;
        let line_height = last_layout.line_height;

        let point = self.text.offset_to_point(offset);

        let row = point.row;

        // Calculate row offset by multiplying the number of lines before it with the line height
        let mut row_offset_y = line_height * self.display_map.buffer_line_to_display_row(row);

        // For Right alignment use 0 margin: the cursor indicator is clamped inside bounds
        // in layout_cursor, so shifting the text here would cause a first-click visual jump.
        let safety_margin = match last_layout.text_align {
            TextAlign::Left => RIGHT_MARGIN,
            TextAlign::Right => px(0.),
            TextAlign::Center => CURSOR_WIDTH,
        };
        // by the lines laid out, which skip what a fold hides
        if let Some(line) = last_layout.line(row) {
            // Check to scroll horizontally and soft wrap lines
            if let Some(pos) = line.position_for_index(point.column, last_layout, false) {
                let bounds_width = bounds.size.width - last_layout.line_number_width;
                let col_offset_x = pos.x;
                row_offset_y += pos.y;
                if col_offset_x - safety_margin < -scroll_offset.x {
                    // If the position is out of the visible area, scroll to make it visible
                    scroll_offset.x = -col_offset_x + safety_margin;
                } else if col_offset_x + safety_margin > -scroll_offset.x + bounds_width {
                    scroll_offset.x = -(col_offset_x - bounds_width + safety_margin);
                }
            }
        }

        // Scroll the row into view. Use the same edge clearance helper as
        // `TextElement::layout_cursor` so both scroll-into-view paths agree
        // (a mismatch flickered on `Down` at end-of-buffer with a small
        // `cursor_surrounding_lines` override).
        let edge_height = if direction.is_some() && self.is_code_editor() {
            super::element::cursor_surrounding_padding(
                self.mode.is_auto_grow(),
                self.cursor_surrounding_lines,
                last_layout.visible_range.len(),
                line_height,
            )
        } else {
            line_height
        };
        if row_offset_y - edge_height + line_height < -scroll_offset.y {
            // Scroll up
            scroll_offset.y = -row_offset_y + edge_height - line_height;
        } else if row_offset_y + edge_height > -scroll_offset.y + bounds.size.height {
            // Scroll down
            scroll_offset.y = -(row_offset_y - bounds.size.height + edge_height);
        }

        // Avoid necessary scroll, when it was already in the correct position.
        if direction == Some(MoveDirection::Up) {
            scroll_offset.y = scroll_offset.y.max(was_offset.y);
        } else if direction == Some(MoveDirection::Down) {
            scroll_offset.y = scroll_offset.y.min(was_offset.y);
        }

        // Clamp the deferred target into the same safe range that
        // `update_scroll_offset` enforces on persist, so paint never shows an
        // over-scrolled frame before the post-paint clamp pulls it back. The
        // range is the text's as it is now: the last layout's scroll size is
        // older than a paste, and a paste past the end stopped the view at
        // the height the text had before it.
        let safe_y_min =
            (-self.scroll_height_now(line_height) + self.input_bounds.size.height).min(px(0.));
        scroll_offset.x = scroll_offset.x.min(px(0.));
        scroll_offset.y = scroll_offset.y.clamp(safe_y_min, px(0.));
        self.deferred_scroll_offset = Some(scroll_offset);
        self.deferred_scroll_follows_caret = false;
        cx.notify();
    }

    /// Scroll to `offset` after an edit, before the edited text is laid
    /// out: down to its row, which the text as it is gives, and across as
    /// far as the layout's follow of the caret goes, which reads the column
    /// from the lines it lays out. [`Self::scroll_to`] reads it from the
    /// last layout's, where the caret's line may be shorter than its
    /// column now: an undo that gave a line back its tail left the caret
    /// off to the right of the view (elundqvist/kvist#201).
    fn scroll_to_edited(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.scroll_to(offset, None, cx);
        self.deferred_scroll_follows_caret = self.deferred_scroll_offset.is_some();
    }

    /// Bring the caret into view where a key typed at it left it out of
    /// the lines laid out, before the next layout chooses its lines, as
    /// VS Code and IntelliJ scroll to the caret at the first key typed.
    /// Left to the layout's follow of the caret, the view went a line a
    /// key towards a caret scrolled far out of sight, as after the find
    /// bar had shown a match, and the caret never came into view
    /// (elundqvist/kvist#369). Text typed, Enter, a cut and the deletes
    /// and indents at the caret bring it here; left to the follow, which
    /// now goes to it at once, the frame laid out the lines at the old
    /// place and painted them at the new, blank for a frame.
    pub(crate) fn reveal_typed_caret(&mut self, cx: &mut Context<Self>) {
        let Some(rows) = self.visible_row_range() else {
            return;
        };
        let row = self.text.offset_to_point(self.cursor()).row;
        if !rows.contains(&row) {
            self.scroll_to_edited(self.cursor(), cx);
        }
    }

    /// How far the text as it is now scrolls, as the next layout will
    /// measure it: its rows on screen, without those a fold hides, and the
    /// room kept below the last one. `scroll_size` is the last layout's,
    /// which has not seen an edit since.
    fn scroll_height_now(&self, line_height: Pixels) -> Pixels {
        let below = super::element::empty_bottom_height(
            self.is_code_editor(),
            self.scroll_beyond_last_line,
            self.input_bounds.size.height,
            line_height,
        );
        (line_height * self.display_map.display_row_count() as f32 + below)
            .max(self.input_bounds.size.height)
    }

    pub(super) fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    pub(super) fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.is_copyable() {
            return;
        }

        let selected_text = self.text.slice(self.selected_range).to_string();
        cx.write_to_clipboard(ClipboardItem::new_string(selected_text));
    }

    pub(super) fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_copyable() {
            return;
        }

        let selected_text = self.text.slice(self.selected_range).to_string();
        cx.write_to_clipboard(ClipboardItem::new_string(selected_text));

        self.undo_manager.pending_intent = Some(EditIntent::Atomic);
        self.replace_text_in_range_silent(None, "", window, cx);
        self.reveal_typed_caret(cx);
    }

    /// Paste what the clipboard holds, read as [`read_clipboard_for_paste`]
    /// reads it.
    pub(super) fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        read_clipboard_for_paste(self, window, cx, Self::paste_item);
    }

    /// Paste the text `item` holds. A clipboard with no text, an image
    /// alone, leaves the text and its selection as they are, as VS Code
    /// and IntelliJ leave them: it replaced the selection with nothing.
    fn paste_item(
        &mut self,
        item: Option<ClipboardItem>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(new_text) = item.and_then(|item| item.text()) else {
            return;
        };
        self.undo_manager.pending_intent = Some(EditIntent::Atomic);
        self.replace_text_in_range_silent(None, &new_text, window, cx);
        self.scroll_to_edited(self.cursor(), cx);
    }

    fn push_history(
        &mut self,
        text: &Rope,
        range: &Range<usize>,
        new_text: &str,
        requested_intent: Option<EditIntent>,
        selection_before: Selection,
        selection_after: Option<Selection>,
    ) {
        if self.undo_manager.is_ignoring() {
            return;
        }

        let range =
            text.clip_offset(range.start, Bias::Left)..text.clip_offset(range.end, Bias::Right);
        let old_text = text.slice(range.clone()).to_string();
        let new_range = range.start..range.start + new_text.len();

        let intent = requested_intent.unwrap_or_else(|| {
            if range.is_empty()
                && old_text.is_empty()
                && !new_text.is_empty()
                && !new_text.contains(['\n', '\r'])
            {
                EditIntent::Typing
            } else {
                EditIntent::Atomic
            }
        });

        let selection_before = match intent {
            EditIntent::Backspace => Selection::new(range.end, range.end),
            EditIntent::DeleteForward => Selection::new(range.start, range.start),
            EditIntent::Typing | EditIntent::Atomic => selection_before,
        };
        let selection_after =
            selection_after.unwrap_or_else(|| Selection::new(new_range.end, new_range.end));

        self.undo_manager.record_transaction(
            Change::new(
                range,
                &old_text,
                new_range,
                new_text,
                selection_before,
                selection_after,
            ),
            intent,
        );
    }

    pub(super) fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        // an edit of the input's own, past a composition (elundqvist/kvist#482)
        self.let_go_of_composition(window, cx);
        self.undo_manager.set_ignoring(true);
        if let Some(changes) = self.undo_manager.undo() {
            let selection = changes.last().unwrap().selection_before;
            let steps: Vec<(Range<usize>, &str)> = changes
                .iter()
                .map(|change| (change.new_range.into(), change.old_text.as_str()))
                .collect();
            self.replay(&steps, window, cx);
            self.selected_range = selection;
            self.open_folds_hiding(self.cursor());
            // before the next layout chooses its lines: left to the
            // caret's follow in the layout, the lines were those at the
            // old place and painted at the new, and an undone paste of
            // thousands of lines left the editor blank until the next frame
            self.scroll_to_edited(self.cursor(), cx);
        }
        self.undo_manager.set_ignoring(false);
    }

    pub(super) fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        self.let_go_of_composition(window, cx);
        self.undo_manager.set_ignoring(true);
        if let Some(changes) = self.undo_manager.redo() {
            let selection = changes.last().unwrap().selection_after;
            let steps: Vec<(Range<usize>, &str)> = changes
                .iter()
                .map(|change| (change.old_range.into(), change.new_text.as_str()))
                .collect();
            self.replay(&steps, window, cx);
            self.selected_range = selection;
            self.open_folds_hiding(self.cursor());
            self.scroll_to_edited(self.cursor(), cx);
        }
        self.undo_manager.set_ignoring(false);
    }

    /// Open the folds that hide the line of `offset`, every one where they
    /// nest: an undo or a redo puts the caret back where it was, which may
    /// be in a fold made since, which the edit no longer opens
    /// (elundqvist/kvist#404), as VS Code opens a fold its caret goes into.
    fn open_folds_hiding(&mut self, offset: usize) {
        let row = self.text.offset_to_point(offset.min(self.text.len())).row;
        let hiding: Vec<usize> = self
            .display_map
            .folded_ranges()
            .iter()
            .filter(|fold| row > fold.start_line && row < fold.end_line)
            .map(|fold| fold.start_line)
            .collect();
        for start_line in hiding {
            self.display_map.set_folded(start_line, false);
        }
    }

    /// Make a step of undo or redo, `steps` its changes in turn, each a
    /// range of the text as the ones before it left it and what goes
    /// there, as one edit, as VS Code and IntelliJ take an undo: the text
    /// takes each, and what follows the text (`follow_edit`) is handed one
    /// edit, from where the first begins to where the last ends, and one
    /// `Change`. Each was made as an edit of its own, so 28 keys typed in
    /// a large file and undone were parsed, folded and marked 28 times
    /// over, and the view answered 28 changes (elundqvist/kvist#368). The
    /// problems move with each change in turn, so one between two of them
    /// stays on its text (`DiagnosticSet::edit_steps`). A single line goes
    /// as typed, through its mask and its check of what it holds, as it
    /// is short.
    fn replay(&mut self, steps: &[(Range<usize>, &str)], window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_multi_line() {
            for (range, text) in steps {
                let range_utf16 = self.range_to_utf16(range);
                self.replace_text_in_range_silent(Some(range_utf16), text, window, cx);
            }
            return;
        }
        if steps.is_empty() {
            return;
        }
        if self.blink_cursor.read(cx).visible() {
            self.pause_blink_cursor(cx);
        }
        let old_text = self.text.clone();
        // what is alike before the first change and after the last
        let (mut head, mut tail) = (usize::MAX, usize::MAX);
        let mut made = Vec::with_capacity(steps.len());
        for (range, text) in steps {
            let range = self.text.clip_offset(range.start, Bias::Left)
                ..self.text.clip_offset(range.end, Bias::Right);
            // a change that puts back what is there changes nothing: the
            // undo of a paste of the text over itself opened every fold
            if puts_back(&self.text, &range, text) {
                continue;
            }
            head = head.min(range.start);
            tail = tail.min(self.text.len() - range.end);
            made.push((range.clone(), text.len()));
            self.text.replace(range, text);
        }
        if !made.is_empty() {
            let range = head..old_text.len() - tail;
            let new_text = self.text.slice(head..self.text.len() - tail).to_string();
            M::adjust_annotations(self, &made);
            self.follow_edit(&old_text, &range, &new_text, &made, window, cx);
        }
        self.ime_marked_range.take();
        self.preferred_column = None;
        self.update_search(cx);
        self.mode.update_auto_grow(&self.display_map);
        if self.emit_events {
            cx.emit(InputEvent::Change);
        }
        cx.notify();
    }

    /// Put each of `edits` in, ranges of the text as it is, apart and the
    /// last first (each ends at or before the start of the one before it
    /// in the list), with what goes in each, as one step of undo of a
    /// change apiece, and made as an undo makes its step (`replay`): the
    /// problems and the folds move with each, so one between two stays on
    /// its text, and the rest that follows the text takes one edit. A
    /// replace-all made the text anew in one change, so the underline of a
    /// problem between two matches went with it, and went again with the
    /// undo (elundqvist/kvist#370). The selection keeps to its text, as VS
    /// Code keeps it through a Replace All, and a redo of the step puts it
    /// there again: an end before an edit stays, one past it moves with
    /// it, and one within it goes to the end of what went there. It went
    /// to the end of the text, as the replace of the whole text this was
    /// put it, and a redo took the view there, to the last line of the
    /// file (elundqvist/kvist#383). A single line takes the edits as one
    /// change of the whole, through its mask and its check. As the other
    /// programmatic edits do, it edits a read-only or a disabled input too;
    /// the find bar's Replace All asks first (`is_replaceable`), and an
    /// application that replaces in the text it shows, as a project-wide
    /// replace does in the file open, makes its changes here so that a
    /// problem between two of them stays (elundqvist/kvist#383).
    pub fn replace_steps(
        &mut self,
        edits: &[(Range<usize>, &str)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if edits.is_empty() {
            return;
        }
        debug_assert!(
            edits.windows(2).all(|w| w[1].0.end <= w[0].0.start),
            "the edits are apart and the last first: {edits:?}"
        );
        // an edit of the input's own, past a composition (elundqvist/kvist#482)
        let moved: Vec<(Range<usize>, &str)>;
        let edits = match self.let_go_of_composition(window, cx) {
            Some(marked) => {
                moved = edits
                    .iter()
                    .map(|(range, text)| {
                        (
                            past_mark(&marked, range.start)..past_mark(&marked, range.end),
                            *text,
                        )
                    })
                    .collect();
                &moved[..]
            }
            None => edits,
        };
        self.with_edits_allowed(|this| this.replace_steps_allowed(edits, window, cx));
    }

    fn replace_steps_allowed(
        &mut self,
        edits: &[(Range<usize>, &str)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_multi_line() {
            let mut text = self.text.clone();
            for (range, new) in edits {
                text.replace(range.clone(), new);
            }
            let range = self.range_to_utf16(&(0..self.text.len()));
            self.replace_text_in_range_silent(Some(range), &text.to_string(), window, cx);
            return;
        }
        self.undo_manager.pending_intent.take();
        let before = self.selected_range;
        // where a place in the text is once the edits are in, the first of
        // them first
        let moved = |at: usize| {
            let mut grown = 0isize;
            for (range, new) in edits.iter().rev() {
                if at <= range.start {
                    break;
                }
                if at < range.end {
                    return (range.start as isize + grown) as usize + new.len();
                }
                grown += new.len() as isize - range.len() as isize;
            }
            (at as isize + grown).max(0) as usize
        };
        let after = Selection::new(moved(before.start), moved(before.end));
        // as undo keeps them: each on the text the ones before it left,
        // which is the text as it is where it begins, as they are the last
        // first
        let changes: Vec<Change> = edits
            .iter()
            .map(|(range, new)| {
                let range = self.text.clip_offset(range.start, Bias::Left)
                    ..self.text.clip_offset(range.end, Bias::Right);
                let old = self.text.slice(range.clone()).to_string();
                Change::new(range.clone(), &old, range.start..range.start + new.len(), new, before, after)
            })
            .collect();
        self.undo_manager.record_step(changes);
        self.replay(edits, window, cx);
        self.selected_range = after;
    }

    /// Get byte offset of the cursor.
    ///
    /// The offset is the UTF-8 offset.
    pub fn cursor(&self) -> usize {
        if let Some(ime_marked_range) = &self.ime_marked_range {
            return ime_marked_range.end;
        }

        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    /// Visible row range in the last laid-out viewport, `None` before first layout.
    pub fn visible_row_range(&self) -> Option<std::ops::Range<usize>> {
        self.last_layout.as_ref().map(|l| l.visible_range.clone())
    }

    /// Current scroll offset of the editor viewport.
    pub fn scroll_offset(&self) -> gpui::Point<gpui::Pixels> {
        self.scroll_handle.offset()
    }

    /// Set scroll offset of the editor viewport.
    ///
    /// The offset will be clamped to the valid range, and applied after the next layout.
    pub fn set_scroll_offset(&mut self, offset: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        self.deferred_scroll_offset = Some(offset);
        self.deferred_scroll_follows_caret = false;
        cx.notify();
    }

    /// Laid-out line height; `None` before first layout.
    pub fn line_height(&self) -> Option<gpui::Pixels> {
        self.last_layout.as_ref().map(|l| l.line_height)
    }

    /// Returns the current selection as a byte range into the text.
    ///
    /// The range is empty (`start == end`) when no text is selected; in
    /// that case the offset equals `cursor()`. Byte offsets are measured
    /// in the underlying rope's byte units.
    pub fn selected_range(&self) -> std::ops::Range<usize> {
        self.selected_range.into()
    }

    /// What an input method is composing, as a byte range into the text: in the
    /// text and underlined, but not typed yet, a dead key's accent before its letter
    /// or a preedit before it is committed. `None` where nothing is being composed, a
    /// composition Escape emptied among them. Marking emits no [`InputEvent::Change`],
    /// so an editor that saves as it is typed in asks this before it writes the text
    /// out: Kvist's wrote a dead key's ´ to disk while it was still composing
    /// (elundqvist/kvist#475).
    pub fn marked_range(&self) -> Option<std::ops::Range<usize>> {
        self.ime_marked_range
            .map(std::ops::Range::<usize>::from)
            .filter(|range| !range.is_empty())
    }

    pub fn select_all(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.undo_manager.break_transaction_coalescing();
        self.selected_range = (0..self.text.len()).into();
        cx.notify();
    }

    /// Set the selected range using UTF-8 byte offsets.
    ///
    /// Non-empty ranges expand to character boundaries. Empty ranges remain empty and are
    /// clipped to the preceding character boundary.
    pub fn set_selected_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let end_bias = if range.start == range.end {
            Bias::Left
        } else {
            Bias::Right
        };
        let start = self.text.clip_offset(range.start, Bias::Left);
        let end = self.text.clip_offset(range.end, end_bias);

        self.move_to(start, None, cx);
        self.selection_reversed = false;
        self.selected_word_range = None;
        self.select_to(end, cx);
    }

    /// Resolve a mouse position to a byte offset in the text.
    ///
    /// Also reports the caret's line-end affinity for that offset: `true` when the position
    /// landed on the wrap boundary of a non-final visual row, meaning the caret belongs at the
    /// end of that row rather than at the start of the next one. Callers that place or extend a
    /// selection must pass it on, or clicking past the last glyph of a wrapped row leaves a
    /// caret one row below the pointer.
    pub(crate) fn index_for_mouse_position(&self, position: Point<Pixels>) -> (usize, bool) {
        // If the text is empty, always return 0
        if self.text.len() == 0 {
            return (0, false);
        }

        let (Some(bounds), Some(last_layout)) =
            (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return (0, false);
        };

        let line_height = last_layout.line_height;
        let line_number_width = last_layout.line_number_width;

        // TIP: About the IBeam cursor
        //
        // If cursor style is IBeam, the mouse mouse position is in the middle of the cursor (This is special in OS)

        // The position is relative to the bounds of the text input
        //
        // bounds.origin:
        //
        // - included the input padding.
        // - included the scroll offset.
        let inner_position = position - bounds.origin - point(line_number_width, px(0.));

        let mut y_offset = last_layout.visible_top;

        // Traverse visible buffer lines (compact, no hidden entries)
        for (vi, (line_layout, _buffer_line)) in last_layout
            .lines
            .iter()
            .zip(last_layout.visible_buffer_lines.iter())
            .enumerate()
        {
            let line_start_offset = last_layout.visible_line_byte_offsets[vi];

            // Calculate line origin for this display row
            let line_origin = point(px(0.), y_offset);
            let pos = inner_position - line_origin;

            // Return offset by use closest_index_for_x if is single line mode.
            if self.is_single_line() {
                let local_index = line_layout.closest_index_for_x(pos.x, last_layout);
                // A single line never wraps, so there is no boundary to disambiguate.
                return (self.resolve_index(line_start_offset + local_index), false);
            }

            // Check if mouse is in this line's bounds
            if let Some((local_index, line_end_affinity)) =
                line_layout.closest_index_for_position(pos, last_layout)
            {
                return (
                    self.resolve_index(line_start_offset + local_index),
                    line_end_affinity,
                );
            } else if pos.y < px(0.) {
                // Mouse is above this line, return start of this line
                return (self.resolve_index(line_start_offset), false);
            }

            y_offset += line_layout.size(line_height).height;
        }

        // Mouse is below all visible lines, return end of text
        (self.text.len(), false)
    }

    /// Map a display byte index back to a text offset, undoing the mask expansion when the input
    /// is masked.
    fn resolve_index(&self, index: usize) -> usize {
        if self.masked {
            self.text.char_index_to_offset(index / MASK_CHAR.len_utf8())
        } else {
            index.min(self.text.len())
        }
    }

    /// Returns a y offsetted point for the line origin.
    /// Select the text from the current cursor position to the given offset.
    ///
    /// The offset is the UTF-8 offset.
    ///
    /// Ensure the offset use self.next_boundary or self.previous_boundary to get the correct offset.
    pub(crate) fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.select_to_with_affinity(offset, false, cx);
    }

    /// Like [`Self::select_to`], but also carries the caret's line-end affinity.
    ///
    /// See [`Self::move_to_with_affinity`] for why the affinity travels with the offset. Note
    /// that plain [`Self::select_to`] clears the affinity: every offset it is given came from
    /// the text rather than from a visual position, so the caret has no reason to keep sticking
    /// to the end of a wrapped row.
    pub(crate) fn select_to_with_affinity(
        &mut self,
        offset: usize,
        line_end_affinity: bool,
        cx: &mut Context<Self>,
    ) {
        M::clear_inline_completion(self, cx);

        self.cursor_line_end_affinity = line_end_affinity;
        let offset = offset.clamp(0, self.text.len());
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };

        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = (self.selected_range.end..self.selected_range.start).into();
        }

        // Ensure keep word selected range
        if let Some(word_range) = self.selected_word_range.as_ref() {
            if self.selected_range.start > word_range.start {
                self.selected_range.start = word_range.start;
            }
            if self.selected_range.end < word_range.end {
                self.selected_range.end = word_range.end;
            }
        }
        if self.selected_range.is_empty() {
            self.update_preferred_column();
        }
        cx.notify()
    }

    /// Unselects the currently selected text.
    pub fn unselect(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.undo_manager.break_transaction_coalescing();
        let offset = self.cursor();
        self.selected_range = (offset..offset).into();
        cx.notify()
    }

    #[inline]
    pub(super) fn offset_from_utf16(&self, offset: usize) -> usize {
        self.text.offset_utf16_to_offset(offset)
    }

    #[inline]
    pub(super) fn offset_to_utf16(&self, offset: usize) -> usize {
        self.text.offset_to_offset_utf16(offset)
    }

    #[inline]
    pub(crate) fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    #[inline]
    pub(super) fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    /// If offset falls on a hidden (folded) line, clamp backward to the end of
    /// the fold header line (last visible position before the fold).
    fn clamp_offset_to_visible_backward(&self, offset: usize) -> usize {
        let line = self.text.offset_to_point(offset).row;
        if self.display_map.is_buffer_line_hidden(line) {
            for fold in self.display_map.folded_ranges() {
                if line > fold.start_line && line <= fold.end_line {
                    return self.text.line_end_offset(fold.start_line);
                }
            }
        }
        offset
    }

    /// If offset falls on a hidden (folded) line, clamp forward to the start of
    /// the fold end line (first visible position after the fold).
    fn clamp_offset_to_visible_forward(&self, offset: usize) -> usize {
        let line = self.text.offset_to_point(offset).row;
        if self.display_map.is_buffer_line_hidden(line) {
            for fold in self.display_map.folded_ranges() {
                if line > fold.start_line && line <= fold.end_line {
                    return self.text.line_start_offset(fold.end_line);
                }
            }
        }
        offset
    }

    pub(super) fn previous_boundary(&self, offset: usize) -> usize {
        self.clamp_offset_to_visible_backward(self.offset_before(offset))
    }

    pub(super) fn next_boundary(&self, offset: usize) -> usize {
        self.clamp_offset_to_visible_forward(self.offset_after(offset))
    }

    /// Where Backspace at `offset` deletes back to: the whole of an emoji
    /// drawn from several characters, a family, a flag, a thumb and its
    /// colour, a keycap, and a `\r\n`; and of any other grapheme its last
    /// character alone, an accent typed after its letter, a vowel sign
    /// after its consonant, as a browser's field deletes, VS Code's boxes
    /// among them, and as Kvist's own line edit does (elundqvist/kvist#451):
    /// taking the whole grapheme took a Devanagari syllable for a vowel
    /// sign mistyped.
    pub(super) fn backspace_boundary(&self, offset: usize) -> usize {
        let offset = self.text.clip_offset(offset, Bias::Right);
        let from = self.offset_before(offset);
        let grapheme = self.text.slice(from..offset);
        let several = grapheme.chars().nth(1).is_some();
        let from = if several && grapheme != "\r\n" && !grapheme.chars().any(is_emoji_part) {
            self.text.clip_offset(offset - 1, Bias::Left)
        } else {
            from
        };
        self.clamp_offset_to_visible_backward(from)
    }

    /// The offset one grapheme before `offset`: what is drawn as one
    /// character is one step, a family emoji, a flag, a letter and its
    /// accent (elundqvist/kvist#456), where a step was a character and a
    /// family seven. A `\r\n` is one grapheme, so the caret never stands
    /// between its bytes: a step back over the `\n` lands before the `\r`.
    ///
    /// A masked input goes a character at a time, as it draws a bullet a
    /// character: a step a grapheme there crossed seven bullets at once
    /// for a family, and showed the shape of what it hides, which GTK's
    /// password entry steps a character at a time not to.
    fn offset_before(&self, offset: usize) -> usize {
        if self.masked {
            return self.text.clip_offset(offset.saturating_sub(1), Bias::Left);
        }
        self.text.grapheme_before(offset)
    }

    /// The offset one grapheme after `offset`: past the `\n` as well when
    /// the step crossed the `\r` of a `\r\n`. A masked input's is one
    /// character after it.
    fn offset_after(&self, offset: usize) -> usize {
        if self.masked {
            return self.text.clip_offset(offset + 1, Bias::Right);
        }
        self.text.grapheme_after(offset)
    }

    /// Returns the true to let InputElement to render cursor, when Input is focused and current BlinkCursor is visible.
    pub(crate) fn show_cursor(&self, window: &Window, cx: &App) -> bool {
        (self.focus_handle.is_focused(window) || M::is_context_menu_open(self, cx))
            && !self.disabled
            && self.blink_cursor.read(cx).visible()
            && window.is_window_active()
    }

    fn on_focus(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.blink_cursor.update(cx, |cursor, cx| {
            cursor.start(cx);
        });
        cx.emit(InputEvent::Focus);
    }

    fn on_blur(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if M::is_context_menu_open(self, cx) {
            return;
        }

        // A composition goes with the keyboard, and leaves this input as it
        // loses it, as the X11 client deletes one as the window loses the
        // keyboard, and as GTK's input method resets as its widget loses the
        // focus, a dead key's sequence ending with it. A dead key's ´ stayed
        // marked here after a shortcut that moved the focus, the shortcut now
        // passing the sequence by, its é typed in the input the shortcut
        // opened: the mark had no composition behind it, and what was typed
        // here next took its place (elundqvist/kvist#482).
        self.let_go_of_composition(window, cx);

        self.undo_manager.break_transaction_coalescing();

        // NOTE: Do not cancel select, when blur.
        // Because maybe user want to copy the selected text by AppMenuBar (will take focus handle).

        M::clear_hover_state(self, cx);
        self.diagnostic_popover = None;
        M::clear_inline_completion(self, cx);
        self.blink_cursor.update(cx, |cursor, cx| {
            cursor.stop(cx);
        });
        self.clamp_number_value(window, cx);
        cx.emit(InputEvent::Blur);
        cx.notify();
    }

    /// Clamp the number value to the `min`/`max` range, used on blur.
    ///
    /// Out-of-range values are allowed while typing (e.g. `1` is an
    /// intermediate state of `15` when min is 10), and clamped on blur.
    fn clamp_number_value(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_single_line() {
            return;
        }
        if !matches!(self.mask_pattern, MaskPattern::Number { .. }) {
            return;
        }
        if self.number_min.is_none() && self.number_max.is_none() {
            return;
        }

        let Ok(value) = self.unmask_value().parse::<f64>() else {
            return;
        };

        let clamped = match (self.number_min, self.number_max) {
            (Some(min), _) if value < min => min,
            (_, Some(max)) if value > max => max,
            _ => return,
        };

        // The clamped value must pass the `pattern`/`validate` check,
        // otherwise keep the value as is.
        let new_text = clamped.to_string();
        if !self.is_valid_input(&new_text, cx) {
            return;
        }

        let range = self.range_to_utf16(&(0..self.text.len()));
        self.replace_text_in_range_silent(Some(range), &new_text, window, cx);
    }

    pub(super) fn pause_blink_cursor(&mut self, cx: &mut Context<Self>) {
        self.blink_cursor.update(cx, |cursor, cx| {
            cursor.pause(cx);
        });
    }

    pub(super) fn on_key_down(&mut self, _: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.pause_blink_cursor(cx);
    }

    pub(super) fn on_drag_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.text.len() == 0 {
            return;
        }

        if self.last_layout.is_none() {
            return;
        }

        if !self.focus_handle.is_focused(window) {
            return;
        }

        if !self.selecting {
            return;
        }

        self.auto_scroll.last_drag_position = Some(event.position);
        let (offset, line_end_affinity) = self.index_for_mouse_position(event.position);
        self.select_to_with_affinity(offset, line_end_affinity, cx);

        if !self.is_single_line() {
            let delta = AutoScroll::compute_delta(event.position.y, self.input_bounds);
            // Input's ScrollHandle uses negative-y-is-down; negate the positive-towards-bottom delta.
            let scroll_delta = delta.map(|d| -d);
            self.auto_scroll.set(scroll_delta, cx, |delta, state, cx| {
                let current = state.scroll_handle.offset();
                state.update_scroll_offset(Some(point(current.x, current.y + delta)), cx);
                if let Some(pos) = state.auto_scroll.last_drag_position {
                    let (offset, line_end_affinity) = state.index_for_mouse_position(pos);
                    state.select_to_with_affinity(offset, line_end_affinity, cx);
                }
            });
        }
    }

    /// Normalize the inserted text before applying it to the input.
    ///
    /// For number inputs (with [`MaskPattern::Number`]), this converts
    /// full-width number characters into their ASCII equivalents,
    /// e.g. `12。5` -> `12.5`.
    fn normalize_input<'a>(&self, new_text: &'a str) -> Cow<'a, str> {
        let normalized = if matches!(self.mask_pattern, MaskPattern::Number { .. }) {
            normalize_number_input(new_text)
        } else {
            Cow::Borrowed(new_text)
        };

        if self.is_single_line() && normalized.contains(['\n', '\r']) {
            Cow::Owned(normalized.replace(['\n', '\r'], ""))
        } else {
            normalized
        }
    }

    pub(crate) fn is_valid_input(&self, new_text: &str, cx: &mut Context<Self>) -> bool {
        if new_text.is_empty() {
            return true;
        }

        if let Some(validate) = &self.validate {
            if !validate(new_text, cx) {
                return false;
            }
        }

        if !self.mask_pattern.is_valid(new_text) {
            return false;
        }

        let Some(pattern) = &self.pattern else {
            return true;
        };

        pattern.is_match(new_text)
    }

    /// Set the mask pattern for formatting the input text.
    ///
    /// The pattern can contain:
    /// - 9: Any digit or dot
    /// - A: Any letter
    /// - *: Any character
    /// - Other characters will be treated as literal mask characters
    ///
    /// Example: "(999)999-999" for phone numbers
    pub fn mask_pattern(mut self, pattern: impl Into<MaskPattern>) -> Self {
        self.mask_pattern = pattern.into();
        self.mask_pattern_set = true;
        if let Some(placeholder) = self.mask_pattern.placeholder() {
            self.placeholder = placeholder.into();
        }
        self
    }

    pub fn set_mask_pattern(
        &mut self,
        pattern: impl Into<MaskPattern>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mask_pattern = pattern.into();
        self.mask_pattern_set = true;
        if let Some(placeholder) = self.mask_pattern.placeholder() {
            self.placeholder = placeholder.into();
        }
        cx.notify();
    }

    /// Apply the default numeric mask unless the caller explicitly selected a mask.
    pub fn ensure_number_mask(&mut self) {
        if self.mask_pattern_set {
            return;
        }
        self.mask_pattern = MaskPattern::Number {
            separator: None,
            fraction: None,
        };
    }

    pub(super) fn set_input_bounds(&mut self, new_bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        let wrap_width_changed = self.input_bounds.size.width != new_bounds.size.width;
        self.input_bounds = new_bounds;

        // Update display_map wrap_width if changed.
        if let Some(last_layout) = self.last_layout.as_ref() {
            if wrap_width_changed {
                let wrap_width = if !self.soft_wrap {
                    // None to disable wrapping (will use Pixels::MAX)
                    None
                } else {
                    last_layout.wrap_width
                };

                self.display_map.on_layout_changed(wrap_width, cx);
                if self.is_multi_line() {
                    self.mode.update_auto_grow(&self.display_map);
                }
                cx.notify();
            }
        }
    }

    /// Return the selected portion of the text, borrowed out of the [`Rope`]
    /// the state owns.
    ///
    /// See [`Self::selected_value`] when an owned string is wanted.
    pub fn selected_text(&self) -> RopeSlice<'_> {
        let range_utf16 = self.range_to_utf16(&self.selected_range.into());
        let range = self.range_from_utf16(&range_utf16);
        self.text.slice(range)
    }

    /// Return the rendered bounds for a UTF-8 byte range in the current input contents.
    ///
    /// Returns `None` when the requested range is not currently laid out or visible.
    pub fn range_to_bounds(&self, range: &Range<usize>) -> Option<Bounds<Pixels>> {
        let Some(last_layout) = self.last_layout.as_ref() else {
            return None;
        };

        let Some(last_bounds) = self.last_bounds else {
            return None;
        };

        let (_, _, start_pos) = self.line_and_position_for_offset(range.start);
        let (_, _, end_pos) = self.line_and_position_for_offset(range.end);

        let Some(start_pos) = start_pos else {
            return None;
        };
        let Some(end_pos) = end_pos else {
            return None;
        };

        Some(Bounds::from_corners(
            last_bounds.origin + start_pos,
            last_bounds.origin + end_pos + point(px(0.), last_layout.line_height),
        ))
    }

    /// Replace text in range in silent.
    ///
    /// This will not trigger any UI interaction, such as auto-completion.
    ///
    /// An edit of the input's own, as every caller's is: a composition
    /// marked here is let go of first (`past_composition`).
    pub(crate) fn replace_text_in_range_silent(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range_utf16 = self.past_composition(range_utf16, window, cx);
        self.silent_replace_text = true;
        self.replace_text_in_range(range_utf16, new_text, window, cx);
        self.silent_replace_text = false;
    }

    /// An edit of the input's own of `range_utf16`, or of the selection, as
    /// typing makes one, past a composition marked here, which it lets go
    /// of first (`past_composition`). `replace_text_in_range` is the
    /// platform's, where no range is the marked one, and what it puts there
    /// ends the composition.
    fn edit_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range_utf16 = self.past_composition(range_utf16, window, cx);
        self.replace_text_in_range(range_utf16, new_text, window, cx);
    }

    /// Let go of a composition marked here before an edit of the input's
    /// own (`let_go_of_composition`), and give `range_utf16`, of the text
    /// with the composition in it, as it is in the text without it.
    ///
    /// Added in elundqvist/gpui-kit (elundqvist/kvist#482). A shortcut after
    /// a dead key runs with the ´ still marked, as GTK has it, and its edit
    /// went in with the composition's: one with a range of its own was
    /// merged into the step of undo the composition had open, which kept
    /// the ´'s old text and took the edit's new, so Ctrl+Delete's word was
    /// in no step and an undo did not bring it back; and one with none, as
    /// the selection's, took the marked range for the selection. GTK's text
    /// widgets reset their input method before such an edit, and the
    /// preedit goes.
    fn past_composition(
        &mut self,
        range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        if self.ime_marked_range.is_none() {
            return range_utf16;
        }
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16));
        let Some(marked) = self.let_go_of_composition(window, cx) else {
            return range_utf16;
        };
        range.map(|range| {
            let range = past_mark(&marked, range.start)..past_mark(&marked, range.end);
            self.range_to_utf16(&range)
        })
    }

    /// Let go of the composition marked here, an input method's or a dead
    /// key's, where the input edits on its own or loses the focus: what is
    /// marked goes, as GTK's preedit goes at its input method's reset, with
    /// the step of undo it had open and nothing in it, and the selection
    /// stays on its text. The range that was marked, for a range of the
    /// text with it in to be given without it (`past_mark`); none where
    /// nothing was marked, or where a single line's check refuses its text
    /// without it, which keeps it as text. A dead key's sequence ends with
    /// it: elundqvist/gpui-kit's Linux clients ask the input whether it
    /// still has the ´ marked at the key after a shortcut.
    ///
    /// Added in elundqvist/gpui-kit (elundqvist/kvist#482).
    fn let_go_of_composition(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let marked: Range<usize> = self.ime_marked_range?.into();
        let selection = self.selected_range;
        // the intent the edit after this asked for is its own
        let intent = self.undo_manager.pending_intent.take();
        self.replace_and_mark_text_in_range(None, "", None, window, cx);
        self.undo_manager.pending_intent = intent;
        if self.ime_marked_range.is_some() {
            self.unmark_text(window, cx);
            return None;
        }
        self.selected_range = Selection::new(
            past_mark(&marked, selection.start),
            past_mark(&marked, selection.end),
        );
        Some(marked)
    }

    /// What follows the text through an edit, once the text has it:
    /// `range` of `old_text`, the text before it, is `new_text` now, made
    /// as `steps`, the changes in turn, each its range in the text the
    /// ones before it left and how long what went there is. The problems
    /// move with each change, and the folds and the lines laid out with
    /// the edit, and the highlighter, the fold candidates and the language
    /// features take it in.
    fn follow_edit(
        &mut self,
        old_text: &Rope,
        range: &Range<usize>,
        new_text: &str,
        steps: &[(Range<usize>, usize)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.text.clone();
        if let Some(diagnostics) = self.mode.diagnostics_mut() {
            diagnostics.edit_steps(steps, &text)
        }
        // Adjust folds before updating wrap map: remove those a step's lines
        // reach and shift others
        self.display_map
            .adjust_folds_for_steps(old_text, &text, range, new_text, steps);
        self.display_map
            .on_text_edited(&self.text, range, new_text.len(), cx);

        self.mode.update_highlighter::<M>(
            super::mode::HighlighterUpdate {
                selected_range: range,
                old_text,
                new_text: &self.text,
                change_text: new_text,
                steps,
                force: true,
            },
            window,
            cx,
        );

        self.update_fold_candidates_incremental(range, new_text);
        M::refresh_language_features(self, window, cx);
    }

    /// Update fold candidates from tree-sitter syntax tree (full extraction).
    /// Used only on initial load or language changes.
    fn update_fold_candidates(&mut self) {
        if !self.mode.is_folding() {
            return;
        }

        let Some(highlighter_rc) = self.mode.highlighter() else {
            return;
        };

        let highlighter = highlighter_rc.borrow();
        let Some(highlighter) = highlighter.as_ref() else {
            return;
        };

        let fold_ranges = highlighter.fold_ranges(&self.text);
        self.display_map.set_fold_candidates(fold_ranges);
    }

    /// Incrementally update fold candidates after a text edit.
    /// Only traverses the edited region of the syntax tree instead of the full tree.
    fn update_fold_candidates_incremental(&mut self, edit_range: &Range<usize>, new_text: &str) {
        if !self.mode.is_folding() {
            return;
        }

        let Some(highlighter_rc) = self.mode.highlighter() else {
            return;
        };

        let highlighter = highlighter_rc.borrow();
        let Some(highlighter) = highlighter.as_ref() else {
            return;
        };

        // The new byte range in the updated text after the edit, on the
        // whole of the lines it begins and ends on: a fold the edit crosses
        // the edges of, or whose first or last line it breaks or joins, was
        // taken (`adjust_folds_for_steps`), one it lay within was kept and
        // takes its extent from what is found here on its first line, and
        // a candidate that begins on its last line or ends on its first is
        // found again only here. Looked for in the edit's bytes alone, a function
        // that began where a cut ended, or where a paste began, lost its
        // chevron until the file was read again.
        let new_end = (edit_range.start + new_text.len()).min(self.text.len());
        let first = self.text.offset_to_point(edit_range.start.min(new_end)).row;
        let last = self.text.offset_to_point(new_end).row;
        let lines = self.text.line_start_offset(first)..self.text.line_end_offset(last);
        self.display_map.update_fold_candidates_for_edit(
            |range, text| highlighter.fold_ranges_for_edit(range, text),
            lines,
            &self.text,
        );
    }
}

/// Whether putting `new` in place of `range` of `text` leaves it as it is:
/// read only where the two are as long, as a paste of the whole text over
/// itself is.
fn puts_back(text: &Rope, range: &Range<usize>, new: &str) -> bool {
    range.len() == new.len() && text.slice(range.clone()) == new
}

impl<M: InputModeKind> EntityInputHandler for InputBaseState<M> {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        adjusted_range.replace(self.range_to_utf16(&range));
        Some(self.text.slice(range).to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range.into()),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.ime_marked_range
            .map(|range| self.range_to_utf16(&range.into()))
    }

    /// End the composition, keeping what it marked as text: an edit, as a
    /// commit is, that says so with [`InputEvent::Change`]. Marking says
    /// nothing, so a composition a click or Escape ended was in the field
    /// and nowhere else, and an editor that saves as it is typed in,
    /// Kvist's, said "Saved" of a file without it until the next key
    /// (elundqvist/kvist#455).
    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let kept = self.ime_marked_range.take().is_some_and(|range| !range.is_empty());
        self.undo_manager.commit_transaction();
        if kept {
            self.update_search(cx);
            if self.emit_events {
                cx.emit(InputEvent::Change);
            }
        }
    }

    /// Replace text in range.
    ///
    /// - If the new text is invalid, it will not be replaced.
    /// - If `range_utf16` is not provided, the current selected range will be used.
    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let requested_intent = self.undo_manager.pending_intent.take();
        if !self.is_editable() {
            return;
        }
        let selection_before = self.selected_range;

        if self.blink_cursor.read(cx).visible() {
            self.pause_blink_cursor(cx);
        }

        // NOTE: The normalization keeps the UTF-16 length, but may change the
        // UTF-8 byte length, so all the byte-offset calculations below must
        // use the normalized text.
        let new_text = self.normalize_input(new_text);
        let new_text: &str = &new_text;

        // Text typed over a selection comes from the platform with no range
        // and no composition, and is typing: one step of undo with what is
        // typed after it, as VS Code and IntelliJ have it, where it had been
        // a step of its own. A paste, a newline or a delete asks for its own
        // intent, or is none of this.
        let requested_intent = requested_intent.or_else(|| {
            let typed_over_selection = range_utf16.is_none()
                && self.ime_marked_range.is_none()
                && !self.selected_range.is_empty()
                && !new_text.is_empty()
                && !new_text.contains(['\n', '\r']);
            typed_over_selection.then_some(EditIntent::Typing)
        });

        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.ime_marked_range.map(|range| {
                let range = self.range_to_utf16(&(range.start..range.end));
                self.range_from_utf16(&range)
            }))
            .unwrap_or(self.selected_range.into());

        let old_text = self.text.clone();
        self.text.replace(range.clone(), new_text);

        let mut new_offset = (range.start + new_text.len()).min(self.text.len());

        // True if the mask has changed the text, e.g. regrouping the
        // separators or completing a leading dot.
        let mut mask_changed = false;

        if self.is_single_line() {
            let pending_text = self.text.to_string();
            // Check if the new text is valid.
            //
            // Only reject the edit if the old text was valid, to avoid
            // trapping a pre-existing invalid text (e.g. a `default_value`
            // that does not conform), the user can still edit to fix it.
            if !self.is_valid_input(&pending_text, cx)
                && self.is_valid_input(&old_text.to_string(), cx)
            {
                self.text = old_text;
                return;
            }

            if !self.mask_pattern.is_none() {
                let mask_text = self.mask_pattern.mask(&pending_text);
                mask_changed = mask_text.as_str() != pending_text;
                self.text = Rope::from(mask_text.as_str());
                let new_text_len =
                    (new_text.len() + mask_text.len()).saturating_sub(pending_text.len());
                new_offset = (range.start + new_text_len).min(mask_text.len());
            }
        }

        // A replacement by what is there already, a paste of the whole text
        // over itself, changes nothing that follows the text: as an edit of
        // all of it, it opened every fold and took every problem's mark and
        // decoration (elundqvist/kvist#404). Nor is it a step of undo, which
        // records no change that leaves the text as it was.
        let puts_back = !mask_changed && puts_back(&old_text, &range, new_text);
        if mask_changed {
            // Masking rewrites the whole document, so ranges recorded against
            // the old text no longer point at anything.
            M::reset_annotations(self);
        } else if !puts_back {
            M::adjust_annotations(self, &[(range.clone(), new_text.len())]);
        }
        if mask_changed {
            // A segment-based history entry no longer matches the masked
            // document, record a whole-document change instead, so that
            // undo/redo can restore the text exactly.
            self.push_history(
                &old_text,
                &(0..old_text.len()),
                &self.text.to_string(),
                Some(EditIntent::Atomic),
                selection_before,
                Some(Selection::new(new_offset, new_offset)),
            );
        } else {
            self.push_history(
                &old_text,
                &range,
                &new_text,
                requested_intent,
                selection_before,
                None,
            );
        }
        // A commit ends the IME composition: macOS delivers `insertText:` for
        // the confirmed candidate without a following `unmarkText`, so close
        // the transaction here. Leaving it open would keep merging every later
        // edit into the same change, which then carries the text and selection
        // of the first composition.
        self.undo_manager.commit_transaction();
        if !puts_back {
            self.follow_edit(&old_text, &range, new_text, &[(range.clone(), new_text.len())], window, cx);
        }
        self.selected_range = (new_offset..new_offset).into();
        self.ime_marked_range.take();
        // The layout is older than the text now, so the caret's place in it
        // is not the column: a vertical move takes that from the layout
        // current then.
        self.preferred_column = None;
        self.update_search(cx);
        if self.is_multi_line() {
            self.mode.update_auto_grow(&self.display_map);
            if !self.silent_replace_text {
                self.reveal_typed_caret(cx);
            }
        }
        if !self.silent_replace_text {
            M::on_text_typed(self, &range, &new_text, window, cx);
        }
        if self.emit_events {
            cx.emit(InputEvent::Change);
        }
        cx.notify();
    }

    /// Mark text is the IME temporary insert on typing.
    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let requested_intent = self.undo_manager.pending_intent.take();
        if !self.is_editable() {
            return;
        }
        // an empty composition where nothing is marked ends none, and
        // leaves the text be: the X11 client deletes a composition it knows
        // of so as the window loses the keyboard, behind a paste that holds
        // the input, and the focus may have moved to a field with nothing
        // marked, whose selection it deleted (elundqvist/kvist#435)
        if new_text.is_empty() && range_utf16.is_none() && self.ime_marked_range.is_none() {
            return;
        }
        let selection_before = self.selected_range;

        let starts_composition = self.ime_marked_range.is_none();
        if starts_composition {
            self.undo_manager.begin_transaction();
        }

        M::reset_language_features(self);

        // See the same NOTE in `replace_text_in_range`.
        let new_text = self.normalize_input(new_text);
        let new_text: &str = &new_text;

        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.ime_marked_range.map(|range| {
                let range = self.range_to_utf16(&(range.start..range.end));
                self.range_from_utf16(&range)
            }))
            .unwrap_or(self.selected_range.into());

        let old_text = self.text.clone();
        self.text.replace(range.clone(), new_text);

        if self.is_single_line() {
            let pending_text = self.text.to_string();
            // See the same NOTE in `replace_text_in_range`.
            if !self.is_valid_input(&pending_text, cx)
                && self.is_valid_input(&old_text.to_string(), cx)
            {
                self.text = old_text;
                if starts_composition {
                    self.undo_manager.commit_transaction();
                }
                return;
            }
        }

        M::adjust_annotations(self, &[(range.clone(), new_text.len())]);
        self.follow_edit(&old_text, &range, new_text, &[(range.clone(), new_text.len())], window, cx);
        if new_text.is_empty() {
            // Cancel selection, when cancel IME input.
            self.selected_range = (range.start..range.start).into();
            self.ime_marked_range = None;
        } else {
            self.ime_marked_range = Some((range.start..range.start + new_text.len()).into());
            self.selected_range = new_selected_range_utf16
                .as_ref()
                .map(|range_utf16| {
                    let new_text = Rope::from(new_text);
                    range.start + new_text.offset_utf16_to_offset(range_utf16.start)
                        ..range.start + new_text.offset_utf16_to_offset(range_utf16.end)
                })
                .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len())
                .into();
        }
        if self.is_multi_line() {
            self.mode.update_auto_grow(&self.display_map);
            self.reveal_typed_caret(cx);
        }
        self.push_history(
            &old_text,
            &range,
            new_text,
            requested_intent,
            selection_before,
            Some(self.selected_range),
        );
        if new_text.is_empty() {
            self.undo_manager.commit_transaction();
        }
        cx.notify();
    }

    /// Used to position IME candidates.
    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let line_height = last_layout.line_height;
        let line_number_width = last_layout.line_number_width;
        let range = self.range_from_utf16(&range_utf16);

        let mut start_origin = None;
        let mut end_origin = None;
        let line_number_origin = point(line_number_width, px(0.));
        let mut y_offset = last_layout.visible_top;

        for (vi, line) in last_layout.lines.iter().enumerate() {
            if start_origin.is_some() && end_origin.is_some() {
                break;
            }

            let index_offset = last_layout.visible_line_byte_offsets[vi];

            if start_origin.is_none() {
                if let Some(p) = line.position_for_index(
                    range.start.saturating_sub(index_offset),
                    last_layout,
                    false,
                ) {
                    start_origin = Some(p + point(px(0.), y_offset));
                }
            }

            if end_origin.is_none() {
                if let Some(p) = line.position_for_index(
                    range.end.saturating_sub(index_offset),
                    last_layout,
                    false,
                ) {
                    end_origin = Some(p + point(px(0.), y_offset));
                }
            }

            y_offset += line.size(line_height).height;
        }

        let start_origin = start_origin.unwrap_or_default();
        let mut end_origin = end_origin.unwrap_or_default();
        // Ensure at same line.
        end_origin.y = start_origin.y;

        Some(Bounds::from_corners(
            bounds.origin + line_number_origin + start_origin,
            // + line_height for show IME panel under the cursor line.
            bounds.origin + line_number_origin + point(end_origin.x, end_origin.y + line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let last_layout = self.last_layout.as_ref()?;
        let line_point = self.last_bounds?.localize(&point)?;

        for (vi, line) in last_layout.lines.iter().enumerate() {
            let offset = last_layout.visible_line_byte_offsets[vi];
            if let Some(utf8_index) = line.index_for_position(line_point, last_layout) {
                return Some(self.offset_to_utf16(offset + utf8_index));
            }
        }

        None
    }
}

impl<M: InputModeKind> Focusable for InputBaseState<M> {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl<M: InputModeKind> Render for InputBaseState<M> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Before anything reads it: the element resolves this style during
        // layout and paint, and both happen after this call in the same frame.
        self.editor_style = self
            .projected_editor_style
            .resolved(&crate::Theme::global(cx).tokens);
        let entity = cx.entity();
        if self._pending_update {
            self.mode.update_highlighter::<M>(
                super::mode::HighlighterUpdate {
                    selected_range: &(0..0),
                    old_text: &self.text,
                    new_text: &self.text,
                    change_text: "",
                    steps: &[],
                    force: false,
                },
                window,
                cx,
            );

            self.update_fold_candidates();
            M::refresh_language_features(self, window, cx);
            self._pending_update = false;
        }

        let element = div()
            .id("input-state")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .when(self.is_editable(), |this| {
                this.on_action(window.listener_for(&entity, InputBaseState::backspace))
                    .on_action(window.listener_for(&entity, InputBaseState::delete))
                    .on_action(
                        window.listener_for(&entity, InputBaseState::delete_to_beginning_of_line),
                    )
                    .on_action(window.listener_for(&entity, InputBaseState::delete_to_end_of_line))
                    .on_action(window.listener_for(&entity, InputBaseState::delete_previous_word))
                    .on_action(window.listener_for(&entity, InputBaseState::delete_next_word))
                    .on_action(window.listener_for(&entity, InputBaseState::enter))
                    .on_action(window.listener_for(&entity, InputBaseState::escape))
                    .on_action(window.listener_for(&entity, InputBaseState::paste))
                    .on_action(window.listener_for(&entity, InputBaseState::cut))
                    .on_action(window.listener_for(&entity, InputBaseState::undo))
                    .on_action(window.listener_for(&entity, InputBaseState::redo))
                    .when(self.is_multi_line(), |this| {
                        this.on_action(window.listener_for(&entity, InputBaseState::indent_inline))
                            .on_action(window.listener_for(&entity, InputBaseState::outdent_inline))
                            .on_action(window.listener_for(&entity, InputBaseState::indent_block))
                            .on_action(window.listener_for(&entity, InputBaseState::outdent_block))
                    })
            })
            .on_action(window.listener_for(&entity, InputBaseState::left))
            .on_action(window.listener_for(&entity, InputBaseState::right))
            .on_action(window.listener_for(&entity, InputBaseState::select_left))
            .on_action(window.listener_for(&entity, InputBaseState::select_right))
            .when(self.is_multi_line(), |this| {
                this.on_action(window.listener_for(&entity, InputBaseState::up))
                    .on_action(window.listener_for(&entity, InputBaseState::down))
                    .on_action(window.listener_for(&entity, InputBaseState::select_up))
                    .on_action(window.listener_for(&entity, InputBaseState::select_down))
                    .on_action(window.listener_for(&entity, InputBaseState::page_up))
                    .on_action(window.listener_for(&entity, InputBaseState::page_down))
            })
            .on_action(window.listener_for(&entity, InputBaseState::on_action_select_all))
            .on_action(window.listener_for(&entity, InputBaseState::select_to_start_of_line))
            .on_action(window.listener_for(&entity, InputBaseState::select_to_end_of_line))
            .on_action(window.listener_for(&entity, InputBaseState::select_to_previous_word))
            .on_action(window.listener_for(&entity, InputBaseState::select_to_next_word))
            .on_action(window.listener_for(&entity, InputBaseState::home))
            .on_action(window.listener_for(&entity, InputBaseState::end))
            .on_action(window.listener_for(&entity, InputBaseState::move_to_start))
            .on_action(window.listener_for(&entity, InputBaseState::move_to_end))
            .on_action(window.listener_for(&entity, InputBaseState::move_to_previous_word))
            .on_action(window.listener_for(&entity, InputBaseState::move_to_next_word))
            .on_action(window.listener_for(&entity, InputBaseState::select_to_start))
            .on_action(window.listener_for(&entity, InputBaseState::select_to_end))
            .on_action(window.listener_for(&entity, InputBaseState::show_character_palette))
            .on_action(window.listener_for(&entity, InputBaseState::copy))
            .on_action(window.listener_for(&entity, InputBaseState::on_action_search))
            .on_action(window.listener_for(&entity, InputBaseState::on_action_replace))
            .on_key_down(window.listener_for(&entity, InputBaseState::on_key_down))
            .on_mouse_down(
                MouseButton::Left,
                window.listener_for(&entity, InputBaseState::on_mouse_down),
            )
            .on_mouse_down(
                MouseButton::Right,
                window.listener_for(&entity, InputBaseState::on_mouse_down),
            )
            .on_mouse_up(
                MouseButton::Left,
                window.listener_for(&entity, InputBaseState::on_mouse_up),
            )
            .on_mouse_up(
                MouseButton::Right,
                window.listener_for(&entity, InputBaseState::on_mouse_up),
            )
            .on_mouse_move(window.listener_for(&entity, InputBaseState::on_mouse_move))
            .on_scroll_wheel(window.listener_for(&entity, InputBaseState::on_scroll_wheel))
            .when(!self.disabled, |this| this.cursor_text())
            .flex_1()
            .when(self.is_multi_line(), |this| this.h_full())
            .flex_grow_1()
            .overflow_x_hidden()
            .when(self.is_multi_line(), |this| {
                this.pt(self.editor_paddings.top)
                    .pr(self.editor_paddings.right)
                    .pb(self.editor_paddings.bottom)
                    .pl(self.editor_paddings.left)
            })
            .child(TextElement::new(entity.clone()).placeholder(self.placeholder.clone()))
            .when(self.shows_scrollbar(), |this| {
                this.child(EditorScrollbar::new(entity.clone()))
            });

        // Actions only one mode handles are registered by that mode, where
        // `Self` is concrete enough to name its own entity type.
        M::register_actions(element, &entity, window)
    }
}

/// Where `offset`, of a text with `marked` in it, is once what is marked is
/// gone: a place in it at its start (elundqvist/kvist#482).
fn past_mark(marked: &Range<usize>, offset: usize) -> usize {
    if offset <= marked.start {
        offset
    } else if offset >= marked.end {
        offset - marked.len()
    } else {
        marked.start
    }
}

/// Whether `c` makes the grapheme it is in an emoji Backspace takes whole:
/// the emoji's own blocks with the regional indicators of a flag and the
/// skin tones, the variation selector that makes a symbol an emoji, the
/// keycap's combining mark and the tags of a subdivision's flag. Kvist's
/// `code::emoji_sequence` reads them so.
fn is_emoji_part(c: char) -> bool {
    matches!(c as u32, 0x1F000..=0x1FAFF | 0xFE0F | 0x20E3 | 0xE0020..=0xE007F)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::theme::Theme;
    use gpui::{TestAppContext, VisualTestContext};

    use crate::input::{EditorMode, InputMode, TextareaMode};

    struct TestRoot<M: InputModeKind>(Entity<InputBaseState<M>>);

    impl<M: InputModeKind> Render for TestRoot<M> {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.0.clone())
        }
    }

    struct InputView<M: InputModeKind> {
        input: Entity<InputBaseState<M>>,
        window_handle: gpui::WindowHandle<TestRoot<M>>,
    }

    /// Helper to open a state of one mode in a window for testing.
    impl<M: InputModeKind> InputView<M> {
        fn build_with(
            cx: &mut TestAppContext,
            make: impl FnOnce(&mut Window, &mut Context<InputBaseState<M>>) -> InputBaseState<M>
            + 'static,
        ) -> Self {
            let mut input: Option<Entity<InputBaseState<M>>> = None;

            let window = cx.update(|cx| {
                cx.open_window(Default::default(), |window, cx| {
                    // Set up the theme first
                    cx.set_global(Theme::default());
                    // Initialize input keybindings
                    super::super::init(cx);

                    input = Some(cx.new(|cx| make(window, cx)));

                    cx.new(|_| TestRoot(input.clone().unwrap()))
                })
                .unwrap()
            });

            Self {
                input: input.clone().unwrap(),
                window_handle: window,
            }
        }
    }

    impl InputView<EditorMode> {
        /// An editor state, for the tests that exercise code-editor behavior.
        fn new(cx: &mut TestAppContext) -> Self {
            Self::build_editor(cx, |state| state)
        }

        fn build_editor(
            cx: &mut TestAppContext,
            f: impl FnOnce(InputBaseState<EditorMode>) -> InputBaseState<EditorMode> + 'static,
        ) -> Self {
            Self::build_with(cx, move |window, cx| {
                f(crate::input::EditorState::new(window, cx).language("sql"))
            })
        }
    }

    impl InputView<TextareaMode> {
        fn build_textarea(
            cx: &mut TestAppContext,
            f: impl FnOnce(InputBaseState<TextareaMode>) -> InputBaseState<TextareaMode> + 'static,
        ) -> Self {
            Self::build_with(cx, move |window, cx| {
                f(crate::input::TextareaState::new(window, cx))
            })
        }
    }

    impl InputView<InputMode> {
        /// A single-line state, the default these tests were written against.
        fn build(
            cx: &mut TestAppContext,
            f: impl FnOnce(InputBaseState<InputMode>) -> InputBaseState<InputMode> + 'static,
        ) -> Self {
            Self::build_with(cx, move |window, cx| {
                f(crate::input::InputState::new(window, cx))
            })
        }
    }

    #[gpui::test]
    fn only_a_multi_line_input_paints_scrollbars(cx: &mut TestAppContext) {
        cx.update(crate::init);

        // A single-line input keeps its caret in view by moving its own offset;
        // it has no viewport to drag, so a scrollbar in a text field is a
        // control that does not exist.
        let single = InputView::build(cx, |state| state);
        single
            .input
            .update(cx, |state, _| assert!(!state.shows_scrollbar()));

        let multi = InputView::build_textarea(cx, |state| state);
        multi
            .input
            .update(cx, |state, _| assert!(state.shows_scrollbar()));
    }

    #[gpui::test]
    fn context_menu_handler_is_deferred_and_respects_disabled(cx: &mut TestAppContext) {
        use std::{cell::Cell, rc::Rc};
        cx.update(crate::init);
        let input_view = InputView::new(cx);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;
        let calls = Rc::new(Cell::new(0usize));
        let items = Rc::new(Cell::new(0usize));

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                let calls2 = calls.clone();
                let items2 = items.clone();
                state.on_context_menu(Rc::new(move |menu, _, _, _, _| {
                    calls2.set(calls2.get() + 1);
                    items2.set(menu.items.len());
                }));
                state.handle_right_click_menu(point(px(0.), px(0.)), 0, window, cx);
            })
        });
        assert_eq!(calls.get(), 1);
        assert_eq!(items.get(), 0);

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.disabled = true;
                state.handle_right_click_menu(point(px(0.), px(0.)), 0, window, cx);
            })
        });
        assert_eq!(calls.get(), 1);
    }

    #[gpui::test]
    fn test_readonly_rejects_user_edits_only(cx: &mut TestAppContext) {
        let input_view = InputView::new(cx);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("hello", window, cx);
                state.set_readonly(true, cx);
            });
        });

        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert!(!state.is_editable());
                assert!(!state.is_replaceable());
            });
        });

        // Typing (and IME) goes through the input handler, it must be rejected.
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, " world", window, cx);
                state.replace_and_mark_text_in_range(None, "あ", None, window, cx);
            });
        });
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| assert_eq!(state.value(), "hello"));
        });

        // The programmatic APIs are not limited by the readonly mode.
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.insert(" world", window, cx);
                state.set_value("changed", window, cx);
            });
        });
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| assert_eq!(state.value(), "changed"));
        });

        // And the user can edit again after leaving the readonly mode.
        // The caret is at the start, because `set_value` has reset the selection.
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_readonly(false, cx);
                state.replace_text_in_range(None, "!", window, cx);
            });
        });
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert!(state.is_editable());
                assert_eq!(state.value(), "!changed");
            });
        });
    }

    /// Regression test: `scroll_to` at end-of-buffer must produce a deferred
    /// scroll target within the safe scroll range, so the painted frame
    /// matches what `update_scroll_offset` persists (no jitter). A small
    /// `cursor_surrounding_lines` override used to mismatch the hardcoded
    /// 3-line edge clearance in `scroll_to`, overshooting `safe_y_min`.
    #[gpui::test]
    fn test_scroll_to_eob_does_not_overshoot_safe_range(cx: &mut TestAppContext) {
        let input_view = InputView::new(cx);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        // JetBrains-style: 1 trailing empty row + 1-line cursor surrounding.
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_scroll_beyond_last_line(Some(1), window, cx);
                state.set_cursor_surrounding_lines(Some(1), window, cx);
                let text: String = (1..=50)
                    .map(|i| format!("line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                state.set_value(text, window, cx);
            });
        });
        cx.run_until_parked();

        // Sanity: paint populated `scroll_size` and `input_bounds` — without
        // these, `safe_y_min` below collapses to 0 and the assertion is vacuous.
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert!(
                    state.scroll_size.height > px(0.),
                    "scroll_size not populated by initial paint"
                );
                assert!(
                    state.input_bounds.size.height > px(0.),
                    "input_bounds not populated by initial paint"
                );
            });
        });

        // Move cursor to end with downward direction — same code path as a
        // `Down` keystroke at EOB. `scroll_to` runs synchronously inside
        // `move_to`; inspect `deferred_scroll_offset` in the same closure
        // before the next paint consumes and clears it.
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                let end = state.text.len();
                state.move_to(end, Some(MoveDirection::Down), cx);

                let deferred = state
                    .deferred_scroll_offset
                    .expect("scroll_to should populate deferred_scroll_offset");
                let safe_y_min =
                    (-state.scroll_size.height + state.input_bounds.size.height).min(px(0.));

                assert!(
                    deferred.y >= safe_y_min,
                    "deferred_scroll_offset.y = {:?} below safe_y_min = {:?} \
                     — paint would jitter (Bug C regression)",
                    deferred.y,
                    safe_y_min,
                );
            });
        });
    }

    /// `line {i}` for each `i` in `lines`, a line apiece.
    fn numbered_lines(lines: std::ops::Range<usize>) -> String {
        lines.map(|i| format!("line {i}\n")).collect()
    }

    /// The caret's line was laid out in the last frame and the caret was
    /// painted inside the view.
    fn assert_caret_in_view<M: InputModeKind>(state: &InputBaseState<M>, what: &str) {
        let caret = state.cursor();
        let row = state.text.offset_to_point(caret).row;
        let rows = state.visible_row_range().expect("laid out");
        assert!(
            rows.contains(&row),
            "{what}: the caret's line {row} is not among the lines laid out, {rows:?}"
        );
        let at = state
            .range_to_bounds(&(caret..caret))
            .expect("the caret has a place");
        let view = state.input_bounds;
        assert!(
            at.top() >= view.top() && at.bottom() <= view.bottom(),
            "{what}: the caret at {at:?} is outside the view {view:?}"
        );
    }

    /// A paste past the end of the text ends with its last line in view. It
    /// scrolled before the pasted lines were laid out, against the height
    /// the text had before them, and the view stopped there with the caret
    /// thousands of lines below it (elundqvist/kvist#201).
    #[gpui::test]
    fn test_a_paste_past_the_end_scrolls_to_its_last_line(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value(numbered_lines(0..100), window, cx)
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                let end = state.text.len();
                state.move_to(end, None, cx);
            });
        });
        cx.run_until_parked();

        cx.update(|window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(numbered_lines(100..4100)));
            input.update(cx, |state, cx| state.paste(&Paste, window, cx));
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(state.text.offset_to_point(state.cursor()).row, 4100);
                assert_caret_in_view(state, "after the paste");
            });
        });
    }

    /// A hold that writes down when it holds the input and when it gives it
    /// back.
    fn recording_hold(log: &Rc<std::cell::RefCell<Vec<&'static str>>>) -> PasteInputHold {
        /// What the hold gives back for its release.
        struct Held(&'static str);
        let held = log.clone();
        let released = log.clone();
        let landed = log.clone();
        PasteInputHold::new(
            move |_, _| {
                held.borrow_mut().push("hold");
                Held("release")
            },
            move |Held(release), _, _| released.borrow_mut().push(release),
        )
        .on_landing(move |_, _| landed.borrow_mut().push("landing"))
    }

    /// A paste whose clipboard is not at hand, another program's on X11,
    /// lands once the read has come, the window's input held until then,
    /// so the frame is not held while the owner hands it over and what is
    /// typed meanwhile still comes after the paste (elundqvist/kvist#401).
    #[gpui::test]
    fn test_a_paste_that_waits_for_the_clipboard_lands_when_it_comes(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        let log = Rc::new(std::cell::RefCell::new(Vec::new()));
        let (send, received) = futures::channel::oneshot::channel::<Option<ClipboardItem>>();
        let read = cx
            .executor()
            .spawn(async move { Ok(received.await.ok().flatten()) });
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("one two", window, cx);
                state.move_to(4, None, cx);
                paste_when_read(
                    state,
                    read,
                    recording_hold(&log),
                    window,
                    cx,
                    InputBaseState::paste_item,
                );
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(input.read(cx).value(), "one two"));
        assert_eq!(*log.borrow(), ["hold"], "the input waits for the paste");

        send.send(Some(ClipboardItem::new_string("and ".into())))
            .unwrap();
        cx.run_until_parked();
        cx.update(|_, cx| {
            let state = input.read(cx);
            assert_eq!(state.value(), "one and two");
            assert_eq!(state.cursor(), 8);
        });
        assert_eq!(*log.borrow(), ["hold", "landing", "release"]);
    }

    /// The app's own copy is at hand: it is pasted at once, and the input
    /// is never held. A read that fails pastes nothing and gives the input
    /// back.
    #[gpui::test]
    fn test_a_paste_of_a_clipboard_at_hand_holds_nothing(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        let log = Rc::new(std::cell::RefCell::new(Vec::new()));
        cx.update(|window, cx| {
            recording_hold(&log).set(cx);
            cx.write_to_clipboard(ClipboardItem::new_string("one ".into()));
            input.update(cx, |state, cx| state.paste(&Paste, window, cx));
            assert_eq!(input.read(cx).value(), "one ");
        });
        assert!(log.borrow().is_empty());

        let (send, received) = futures::channel::oneshot::channel::<Option<ClipboardItem>>();
        let read = cx.executor().spawn(async move {
            received
                .await
                .map_err(|_| gpui::ClipboardReadError::Unavailable)
        });
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                paste_when_read(
                    state,
                    read,
                    recording_hold(&log),
                    window,
                    cx,
                    InputBaseState::paste_item,
                )
            });
        });
        drop(send);
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(input.read(cx).value(), "one "));
        assert_eq!(*log.borrow(), ["hold", "landing", "release"]);
    }

    /// What a paste that waits looks at the keys pressed meanwhile with,
    /// kept by [`keyed_hold`] to press them.
    type Keys = Rc<std::cell::RefCell<Option<PasteKeyDown>>>;

    /// A hold that writes down when it holds the input and when it gives it
    /// back, as `recording_hold` does, and keeps what the paste looks at
    /// the keys with in `keys`.
    fn keyed_hold(log: &Rc<std::cell::RefCell<Vec<&'static str>>>, keys: &Keys) -> PasteInputHold {
        struct Held;
        let (held, released, landed, keys) = (log.clone(), log.clone(), log.clone(), keys.clone());
        PasteInputHold::with_keys(
            move |_, _, key_down| {
                held.borrow_mut().push("hold");
                *keys.borrow_mut() = Some(key_down);
                Held
            },
            move |Held, _, _| released.borrow_mut().push("release"),
        )
        .on_landing(move |_, _| landed.borrow_mut().push("landing"))
    }

    /// Press `keystroke` while the paste waits: whether it took the key.
    fn press(cx: &mut VisualTestContext, keys: &Keys, keystroke: &str) -> bool {
        let key_down = KeyDownEvent {
            keystroke: gpui::Keystroke::parse(keystroke).unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        cx.update(|window, cx| {
            let mut keys = keys.borrow_mut();
            let key_down_with = keys.as_mut().expect("the paste waits");
            key_down_with(&key_down, window, cx)
        })
    }

    /// A paste whose clipboard has not come in `PASTE_WAIT_SHOWN_AFTER` is
    /// shown as waiting, and an Escape then gives it up: the input is let
    /// go at once and the text stays as it was when the read comes after
    /// all. Before then an Escape, and any other key, waits with the rest
    /// (elundqvist/kvist#435).
    #[gpui::test]
    fn test_escape_gives_up_a_paste_that_has_waited(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        let log = Rc::new(std::cell::RefCell::new(Vec::new()));
        let keys = Keys::default();
        let (send, received) = futures::channel::oneshot::channel::<Option<ClipboardItem>>();
        let read = cx
            .executor()
            .spawn(async move { Ok(received.await.ok().flatten()) });
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("one two", window, cx);
                state.move_to(4, None, cx);
                paste_when_read(
                    state,
                    read,
                    keyed_hold(&log, &keys),
                    window,
                    cx,
                    InputBaseState::paste_item,
                );
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| assert!(!paste_waits(window, cx), "not yet shown"));
        assert!(
            !press(&mut cx, &keys, "escape"),
            "an Escape before then waits"
        );

        cx.executor().advance_clock(PASTE_WAIT_SHOWN_AFTER);
        cx.run_until_parked();
        cx.update(|window, cx| assert!(paste_waits(window, cx), "shown as waiting"));
        assert!(!press(&mut cx, &keys, "a"), "another key waits");
        assert!(!press(&mut cx, &keys, "shift-escape"));
        assert_eq!(*log.borrow(), ["hold"]);

        assert!(press(&mut cx, &keys, "escape"), "Escape gives it up");
        assert_eq!(
            *log.borrow(),
            ["hold", "release"],
            "the input is let go at once"
        );
        cx.update(|window, cx| assert!(!paste_waits(window, cx)));
        assert!(!press(&mut cx, &keys, "escape"), "given up once");

        send.send(Some(ClipboardItem::new_string("late ".into())))
            .unwrap();
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(input.read(cx).value(), "one two"));
        assert_eq!(*log.borrow(), ["hold", "release"], "nothing lands");
    }

    /// A paste shown as waiting that then lands is no longer shown, and
    /// lets the input go once.
    #[gpui::test]
    fn test_a_paste_shown_as_waiting_lands_and_is_shown_no_more(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        let log = Rc::new(std::cell::RefCell::new(Vec::new()));
        let keys = Keys::default();
        let (send, received) = futures::channel::oneshot::channel::<Option<ClipboardItem>>();
        let read = cx
            .executor()
            .spawn(async move { Ok(received.await.ok().flatten()) });
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                paste_when_read(
                    state,
                    read,
                    keyed_hold(&log, &keys),
                    window,
                    cx,
                    InputBaseState::paste_item,
                );
            });
        });
        cx.executor().advance_clock(PASTE_WAIT_SHOWN_AFTER);
        cx.run_until_parked();
        cx.update(|window, cx| assert!(paste_waits(window, cx)));

        send.send(Some(ClipboardItem::new_string("came".into())))
            .unwrap();
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(!paste_waits(window, cx));
            assert_eq!(input.read(cx).value(), "came");
        });
        assert_eq!(*log.borrow(), ["hold", "landing", "release"]);
        assert!(
            !press(&mut cx, &keys, "escape"),
            "a paste that landed takes no key"
        );
    }

    /// A paste whose clipboard could not be read, its owner not answering,
    /// pastes nothing and lets the input go, and its window says so for
    /// `PASTE_UNANSWERED_SHOWN_FOR`, or until a paste waits again; one
    /// given up with Escape says nothing (elundqvist/kvist#435).
    #[gpui::test]
    fn test_a_paste_whose_clipboard_could_not_be_read_says_so(cx: &mut TestAppContext) {
        type Read = Result<Option<ClipboardItem>, gpui::ClipboardReadError>;
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        let log = Rc::new(std::cell::RefCell::new(Vec::new()));
        let keys = Keys::default();
        let paste = |cx: &mut VisualTestContext,
                     log: &Rc<std::cell::RefCell<Vec<&'static str>>>| {
            let (send, received) = futures::channel::oneshot::channel::<Read>();
            let read = cx
                .executor()
                .spawn(async move { received.await.unwrap_or(Ok(None)) });
            cx.update(|window, cx| {
                input.update(cx, |state, cx| {
                    paste_when_read(
                        state,
                        read,
                        keyed_hold(log, &keys),
                        window,
                        cx,
                        InputBaseState::paste_item,
                    )
                })
            });
            send
        };
        let unanswered =
            |cx: &mut VisualTestContext| cx.update(|window, cx| paste_unanswered(window, cx));
        cx.update(|window, cx| input.update(cx, |state, cx| state.set_value("one", window, cx)));

        let send = paste(&mut cx, &log);
        cx.executor().advance_clock(PASTE_WAIT_SHOWN_AFTER);
        cx.run_until_parked();
        assert!(!unanswered(&mut cx));
        send.send(Err(gpui::ClipboardReadError::Unavailable))
            .unwrap();
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(!paste_waits(window, cx));
            assert_eq!(input.read(cx).value(), "one");
        });
        assert!(unanswered(&mut cx), "the window says it was not read");
        assert_eq!(*log.borrow(), ["hold", "landing", "release"]);
        cx.executor().advance_clock(PASTE_UNANSWERED_SHOWN_FOR);
        cx.run_until_parked();
        assert!(!unanswered(&mut cx), "for a while");

        let send = paste(&mut cx, &log);
        send.send(Err(gpui::ClipboardReadError::Unavailable))
            .unwrap();
        cx.run_until_parked();
        assert!(unanswered(&mut cx));
        let send = paste(&mut cx, &log);
        cx.run_until_parked();
        assert!(!unanswered(&mut cx), "a paste that waits again ends it");
        cx.executor().advance_clock(PASTE_WAIT_SHOWN_AFTER);
        cx.run_until_parked();
        assert!(press(&mut cx, &keys, "escape"), "given up");
        send.send(Err(gpui::ClipboardReadError::Unavailable))
            .unwrap();
        cx.run_until_parked();
        assert!(!unanswered(&mut cx), "one given up says nothing");
    }

    /// A view that pastes into something of its own, not an input, as a
    /// terminal pastes into its shell, reads the clipboard as an input
    /// does: what is at hand at once, nothing held, and what waits once it
    /// has come, the window's input held until then; a view gone by then
    /// is handed nothing, and the input is let go (elundqvist/kvist#433).
    #[gpui::test]
    fn test_a_view_that_is_no_input_pastes_as_an_input_does(cx: &mut TestAppContext) {
        struct Pasted(Vec<String>);
        impl Render for Pasted {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }
        fn take(
            view: &mut Pasted,
            item: Option<ClipboardItem>,
            _: &mut Window,
            _: &mut Context<Pasted>,
        ) {
            view.0.extend(item.and_then(|item| item.text()));
        }
        let waiting = |cx: &mut VisualTestContext| {
            let (send, received) = futures::channel::oneshot::channel::<Option<ClipboardItem>>();
            let read = cx
                .executor()
                .spawn(async move { Ok(received.await.ok().flatten()) });
            (send, read)
        };
        let (view, cx) = cx.add_window_view(|_, _| Pasted(Vec::new()));
        let log = Rc::new(std::cell::RefCell::new(Vec::new()));
        cx.update(|window, cx| {
            recording_hold(&log).set(cx);
            cx.write_to_clipboard(ClipboardItem::new_string("at hand".into()));
            view.update(cx, |view, cx| {
                read_clipboard_for_paste(view, window, cx, take)
            });
            assert_eq!(view.read(cx).0, ["at hand"]);
        });
        assert!(log.borrow().is_empty(), "nothing waits: {:?}", log.borrow());

        let (send, read) = waiting(cx);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                paste_when_read(view, read, recording_hold(&log), window, cx, take)
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(view.read(cx).0, ["at hand"]));
        assert_eq!(*log.borrow(), ["hold"], "the input waits for the paste");
        send.send(Some(ClipboardItem::new_string("another program's".into())))
            .unwrap();
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(view.read(cx).0, ["at hand", "another program's"]));
        assert_eq!(*log.borrow(), ["hold", "landing", "release"]);

        log.borrow_mut().clear();
        let (send, read) = waiting(cx);
        let gone = cx.update(|window, cx| {
            let gone = cx.new(|_| Pasted(Vec::new()));
            gone.update(cx, |view, cx| {
                paste_when_read(view, read, recording_hold(&log), window, cx, take)
            });
            gone.downgrade()
        });
        cx.run_until_parked();
        assert!(gone.upgrade().is_none(), "the view has gone");
        send.send(Some(ClipboardItem::new_string("too late".into())))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(*log.borrow(), ["hold", "landing", "release"]);
    }

    /// A clipboard that holds an image and no text leaves the text and its
    /// selection as they are, as VS Code and IntelliJ do: it replaced the
    /// selection with nothing.
    #[gpui::test]
    fn test_a_paste_of_an_image_alone_leaves_the_text(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        cx.update(|window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_image(&gpui::Image::from_bytes(
                gpui::ImageFormat::Png,
                vec![0x89, b'P', b'N', b'G'],
            )));
            input.update(cx, |state, cx| {
                state.set_value("hello world", window, cx);
                state.selected_range = (0..5).into();
                state.paste(&Paste, window, cx);
            });
            let state = input.read(cx);
            assert_eq!(state.value(), "hello world");
            assert_eq!(state.selected_range, (0..5).into());
        });
    }

    /// An undo that takes the caret thousands of lines back, and the redo
    /// after it, draw the lines about the caret in the frame that follows.
    /// The undo moved the caret and left the view to the layout's follow,
    /// which chose the lines at the old place and then jumped to the caret:
    /// the frame was blank until another came, a third of a second later
    /// with the caret's blink (elundqvist/kvist#201).
    #[gpui::test]
    fn test_an_undone_paste_is_drawn_about_the_caret(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value(numbered_lines(0..200), window, cx)
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                let second = state.text.line_start_offset(1);
                state.move_to(second, None, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(numbered_lines(1000..5000)));
            input.update(cx, |state, cx| state.paste(&Paste, window, cx));
        });
        cx.run_until_parked();
        cx.update(|_, cx| input.read_with(cx, |state, _| assert_caret_in_view(state, "pasted")));

        cx.update(|window, cx| input.update(cx, |state, cx| state.undo(&Undo, window, cx)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(state.text.lines_len(), 201);
                assert_caret_in_view(state, "undone");
            });
        });

        cx.update(|window, cx| input.update(cx, |state, cx| state.redo(&Redo, window, cx)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(state.text.offset_to_point(state.cursor()).row, 4001);
                assert_caret_in_view(state, "redone");
            });
        });
    }

    /// The text at `at` was laid out in the last frame and painted inside
    /// the view.
    fn assert_offset_in_view<M: InputModeKind>(state: &InputBaseState<M>, at: usize, what: &str) {
        let row = state.text.offset_to_point(at).row;
        let rows = state.visible_row_range().expect("laid out");
        assert!(
            rows.contains(&row),
            "{what}: line {row} is not among the lines laid out, {rows:?}"
        );
        let bounds = state.range_to_bounds(&(at..at)).expect("it has a place");
        let view = state.input_bounds;
        assert!(
            bounds.top() >= view.top() && bounds.bottom() <= view.bottom(),
            "{what}: line {row} at {bounds:?} is outside the view {view:?}"
        );
    }

    /// 400 lines, "needle N" on lines 20, 250 and 330 and "line N" on the
    /// rest, in an editor with the caret at the start of `caret_line` and
    /// the view on it.
    fn needles(
        cx: &mut TestAppContext,
        caret_line: usize,
    ) -> (Entity<InputBaseState<EditorMode>>, VisualTestContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        let text: String = (0..400)
            .map(|i| {
                if [20, 250, 330].contains(&i) {
                    format!("needle {i}\n")
                } else {
                    format!("line {i}\n")
                }
            })
            .collect();
        cx.update(|window, cx| input.update(cx, |state, cx| state.set_value(text, window, cx)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                let at = state.text.line_start_offset(caret_line);
                state.move_to(at, None, cx);
            })
        });
        cx.run_until_parked();
        (input, cx)
    }

    /// The find bar's current match, counted from none, and that its line
    /// is in view.
    fn assert_current_match(
        input: &Entity<InputBaseState<EditorMode>>,
        cx: &mut VisualTestContext,
        index: usize,
        line: usize,
        what: &str,
    ) {
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                let matcher = &state.search_session().matcher;
                assert_eq!(matcher.current_match_index(), index, "{what}");
                let range = matcher.matched_ranges()[index].clone();
                assert_eq!(state.text.offset_to_point(range.start).row, line, "{what}");
                assert_offset_in_view(state, range.start, what);
            })
        });
    }

    /// A query typed into the find bar goes to its first match from where
    /// the search began, the caret, which is the current one, and Enter on
    /// to the one after it, as VS Code and IntelliJ do; one with no match
    /// after the caret goes round to the first in the text. The matches
    /// were counted from the first in the text, out of sight above, and
    /// nothing moved until Enter, which went to the second
    /// (elundqvist/kvist#208).
    #[gpui::test]
    fn test_a_query_typed_goes_to_its_first_match_from_the_caret(cx: &mut TestAppContext) {
        // the caret, and the view with it, well past the first needle and
        // short of the second
        let (input, mut cx) = needles(cx, 150);
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                state.open_search(false, cx);
                // typed a character at a time, as the bar hands them over
                for typed in ["n", "ne", "needle"] {
                    state.set_search_query(typed, true, cx);
                }
                assert_eq!(state.search_session().matcher.len(), 3);
            })
        });
        assert_current_match(&input, &mut cx, 1, 250, "typed");
        cx.update(|_, cx| input.update(cx, |state, cx| state.next_search_match(cx)));
        assert_current_match(&input, &mut cx, 2, 330, "Enter goes on to the next");
        // the query typed again, with the view where Enter left it, below
        // what it finds from the caret: the view goes back up to it. It
        // was told to go down, by the matches' order, and stayed
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                for typed in ["l", "line 15"] {
                    state.set_search_query(typed, true, cx);
                }
            })
        });
        // "line 15" and "line 150" to "line 159": the caret is on 150
        assert_current_match(&input, &mut cx, 1, 150, "typed again, above the view");
        // nothing after the caret: round to the first in the text
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                state.close_search(cx);
                let at = state.text.line_start_offset(399);
                state.move_to(at, None, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                state.open_search(false, cx);
                state.set_search_query("needle 2", true, cx);
            })
        });
        assert_current_match(&input, &mut cx, 0, 20, "nothing after the caret");
    }

    /// The find bar opened on a selection has that occurrence as its
    /// current match, and Enter goes to the one after it, as VS Code and
    /// IntelliJ do. The search began at the top of the view: a match above
    /// the selection was the current one, and Enter went back up to an
    /// earlier one (elundqvist/kvist#208). With nothing selected and the
    /// caret scrolled out of sight, it begins with what is in view.
    #[gpui::test]
    fn test_a_search_begins_at_the_selection_or_the_caret(cx: &mut TestAppContext) {
        let (input, mut cx) = needles(cx, 245);
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                // "line 2" is on line 2, on 21 to 29 and from 200 on, of
                // which the view holds some above the one selected, on 240
                let at = state.text.line_start_offset(240);
                state.selected_range = (at..at + "line 2".len()).into();
                let top = state
                    .last_layout
                    .as_ref()
                    .unwrap()
                    .visible_range_offset
                    .start;
                assert!(
                    top < state.text.line_start_offset(239),
                    "the view begins above it"
                );
                state.open_search(false, cx);
                assert_eq!(state.search_session().query, "line 2");
                assert_eq!(state.search_session().anchor_offset, Some(at));
            })
        });
        // counted from none, the one on 240 is the 50th
        let selected = 1 + 9 + 40;
        assert_current_match(&input, &mut cx, selected, 240, "the selection");
        cx.update(|_, cx| input.update(cx, |state, cx| state.next_search_match(cx)));
        assert_current_match(
            &input,
            &mut cx,
            selected + 1,
            241,
            "Enter, the one after it",
        );

        // the caret left on line 10 and the view scrolled away from it
        let (input, mut cx) = needles(&mut cx.cx, 10);
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                let line_height = state.last_layout.as_ref().unwrap().line_height;
                state.set_scroll_offset(gpui::point(px(0.), -line_height * 300usize), cx);
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                state.open_search(false, cx);
                assert_eq!(
                    state.search_session().anchor_offset,
                    Some(state.text.line_start_offset(300)),
                    "the top of the view"
                );
                state.set_search_query("needle", true, cx);
            })
        });
        assert_current_match(&input, &mut cx, 2, 330, "the caret out of sight");
    }

    /// The find bar opened on the query it last held moves nothing, as VS
    /// Code's does not: its current match is the first from the caret, and
    /// the first Enter goes to that one where it is out of sight, not past
    /// it, and on to the next where it is in view.
    #[gpui::test]
    fn test_the_find_bar_opens_on_its_last_query_and_moves_nothing(cx: &mut TestAppContext) {
        let (input, mut cx) = needles(cx, 150);
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                state.open_search(false, cx);
                state.set_search_query("needle", true, cx);
                state.close_search(cx);
                let at = state.text.line_start_offset(150);
                state.move_to(at, None, cx);
            })
        });
        cx.run_until_parked();
        let was = cx.update(|_, cx| input.read_with(cx, |state, _| state.scroll_offset()));
        cx.update(|_, cx| input.update(cx, |state, cx| state.open_search(false, cx)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(state.search_session().query, "needle");
                assert_eq!(state.search_session().matcher.current_match_index(), 1);
                assert_eq!(
                    state.scroll_offset(),
                    was,
                    "the bar opened and the view stayed"
                );
            })
        });
        cx.update(|_, cx| input.update(cx, |state, cx| state.next_search_match(cx)));
        assert_current_match(
            &input,
            &mut cx,
            1,
            250,
            "the first Enter, to the one out of sight",
        );
        cx.update(|_, cx| input.update(cx, |state, cx| state.next_search_match(cx)));
        assert_current_match(&input, &mut cx, 2, 330, "the second, on to the next");
        // opened with its current match in view, Enter goes on from it
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                state.close_search(cx);
                let at = state.text.line_start_offset(248);
                state.move_to(at, None, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| input.update(cx, |state, cx| state.open_search(false, cx)));
        assert_current_match(&input, &mut cx, 1, 250, "opened beside it");
        cx.update(|_, cx| input.update(cx, |state, cx| state.next_search_match(cx)));
        assert_current_match(&input, &mut cx, 2, 330, "Enter, on from the one in view");
        // Shift+Enter from where the bar opened goes to the one before
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                state.close_search(cx);
                let at = state.text.line_start_offset(150);
                state.move_to(at, None, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                state.open_search(false, cx);
                state.previous_search_match(cx);
            })
        });
        assert_current_match(
            &input,
            &mut cx,
            0,
            20,
            "Shift+Enter, the one before the caret",
        );
    }

    /// With the find bar shut, an edit does not look for its query in the
    /// text again: it copied and scanned the whole text at every keystroke
    /// once a query had been set (elundqvist/kvist#315). The bar finds the
    /// matches of the text as it is when it opens, and follows each edit
    /// while it is open.
    #[gpui::test]
    fn test_a_shut_find_bar_does_not_search_at_each_edit(cx: &mut TestAppContext) {
        let (input, mut cx) = needles(cx, 0);
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.open_search(false, cx);
                state.set_search_query("needle", true, cx);
                assert_eq!(state.search_session().matcher.len(), 3);
                state.close_search(cx);
                // shut, it holds neither the text it looked in nor its
                // matches, so nothing but the editor keeps an old text
                let matcher = &state.search_session().matcher;
                assert_eq!(
                    matcher.text().len(),
                    0,
                    "the text is let go as the bar shuts"
                );
                assert!(matcher.is_empty());
                // a fourth typed at the top, with the bar shut
                state.replace_text_in_range(None, "needle\n", window, cx);
                let matcher = &state.search_session().matcher;
                assert_eq!(
                    (matcher.len(), matcher.text().len()),
                    (0, 0),
                    "not looked for while the bar is shut"
                );
                state.open_search(false, cx);
                let matcher = &state.search_session().matcher;
                assert_eq!(matcher.len(), 4, "found as the bar opens");
                assert_eq!(matcher.matched_ranges()[0], 0..6);
                // open, an edit is followed at once
                let end = state.text.len();
                state.replace_text_in_range(Some(end..end), "needle", window, cx);
                assert_eq!(
                    state.search_session().matcher.len(),
                    5,
                    "followed while the bar is open"
                );
            })
        });
    }

    /// A key typed at a caret out of sight below brings the caret's line
    /// into view at once, as VS Code and IntelliJ do. The layout's follow
    /// of the caret moved the view a line a key, so with the view at the
    /// top, where the find bar's first match had taken it, and the caret
    /// thousands of lines below, the view crept down a line a key and the
    /// caret never came into sight (elundqvist/kvist#369).
    #[gpui::test]
    fn test_a_key_typed_at_a_caret_out_of_sight_brings_it_into_view(cx: &mut TestAppContext) {
        let (input, mut cx) = needles(cx, 330);
        cx.update(|_, cx| {
            input.update(cx, |state, cx| state.set_scroll_offset(point(px(0.), px(0.)), cx))
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                let rows = state.visible_row_range().unwrap();
                assert!(!rows.contains(&330), "the caret is out of sight, {rows:?}");
            })
        });
        cx.update(|window, cx| {
            input.update(cx, |state, cx| state.replace_text_in_range(None, "x", window, cx))
        });
        cx.run_until_parked();
        cx.update(|_, cx| input.read_with(cx, |state, _| assert_caret_in_view(state, "below")));

        // and above, the view far below it
        let (input, mut cx) = needles(&mut cx.cx, 20);
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                let line_height = state.last_layout.as_ref().unwrap().line_height;
                state.set_scroll_offset(point(px(0.), -line_height * 300usize), cx);
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            input.update(cx, |state, cx| state.replace_text_in_range(None, "x", window, cx))
        });
        cx.run_until_parked();
        cx.update(|_, cx| input.read_with(cx, |state, _| assert_caret_in_view(state, "above")));

        // Enter, and a word deleted, in the frame after them, as a key:
        // left to the layout's follow of the caret, which goes to it, the
        // frame laid out the lines at the old place and painted them at
        // the new, and the caret's line came a frame later
        let (input, mut cx) = needles(&mut cx.cx, 330);
        cx.update(|_, cx| {
            input.update(cx, |state, cx| state.set_scroll_offset(point(px(0.), px(0.)), cx))
        });
        cx.run_until_parked();
        let enter = Enter {
            secondary: false,
            shift: false,
        };
        cx.update(|window, cx| input.update(cx, |state, cx| state.enter(&enter, window, cx)));
        cx.run_until_parked();
        cx.update(|_, cx| input.read_with(cx, |state, _| assert_caret_in_view(state, "Enter")));
        let (input, mut cx) = needles(&mut cx.cx, 330);
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                let at = state.text.line_end_offset(330);
                state.move_to(at, None, cx);
                state.set_scroll_offset(point(px(0.), px(0.)), cx)
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.delete_previous_word(&DeleteToPreviousWordStart, window, cx)
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| input.read_with(cx, |state, _| assert_caret_in_view(state, "a word deleted")));
    }

    /// The editor's problems move with what is typed above and before
    /// them, and stay where they are for what is typed after them. They
    /// were dropped at every edit and had to be handed back at every key
    /// (elundqvist/kvist#370).
    #[gpui::test]
    fn test_problems_move_with_an_edit(cx: &mut TestAppContext) {
        use crate::input::{Diagnostic, Position};
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("select a;\nselect b;\nselect c;\n", window, cx);
                let set = state.diagnostics_mut().unwrap();
                set.push(Diagnostic::new(Position::new(1, 7)..Position::new(1, 8), "b"));
                set.push(Diagnostic::new(Position::new(2, 7)..Position::new(2, 8), "c"));
            })
        });
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                // a line put in above both, and a column before `c`
                state.replace_text_in_range(Some(0..0), "-- note\n", window, cx);
                let at = state.text.line_start_offset(3);
                let at = state.range_to_utf16(&(at..at));
                state.replace_text_in_range(Some(at), "  ", window, cx);
                // and something after `b`, on its line
                let end = state.text.line_end_offset(2);
                let end = state.range_to_utf16(&(end..end));
                state.replace_text_in_range(Some(end), " -- b", window, cx);
                let marks: Vec<(String, String)> = state
                    .diagnostics()
                    .unwrap()
                    .iter()
                    .map(|d| (d.message.to_string(), state.text.slice(d.range.clone()).to_string()))
                    .collect();
                assert_eq!(marks, [("b".into(), "b".into()), ("c".into(), "c".into())]);
                let at: Vec<Position> = state.diagnostics().unwrap().iter().map(|d| d.diagnostic.range.start).collect();
                assert_eq!(at, [Position::new(2, 7), Position::new(3, 9)]);
            })
        });
    }

    /// A replace-all of the find bar, its undo and its redo leave a
    /// problem between two matches on its text: each match is a change of
    /// its own, in one step of undo, and the problems move with each. The
    /// text was made anew in one change, from the first match to the last
    /// for its undo, and the underline between them went with it until
    /// the problems were handed again (elundqvist/kvist#370).
    #[gpui::test]
    fn test_problems_stay_between_the_matches_of_a_replace_all(cx: &mut TestAppContext) {
        use crate::input::{Diagnostic, Position};
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        let text = "fn main() {\n    let x: i32 = \"no\";\n    if x {\n    }\n}\n";
        let marks = |state: &InputBaseState<EditorMode>| -> Vec<(String, Position)> {
            state
                .diagnostics()
                .unwrap()
                .iter()
                .map(|d| (state.text.slice(d.range.clone()).to_string(), d.diagnostic.range.start))
                .collect()
        };
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value(text, window, cx);
                let set = state.diagnostics_mut().unwrap();
                set.push(Diagnostic::new(Position::new(1, 17)..Position::new(1, 21), "expected i32"));
                state.open_search(true, cx);
                state.set_search_query("{", true, cx);
            })
        });
        cx.run_until_parked();
        let said = vec![("\"no\"".to_string(), Position::new(1, 17))];
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                assert_eq!(state.replace_all_search_matches("{ ", window, cx), 2);
                assert_eq!(state.text.to_string(), text.replace('{', "{ "));
                assert_eq!(marks(state), said, "the replace");
                state.undo(&Undo, window, cx);
                assert_eq!(state.text.to_string(), text, "one step of undo");
                assert_eq!(marks(state), said, "its undo");
                state.redo(&Redo, window, cx);
                assert_eq!(state.text.to_string(), text.replace('{', "{ "));
                assert_eq!(marks(state), said, "its redo");
                state.undo(&Undo, window, cx);
                assert_eq!(state.text.to_string(), text);
            })
        });
    }

    /// An application's own changes, a project-wide replace's in the file
    /// it shows, go in through `replace_steps` as the find bar's do, into
    /// a read-only editor too, as the other programmatic edits go: a
    /// problem between two of them stays on its text, and one undo takes
    /// them all back (elundqvist/kvist#383).
    #[gpui::test]
    fn test_an_applications_steps_keep_the_problems_between_them(cx: &mut TestAppContext) {
        use crate::input::{Diagnostic, Position};
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        let text = "old(1);\nlet x: i32 = \"no\";\nold(2); old(3);\n";
        let marks = |state: &InputBaseState<EditorMode>| -> Vec<String> {
            state
                .diagnostics()
                .unwrap()
                .iter()
                .map(|d| state.text.slice(d.range.clone()).to_string())
                .collect()
        };
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value(text, window, cx);
                state.set_readonly(true, cx);
                let set = state.diagnostics_mut().unwrap();
                set.push(Diagnostic::new(Position::new(1, 13)..Position::new(1, 17), "expected i32"));
                // the last first: the second line of matches, then the first
                let third = text.rfind("old").unwrap();
                let second = text.find("old(2)").unwrap();
                let edits: Vec<(Range<usize>, &str)> =
                    vec![(second..third + 3, "new(2); new"), (0..3, "new")];
                state.replace_steps(&edits, window, cx);
                assert_eq!(state.text.to_string(), text.replace("old", "new"));
                assert_eq!(marks(state), ["\"no\""], "the problem between them");
                state.set_readonly(false, cx);
                state.undo(&Undo, window, cx);
                assert_eq!(state.text.to_string(), text, "one step of undo");
                assert_eq!(marks(state), ["\"no\""], "and after it");
            })
        });
    }

    /// The caret keeps to its text through a replace-all, its undo and its
    /// redo, the find bar's and an application's: moved by the matches
    /// before it, and, within one, at the end of what went there. It went
    /// to the end of the text, and a redo took the view to the last line
    /// of the file (elundqvist/kvist#383).
    #[gpui::test]
    fn test_the_caret_keeps_to_its_text_through_a_replace_all(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        let text = "fn main() {\n    let x: i32 = \"no\";\n    if x {\n    }\n}\n";
        let caret = text.find("\"no\"").unwrap();
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value(text, window, cx);
                state.open_search(true, cx);
                state.set_search_query("{", true, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_range(caret..caret, cx);
                assert_eq!(state.replace_all_search_matches("{ ", window, cx), 2);
                // one `{` before it, a byte longer
                assert_eq!(state.selected_range(), caret + 1..caret + 1, "the replace");
                state.undo(&Undo, window, cx);
                assert_eq!(state.selected_range(), caret..caret, "its undo");
                state.redo(&Redo, window, cx);
                assert_eq!(state.selected_range(), caret + 1..caret + 1, "its redo");
                state.undo(&Undo, window, cx);
                // a selection from the `x` of `let x` to within `i32`, by
                // an application's changes, the last first
                let (x, i32_at) = (text.find("x:").unwrap(), text.find("i32").unwrap());
                state.set_selected_range(x..i32_at + 1, cx);
                let edits: Vec<(Range<usize>, &str)> = vec![(i32_at..i32_at + 3, "u64"), (0..2, "pub fn")];
                state.replace_steps(&edits, window, cx);
                assert_eq!(state.text.to_string(), text.replacen("fn", "pub fn", 1).replacen("i32", "u64", 1));
                assert_eq!(state.selected_range(), x + 4..i32_at + 4 + 3, "the end within `i32` at the end of `u64`");
                state.undo(&Undo, window, cx);
                state.redo(&Redo, window, cx);
                assert_eq!(state.selected_range(), x + 4..i32_at + 4 + 3, "and its redo");
            })
        });
    }

    /// A text set whole holds none of the old one's problems, by
    /// `set_value` and by `replace_all`, so whoever hands them hands them
    /// again. Moved by one edit over the whole, every one was left an
    /// empty mark at the start, which the hover found, and Kvist took
    /// them to be held still and handed none after Blame or a file read
    /// again (elundqvist/kvist#370).
    #[gpui::test]
    fn test_a_text_set_whole_holds_no_problems(cx: &mut TestAppContext) {
        use crate::input::{Diagnostic, Position};
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        let mark = |state: &mut InputBaseState<EditorMode>| {
            let set = state.diagnostics_mut().unwrap();
            set.push(Diagnostic::new(Position::new(1, 7)..Position::new(1, 8), "b"));
            assert_eq!(set.len(), 1);
        };
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("select a;\nselect b;\n", window, cx);
                mark(state);
                state.set_value("select a;\nselect b;\n", window, cx);
                assert_eq!(state.diagnostics().unwrap().len(), 0, "set_value");
                assert!(state.diagnostics().unwrap().for_offset(0).is_none());
                mark(state);
                state.replace_all("select a;\nselect bb;\n", window, cx);
                assert_eq!(state.diagnostics().unwrap().len(), 0, "replace_all");
            })
        });
    }

    /// Finds folds as a syntax tree does: each `fn` line to the `}` line
    /// after it, and for an edit those whose bytes it reaches.
    struct Blocks;

    impl Blocks {
        fn all(text: &Rope) -> Vec<(Range<usize>, crate::input::FoldRange)> {
            let mut blocks = Vec::new();
            let mut open = None;
            for row in 0..text.lines_len() {
                let line = text.slice_line(row).to_string();
                if line.starts_with("fn ") {
                    open = Some(row);
                } else if line.starts_with('}') {
                    if let Some(start) = open.take() {
                        let bytes = text.line_start_offset(start)..text.line_start_offset(row) + 1;
                        blocks.push((bytes, crate::input::FoldRange::new(start, row)));
                    }
                }
            }
            blocks
        }
    }

    impl crate::input::InputHighlighter for Blocks {
        fn language(&self) -> SharedString {
            "rust".into()
        }

        fn update(&mut self, _: Option<crate::input::InputEdit>, _: &Rope, _: bool, _: &mut Window, _: &mut Context<crate::input::EditorState>) {}

        fn styles(&self, range: &Range<usize>, _: &dyn crate::input::HighlightStyleResolver) -> Vec<(Range<usize>, gpui::HighlightStyle)> {
            vec![(range.clone(), gpui::HighlightStyle::default())]
        }

        fn fold_ranges(&self, text: &Rope) -> Vec<crate::input::FoldRange> {
            Self::all(text).into_iter().map(|(_, fold)| fold).collect()
        }

        fn fold_ranges_for_edit(&self, range: Range<usize>, text: &Rope) -> Vec<crate::input::FoldRange> {
            Self::all(text)
                .into_iter()
                .filter(|(bytes, _)| bytes.start < range.end && bytes.end > range.start)
                .map(|(_, fold)| fold)
                .collect()
        }
    }

    /// Functions cut and pasted elsewhere leave every function with its
    /// fold: the one a cut ended at, and the one a paste began at, are
    /// found again with those the edit reached. Their folds were taken
    /// with the edit's lines and looked for in its bytes alone, so the
    /// two lost their chevrons until the file was read again.
    #[gpui::test]
    fn test_functions_cut_and_pasted_keep_their_folds(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        // five functions of nine lines each
        let text: String = (0..5)
            .map(|k| format!("fn k{k}(x: i32) -> i32 {{\n    if x > {k} {{\n        x + 1\n    }} else {{\n        x - 1\n    }}\n    // k{k}\n    // end\n}}\n"))
            .collect();
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_highlighter_factory(
                    Rc::new(|_: &str| Some(Box::new(Blocks) as Box<dyn crate::input::InputHighlighter>)),
                    cx,
                );
                state.set_value(text.clone(), window, cx);
            })
        });
        cx.run_until_parked();
        let folds_of_each = |state: &InputBaseState<EditorMode>| -> Vec<(usize, bool)> {
            Blocks::all(&state.text)
                .into_iter()
                .map(|(_, fold)| (fold.start_line, state.display_map.is_fold_candidate(fold.start_line)))
                .collect()
        };
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert!(folds_of_each(state).iter().all(|(_, has)| *has), "as read: {:?}", folds_of_each(state));
            })
        });
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                // k1 to k3 cut, from the start of k1's line to that of k4's
                let cut = state.text.line_start_offset(9)..state.text.line_start_offset(36);
                let taken = state.text.slice(cut.clone()).to_string();
                let range = state.range_to_utf16(&cut);
                state.replace_text_in_range(Some(range), "", window, cx);
                assert_eq!(folds_of_each(state), [(0, true), (9, true)], "after the cut");
                // and pasted at the top
                state.replace_text_in_range(Some(0..0), &taken, window, cx);
                let all = folds_of_each(state);
                assert_eq!(all.len(), 5);
                assert!(all.iter().all(|(_, has)| *has), "after the paste: {all:?}");
            })
        });
    }

    /// A replace-all of the find bar, its undo and its redo keep the folds
    /// shut, moved by the lines the matches before them put in, and those
    /// a match is in grown by its lines. Taken as one edit from the first
    /// match to the last, every fold between them opened, at the replace
    /// and at its undo (elundqvist/kvist#384); and one a match was in
    /// opened, as any edit in it did (elundqvist/kvist#404).
    #[gpui::test]
    fn test_a_replace_all_keeps_the_folds(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        // five functions of nine lines each, a match in k0, k2 and k4
        let text: String = (0..5)
            .map(|k| {
                let body = if k % 2 == 0 { "needle" } else { "x" };
                format!("fn k{k}(x: i32) -> i32 {{\n    if x > {k} {{\n        {body} + 1\n    }} else {{\n        x - 1\n    }}\n    // k{k}\n    // end\n}}\n")
            })
            .collect();
        let folded = |state: &InputBaseState<EditorMode>| -> Vec<(usize, usize)> {
            state
                .display_map
                .folded_ranges()
                .iter()
                .map(|fold| (fold.start_line, fold.end_line))
                .collect()
        };
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_highlighter_factory(
                    Rc::new(|_: &str| Some(Box::new(Blocks) as Box<dyn crate::input::InputHighlighter>)),
                    cx,
                );
                state.set_value(text.clone(), window, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                for line in [0, 9, 27, 36] {
                    state.display_map.set_folded(line, true);
                }
                assert_eq!(folded(state), [(0, 8), (9, 17), (27, 35), (36, 44)]);
                state.open_search(true, cx);
                state.set_search_query("needle", true, cx);
            })
        });
        cx.run_until_parked();
        // each match a line longer: k4 holds one and grows by its line,
        // k1 and k3 move; k0, which the bar opened as it went to its match,
        // stays open
        let replaced = [(10, 18), (29, 37), (38, 47)];
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                assert_eq!(state.replace_all_search_matches("pin\n        // more", window, cx), 3);
                assert_eq!(state.text.to_string(), text.replace("needle", "pin\n        // more"));
                assert_eq!(folded(state), replaced, "the replace");
                state.undo(&Undo, window, cx);
                assert_eq!(state.text.to_string(), text, "one step of undo");
                assert_eq!(folded(state), [(9, 17), (27, 35), (36, 44)], "its undo");
                state.redo(&Redo, window, cx);
                assert_eq!(folded(state), replaced, "its redo");
                // and every fold has its chevron where its function is
                let candidates: Vec<usize> = Blocks::all(&state.text).into_iter().map(|(_, fold)| fold.start_line).collect();
                assert!(candidates.iter().all(|&line| state.display_map.is_fold_candidate(line)), "{candidates:?}");
            })
        });
    }

    /// An application's changes (`replace_steps`), as a rename makes them,
    /// their undo and their redo move a decoration between two of them as
    /// the changes before it move its text, so a line's breakpoint stays on
    /// its line, and keep the folds the changes are in shut; the line
    /// above a fold taken moves the fold, and a paste of the whole text
    /// over itself leaves the folds and the decorations as they are. The
    /// decoration went to the line of the first change and
    /// stayed there through the undo, and every one of those folds opened
    /// (elundqvist/kvist#404).
    #[gpui::test]
    fn test_changes_keep_decorations_and_folds_where_they_are(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        // five functions of nine lines each and a line between them, `sum`
        // on the first line and on two hidden lines of each
        let text: String = (0..5)
            .map(|k| format!("fn k{k}(sum: i32) -> i32 {{\n    if sum > {k} {{\n        sum + 1\n    }} else {{\n        0\n    }}\n    // k{k}\n    // end\n}}\n\n"))
            .collect();
        let folded = |state: &InputBaseState<EditorMode>| -> Vec<(usize, usize)> {
            state.display_map.folded_ranges().iter().map(|fold| (fold.start_line, fold.end_line)).collect()
        };
        let line = |text: &Rope, l: usize| text.line_start_offset(l)..text.line_start_offset(l + 1);
        let collection = cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_highlighter_factory(
                    Rc::new(|_: &str| Some(Box::new(Blocks) as Box<dyn crate::input::InputHighlighter>)),
                    cx,
                );
                state.set_value(text.clone(), window, cx);
                // a breakpoint on k1's `else` line, and one on k2's first
                let style = gpui::HighlightStyle::default();
                state.create_decorations_collection(
                    vec![
                        crate::input::TextDecoration::new(line(&state.text, 13), style),
                        crate::input::TextDecoration::new(line(&state.text, 20), style),
                    ],
                    cx,
                )
            })
        });
        cx.run_until_parked();
        let all_folded = [(0, 8), (10, 18), (30, 38), (40, 48)];
        cx.update(|_, cx| {
            input.update(cx, |state, _| {
                for (line, _) in all_folded {
                    state.display_map.set_folded(line, true);
                }
                assert_eq!(folded(state), all_folded);
            })
        });
        let decorated = |cx: &mut VisualTestContext| -> Vec<Range<usize>> { cx.update(|_, cx| collection.get_ranges(cx)) };
        // `sum` renamed `total` in k0, k1 and k3, the last first
        let in_k = |text: &str, at: usize| text[..at].matches('\n').count() / 10;
        let mut places: Vec<Range<usize>> = text
            .match_indices("sum")
            .map(|(i, _)| i..i + 3)
            .filter(|r| ![2, 4].contains(&in_k(&text, r.start)))
            .collect();
        places.reverse();
        let renamed: String = text
            .split_inclusive('\n')
            .enumerate()
            .map(|(row, l)| if [2, 4].contains(&(row / 10)) { l.to_string() } else { l.replace("sum", "total") })
            .collect();
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                let edits: Vec<(Range<usize>, &str)> = places.iter().map(|r| (r.clone(), "total")).collect();
                state.replace_steps(&edits, window, cx);
                assert_eq!(state.text.to_string(), renamed);
                assert_eq!(folded(state), all_folded, "renamed in them");
            })
        });
        let before = Rope::from(text.as_str());
        let after = Rope::from(renamed.as_str());
        assert_eq!(decorated(&mut cx), [line(&after, 13), line(&after, 20)], "the rename");
        cx.update(|window, cx| input.update(cx, |state, cx| state.undo(&Undo, window, cx)));
        assert_eq!(decorated(&mut cx), [line(&before, 13), line(&before, 20)], "its undo");
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                assert_eq!(state.text.to_string(), text);
                assert_eq!(folded(state), all_folded, "its undo");
                state.redo(&Redo, window, cx);
                assert_eq!(folded(state), all_folded, "its redo");
            })
        });
        assert_eq!(decorated(&mut cx), [line(&after, 13), line(&after, 20)], "its redo");
        let moved = [(0, 8), (10, 18), (29, 37), (39, 47)];
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                // the line above k3 taken
                let above = line(&state.text, 29);
                let range = state.range_to_utf16(&above);
                state.replace_text_in_range(Some(range), "", window, cx);
                assert_eq!(folded(state), moved, "the line above k3 taken");
                // the whole text pasted over itself
                let same = state.text.to_string();
                state.select_all(window, cx);
                state.replace_text_in_range(None, &same, window, cx);
                assert_eq!(state.text.to_string(), same);
                assert_eq!(folded(state), moved, "pasted over itself");
                // which is no step of undo: the undo puts the line back
                state.undo(&Undo, window, cx);
                assert_eq!(folded(state), all_folded, "the line put back");
            })
        });
        assert_eq!(decorated(&mut cx), [line(&after, 13), line(&after, 20)], "the paste, and the line put back");
    }

    /// An undo that puts the caret back in a fold made since opens it, as
    /// VS Code opens a fold its caret goes into: the edit in it no longer
    /// opens it (elundqvist/kvist#404).
    #[gpui::test]
    fn test_an_undo_into_a_fold_opens_it(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        let text: String = (0..3).map(|k| format!("fn k{k}() {{\n    let a = 1;\n    let b = 2;\n}}\n")).collect();
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_highlighter_factory(
                    Rc::new(|_: &str| Some(Box::new(Blocks) as Box<dyn crate::input::InputHighlighter>)),
                    cx,
                );
                state.set_value(text.clone(), window, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                // typed on k1's second line, then k1 folded and the caret
                // put on k0
                let at = state.text.line_start_offset(6) + 4;
                state.set_selected_range(at..at, cx);
                state.replace_text_in_range(None, "x", window, cx);
                state.display_map.set_folded(4, true);
                assert_eq!(state.display_map.folded_ranges().len(), 1);
                state.set_selected_range(0..0, cx);
                state.undo(&Undo, window, cx);
                assert_eq!(state.text.to_string(), text);
                assert!(state.display_map.folded_ranges().is_empty(), "the undo's caret is in it");
                assert_eq!(state.cursor(), at);
            })
        });
    }

    /// Counts what an editor hands its highlighter: the edit of each update.
    struct Edits(Rc<std::cell::RefCell<Vec<Option<crate::input::InputEdit>>>>);

    impl crate::input::InputHighlighter for Edits {
        fn language(&self) -> SharedString {
            "sql".into()
        }

        fn update(
            &mut self,
            edit: Option<crate::input::InputEdit>,
            _: &Rope,
            _: bool,
            _: &mut Window,
            _: &mut Context<crate::input::EditorState>,
        ) {
            self.0.borrow_mut().push(edit);
        }

        fn styles(
            &self,
            range: &Range<usize>,
            _: &dyn crate::input::HighlightStyleResolver,
        ) -> Vec<(Range<usize>, gpui::HighlightStyle)> {
            vec![(range.clone(), gpui::HighlightStyle::default())]
        }

        fn fold_ranges(&self, _: &Rope) -> Vec<crate::input::FoldRange> {
            Vec::new()
        }
    }

    /// Counts how many changes an editor hands its highlighter one by one
    /// at each update (`InputHighlighter::update_steps`), none for one edit.
    struct Steps(Rc<std::cell::RefCell<Vec<usize>>>);

    impl crate::input::InputHighlighter for Steps {
        fn language(&self) -> SharedString {
            "sql".into()
        }

        fn update(
            &mut self,
            edit: Option<crate::input::InputEdit>,
            _: &Rope,
            _: bool,
            _: &mut Window,
            _: &mut Context<crate::input::EditorState>,
        ) {
            if edit.is_some() {
                self.0.borrow_mut().push(0);
            }
        }

        fn update_steps(
            &mut self,
            edits: &[crate::input::InputEdit],
            _: crate::input::InputEdit,
            _: &Rope,
            _: bool,
            _: &mut Window,
            _: &mut Context<crate::input::EditorState>,
        ) {
            self.0.borrow_mut().push(edits.len());
        }

        fn styles(
            &self,
            range: &Range<usize>,
            _: &dyn crate::input::HighlightStyleResolver,
        ) -> Vec<(Range<usize>, gpui::HighlightStyle)> {
            vec![(range.clone(), gpui::HighlightStyle::default())]
        }

        fn fold_ranges(&self, _: &Rope) -> Vec<crate::input::FoldRange> {
            Vec::new()
        }
    }

    /// An application's changes, their undo and their redo are handed to
    /// the highlighter one by one, as well as the one edit that covers
    /// them, so that a tree it moves with an edit keeps what lies between
    /// them in its place; a key is one edit (elundqvist/kvist#404).
    #[gpui::test]
    fn test_changes_are_handed_to_the_highlighter_one_by_one(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        let steps = Rc::new(std::cell::RefCell::new(Vec::new()));
        let factory_steps = steps.clone();
        let text = "fn sum(sum: u32) -> u32 {\n    sum\n}\n";
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_highlighter_factory(
                    Rc::new(move |_: &str| {
                        Some(Box::new(Steps(factory_steps.clone())) as Box<dyn crate::input::InputHighlighter>)
                    }),
                    cx,
                );
                state.set_value(text, window, cx);
            })
        });
        cx.run_until_parked();
        steps.borrow_mut().clear();
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                let edits: Vec<(Range<usize>, &str)> =
                    text.rmatch_indices("sum").map(|(at, _)| (at..at + 3, "total")).collect();
                state.replace_steps(&edits, window, cx);
                state.undo(&Undo, window, cx);
                state.redo(&Redo, window, cx);
                state.move_to(0, None, cx);
                state.replace_text_in_range(None, "x", window, cx);
            })
        });
        assert_eq!(*steps.borrow(), [3, 3, 3, 0], "the changes, their undo, their redo, and a key");
    }

    /// An undo, and the redo after it, is one edit to what follows the
    /// text, as VS Code and IntelliJ take it: the highlighter, the folds
    /// and the problems are handed one edit from where the first change
    /// begins to where the last ends. Each change of the step was made
    /// again as an edit of its own, so 28 keys typed in a large file and
    /// undone parsed, folded and marked the text 28 times over, and the
    /// highlighter's own work grew with each (elundqvist/kvist#368).
    #[gpui::test]
    fn test_an_undo_of_many_keys_is_one_edit(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        let edits = Rc::new(std::cell::RefCell::new(Vec::new()));
        // a megabyte of text
        let text = numbered_lines(0..100_000);
        assert!(text.len() > 1_000_000);
        let factory_edits = edits.clone();
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_highlighter_factory(
                    Rc::new(move |_: &str| {
                        Some(Box::new(Edits(factory_edits.clone()))
                            as Box<dyn crate::input::InputHighlighter>)
                    }),
                    cx,
                );
                state.set_value(text.clone(), window, cx);
            })
        });
        cx.run_until_parked();
        let at = cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                let at = state.text.line_start_offset(50_000) + "line".len();
                state.move_to(at, None, cx);
                at
            })
        });
        cx.run_until_parked();
        // 50 keys, one step of undo
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                for _ in 0..50 {
                    state.replace_text_in_range(None, "y", window, cx);
                }
            })
        });
        cx.run_until_parked();
        let typed = cx.update(|_, cx| input.read_with(cx, |state, _| state.text.to_string()));
        edits.borrow_mut().clear();

        let began = std::time::Instant::now();
        cx.update(|window, cx| input.update(cx, |state, cx| state.undo(&Undo, window, cx)));
        let took = began.elapsed();
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(state.text.to_string(), text, "the keys taken back");
                assert_eq!(state.cursor(), at);
            })
        });
        let undone: Vec<_> = edits.borrow_mut().drain(..).collect();
        assert_eq!(undone.len(), 1, "one edit for the undo");
        let edit = undone[0].clone().expect("an edit");
        assert_eq!(
            (edit.start_byte, edit.old_end_byte, edit.new_end_byte),
            (at, at + 50, at),
            "from where the first change begins to where the last ends"
        );
        assert!(took < std::time::Duration::from_secs(1), "the undo took {took:?}");

        cx.update(|window, cx| input.update(cx, |state, cx| state.redo(&Redo, window, cx)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(state.text.to_string(), typed, "the keys again");
                assert_eq!(state.cursor(), at + 50);
            })
        });
        let redone: Vec<_> = edits.borrow_mut().drain(..).collect();
        assert_eq!(redone.len(), 1, "one edit for the redo");
        let edit = redone[0].clone().expect("an edit");
        assert_eq!((edit.start_byte, edit.old_end_byte, edit.new_end_byte), (at, at, at + 50));
    }

    /// A text made anew, a paste over all of it, its undo and its redo,
    /// is handed to the highlighter as no edit, to be parsed afresh; an
    /// edit of part of it, a paste over all but its first byte, as the
    /// edit it is. Handed as an edit of the whole, the trees were edited
    /// node by node to be thrown away, 33 ms of a paste of 1.4 MB over as
    /// much (elundqvist/kvist#384).
    #[gpui::test]
    fn test_a_text_made_anew_is_no_edit_to_the_highlighter(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        let edits = Rc::new(std::cell::RefCell::new(Vec::new()));
        let factory_edits = edits.clone();
        let (text, pasted) = (numbered_lines(0..1_000), numbered_lines(5..2_000));
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_highlighter_factory(
                    Rc::new(move |_: &str| {
                        Some(Box::new(Edits(factory_edits.clone()))
                            as Box<dyn crate::input::InputHighlighter>)
                    }),
                    cx,
                );
                state.set_value(text.clone(), window, cx);
            })
        });
        cx.run_until_parked();
        let handed = |edits: &Rc<std::cell::RefCell<Vec<Option<crate::input::InputEdit>>>>| {
            edits.borrow_mut().drain(..).map(|edit| edit.is_some()).collect::<Vec<_>>()
        };
        edits.borrow_mut().clear();
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.select_all(window, cx);
                state.replace_text_in_range(None, &pasted, window, cx);
                assert_eq!(state.text.to_string(), pasted);
                assert_eq!(handed(&edits), [false], "the paste over all");
                state.undo(&Undo, window, cx);
                assert_eq!(state.text.to_string(), text);
                assert_eq!(handed(&edits), [false], "its undo");
                state.redo(&Redo, window, cx);
                assert_eq!(handed(&edits), [false], "its redo");
                state.undo(&Undo, window, cx);
                assert_eq!(handed(&edits), [false], "its undo again");
                // all but the first byte, and a key typed
                let range = state.range_to_utf16(&(1..state.text.len()));
                state.replace_text_in_range(Some(range), &pasted, window, cx);
                state.replace_text_in_range(None, "x", window, cx);
                assert_eq!(handed(&edits), [true, true], "an edit of part of it");
            })
        });
    }

    /// A match in a fold opens it when it is gone to, by typing or by
    /// Enter, as VS Code and IntelliJ open the fold the current match is
    /// in. Folded, the bar said 1/1 and the match stayed hidden whatever
    /// was pressed.
    #[gpui::test]
    fn test_a_match_gone_to_in_a_fold_opens_it(cx: &mut TestAppContext) {
        use crate::input::FoldRange;
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        let text: String = (0..30)
            .map(|i| {
                if [7, 20].contains(&i) {
                    format!("needle {i}\n")
                } else {
                    format!("line {i}\n")
                }
            })
            .collect();
        let fold = |state: &mut InputBaseState<EditorMode>| {
            state.display_map.set_folded(2, true);
            state.display_map.set_folded(4, true);
            assert!(state.display_map.is_buffer_line_hidden(7));
        };
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value(text, window, cx);
                // one fold inside another, both over line 7
                state.apply_highlighter_fold_candidates(
                    vec![FoldRange::new(2, 12), FoldRange::new(4, 9)],
                    cx,
                );
                fold(state);
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                state.open_search(false, cx);
                state.set_search_query("needle", true, cx);
                assert!(state.display_map.folded_ranges().is_empty(), "typed");
            })
        });
        assert_current_match(&input, &mut cx, 0, 7, "typed, in a fold");
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                state.next_search_match(cx);
                fold(state);
                // round to the first again, which the folds hide
                state.next_search_match(cx);
                assert!(state.display_map.folded_ranges().is_empty(), "Enter");
            })
        });
        assert_current_match(&input, &mut cx, 0, 7, "Enter, in a fold");
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                state.next_search_match(cx);
                fold(state);
                state.previous_search_match(cx);
                assert!(state.display_map.folded_ranges().is_empty(), "Shift+Enter");
            })
        });
        assert_current_match(&input, &mut cx, 0, 7, "Shift+Enter, in a fold");
        // a fold that does not hide the match stays shut
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                state.display_map.set_folded(4, true);
                assert!(!state.display_map.is_buffer_line_hidden(20));
                state.next_search_match(cx);
                assert_eq!(state.display_map.folded_ranges().len(), 1);
            })
        });
        assert_current_match(&input, &mut cx, 1, 20, "beside a fold");
    }

    /// The caret was painted inside the view across as well as down.
    fn assert_caret_in_view_across<M: InputModeKind>(state: &InputBaseState<M>, what: &str) {
        let caret = state.cursor();
        let at = state
            .range_to_bounds(&(caret..caret))
            .expect("the caret has a place");
        let view = state.input_bounds;
        assert!(
            at.left() >= view.left() && at.left() <= view.right(),
            "{what}: the caret at {at:?} is outside the view {view:?}"
        );
    }

    /// An undo that gives the caret's line back its tail, wider than the
    /// view, a redo of typing past the view's width and a paste of a line
    /// wider than it scroll across to the caret. They scroll to the caret
    /// before the lines are laid out, and the column was read from the
    /// last layout, where the line was shorter than the caret's column: the
    /// scroll across was left where it was, and replaced the layout's own
    /// follow of the caret, which left it off to the right
    /// (elundqvist/kvist#201).
    #[gpui::test]
    fn test_an_undo_a_redo_and_a_paste_scroll_across_to_the_caret(cx: &mut TestAppContext) {
        let view = InputView::build_editor(cx, |state| state.soft_wrap(false));
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        let long = "x".repeat(1000);
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value(format!("{long}\nshort\n"), window, cx)
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_range(60..1000, cx);
                state.delete(&Delete, window, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(state.cursor(), 60);
                assert_caret_in_view_across(state, "deleted");
            })
        });

        cx.update(|window, cx| input.update(cx, |state, cx| state.undo(&Undo, window, cx)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(state.cursor(), 1000);
                assert_caret_in_view_across(state, "undone");
            })
        });

        // typing past the view's width, taken back to the line's start, and
        // done again
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                let short = state.text.line_start_offset(1);
                state.move_to(short, None, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| input.update(cx, |state, cx| state.insert(long.clone(), window, cx)));
        cx.run_until_parked();
        cx.update(|window, cx| input.update(cx, |state, cx| state.undo(&Undo, window, cx)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(state.cursor(), state.text.line_start_offset(1));
                assert_caret_in_view_across(state, "typing undone");
            })
        });
        cx.update(|window, cx| input.update(cx, |state, cx| state.redo(&Redo, window, cx)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(state.cursor(), state.text.line_start_offset(1) + 1000);
                assert_caret_in_view_across(state, "redone");
            })
        });

        // and a paste of a line wider than the view, into a short one
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("short\nshort\n", window, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(long.clone()));
            input.update(cx, |state, cx| {
                let short = state.text.line_start_offset(1);
                state.move_to(short, None, cx);
                state.paste(&Paste, window, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(state.cursor(), state.text.line_start_offset(1) + 1000);
                assert_caret_in_view_across(state, "pasted");
            })
        });
    }

    /// A caret moved to a column past the view's width, on a line below a
    /// fold, is scrolled across to. The column was read from the line laid
    /// out at the caret's row less the first row laid out, which below a
    /// fold is another line, or none: the scroll across was left where it
    /// was, and replaced the layout's own follow of the caret.
    #[gpui::test]
    fn test_a_caret_below_a_fold_is_scrolled_across_to(cx: &mut TestAppContext) {
        use crate::input::FoldRange;
        let view = InputView::build_editor(cx, |state| state.soft_wrap(false));
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                let mut text = numbered_lines(0..20);
                text.push_str(&"x".repeat(1000));
                text.push('\n');
                state.set_value(text, window, cx);
                state.apply_highlighter_fold_candidates(vec![FoldRange::new(2, 12)], cx);
                state.display_map.set_folded(2, true);
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                let far = state.text.line_start_offset(20) + 900;
                state.move_to(far, None, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(state.cursor(), state.text.line_start_offset(20) + 900);
                assert_caret_in_view_across(state, "moved");
            })
        });
    }

    /// A caret moved far out of view with nothing to scroll to it first is
    /// followed by the layout, which jumps there after choosing the lines
    /// for the old place: that frame is short of lines, and it asks for the
    /// next, which draws the lines at the caret.
    #[gpui::test]
    fn test_a_frame_short_of_lines_asks_for_the_next(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value(numbered_lines(0..3000), window, cx)
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                let end = state.text.len();
                state.move_to(end, None, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        cx.run_until_parked();

        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                state.selected_range = (10..10).into();
                cx.notify();
            });
        });
        cx.run_until_parked();
        let asked = cx.update(|window, cx| window.simulate_next_frame(cx));
        assert!(asked > 0, "the frame short of lines asks for the next");
        cx.run_until_parked();
        cx.update(|_, cx| input.read_with(cx, |state, _| assert_caret_in_view(state, "moved")));
    }

    /// What lies on a line a fold hides has no place on screen. It was
    /// answered with the start of the line that closes the fold, so a
    /// caller drawing beside each line, a problem's note at its end, drew
    /// every hidden line's there, one over the other (elundqvist/kvist#201).
    #[gpui::test]
    fn test_a_line_a_fold_hides_has_no_bounds(cx: &mut TestAppContext) {
        use crate::input::FoldRange;
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value(numbered_lines(0..12), window, cx);
                state.apply_highlighter_fold_candidates(vec![FoldRange::new(2, 6)], cx);
                state.display_map.set_folded(2, true);
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                let text = &state.text;
                for line in 3..6 {
                    for at in [text.line_start_offset(line), text.line_end_offset(line)] {
                        assert_eq!(state.range_to_bounds(&(at..at)), None, "line {line}");
                    }
                }
                let top = |line: usize| {
                    let at = text.line_end_offset(line);
                    state.range_to_bounds(&(at..at)).expect("shown").top()
                };
                let row = state.line_height().unwrap();
                assert_eq!(
                    top(6) - top(2),
                    row,
                    "the line that closes the fold is right below it"
                );
                assert_eq!(top(7) - top(6), row);
            });
        });
    }

    /// A range on the lines a fold hides, a search match or a document
    /// colour, is not drawn. It was drawn at the start of the line that
    /// closes the fold, the place the editor gave what the fold hid
    /// (elundqvist/kvist#201), as a box of six pixels.
    #[gpui::test]
    fn test_a_range_a_fold_hides_is_not_drawn(cx: &mut TestAppContext) {
        use crate::input::FoldRange;
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value(numbered_lines(0..12), window, cx);
                state.apply_highlighter_fold_candidates(vec![FoldRange::new(2, 6)], cx);
                state.display_map.set_folded(2, true);
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                let text = &state.text;
                let layout = state.last_layout.as_ref().expect("laid out");
                let bounds = state.last_bounds.expect("laid out");
                let drawn = |range: Range<usize>| {
                    TextElement::<EditorMode>::layout_match_range(range, layout, &bounds).is_some()
                };
                let word = |line: usize| text.line_start_offset(line)..text.line_start_offset(line) + 4;
                for line in 3..6 {
                    assert!(!drawn(word(line)), "a match on line {line}, which the fold hides");
                }
                // to the end of the last line hidden, its newline and all
                assert!(!drawn(text.line_start_offset(5)..text.line_start_offset(6)));
                assert!(drawn(word(2)), "on the fold's first line");
                assert!(drawn(word(6)), "on the line that closes it");
                assert!(drawn(word(7)));
                assert!(drawn(text.line_start_offset(4)..text.line_start_offset(6) + 2), "into the line that closes it");
                assert!(drawn(text.line_start_offset(2)..text.line_start_offset(4) + 2), "from the fold's first line into it");
            });
        });
    }

    /// A folded line ends in a `⋯`, as in VS Code, and a click on it opens
    /// the fold. A folded line showed nothing of its body being gone but the
    /// gutter's chevron, which shows on hover (elundqvist/kvist#201).
    #[gpui::test]
    fn test_a_folded_line_ends_in_a_placeholder_that_opens_it(cx: &mut TestAppContext) {
        use crate::input::FoldRange;
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value(numbered_lines(0..12), window, cx);
                state.apply_highlighter_fold_candidates(
                    vec![FoldRange::new(2, 6), FoldRange::new(7, 10)],
                    cx,
                );
                state.display_map.set_folded(2, true);
            });
        });
        cx.run_until_parked();
        let placeholder = cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(
                    state
                        .fold_placeholders
                        .iter()
                        .map(|(line, _)| *line)
                        .collect::<Vec<_>>(),
                    vec![2],
                    "one for the fold that is closed, none for the one that is open"
                );
                let (_, placeholder) = state.fold_placeholders[0];
                assert_eq!(state.fold_placeholder_bounds(2), Some(placeholder));
                assert_eq!(state.fold_placeholder_bounds(7), None, "open");
                let end = state.text.line_end_offset(2);
                let end = state.range_to_bounds(&(end..end)).unwrap();
                assert!(placeholder.left() > end.left(), "after the line's text");
                assert!(placeholder.size.width > px(0.));
                assert!(
                    placeholder.top() >= end.top()
                        && placeholder.bottom() <= end.bottom() + px(0.5)
                );
                placeholder
            })
        });

        cx.simulate_mouse_down(
            placeholder.center(),
            MouseButton::Left,
            gpui::Modifiers::none(),
        );
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert!(!state.display_map.is_folded_at(2), "the click opened it");
                assert!(state.fold_placeholders.is_empty());
                // and did not move the caret, as a click on the text would
                assert_eq!(state.cursor(), 0);
            });
        });
    }

    /// The text scrolls as far as its rows on screen go, and no further
    /// for the rows a fold hides: its last line stops where it stops with
    /// nothing folded. The height to scroll was counted with the hidden
    /// rows, and a view scrolled to the end, or to the end of a paste or an
    /// undo there, was that many rows past the last line, and blank
    /// (elundqvist/kvist#201).
    #[gpui::test]
    fn test_a_fold_scrolls_no_further_than_its_rows(cx: &mut TestAppContext) {
        use crate::input::FoldRange;
        let last_line_at_the_end = |cx: &mut TestAppContext, fold: bool| {
            let view = InputView::<EditorMode>::new(cx);
            let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
            let input = view.input;
            cx.update(|window, cx| {
                input.update(cx, |state, cx| {
                    state.set_value(numbered_lines(0..200), window, cx);
                    state.apply_highlighter_fold_candidates(vec![FoldRange::new(2, 102)], cx);
                    state.display_map.set_folded(2, fold);
                });
            });
            cx.run_until_parked();
            cx.update(|_, cx| {
                input.update(cx, |state, cx| {
                    state.set_scroll_offset(point(px(0.), px(-1_000_000.)), cx)
                })
            });
            cx.run_until_parked();
            cx.update(|_, cx| {
                input.update(cx, |state, cx| {
                    let end = state.text.len();
                    state.move_to(end, None, cx);
                })
            });
            cx.run_until_parked();
            // and a paste at the end, which scrolls to it before it is laid out
            cx.update(|window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(numbered_lines(200..210)));
                input.update(cx, |state, cx| state.paste(&Paste, window, cx));
            });
            cx.run_until_parked();
            cx.update(|_, cx| {
                input.read_with(cx, |state, _| {
                    let end = state.text.len();
                    let at = state.range_to_bounds(&(end..end)).expect("the last line is laid out");
                    (at.top() - state.input_bounds.top(), state.scroll_handle.offset().y)
                })
            })
        };
        let (open, _) = last_line_at_the_end(cx, false);
        let (folded, offset) = last_line_at_the_end(cx, true);
        assert!(offset < px(0.), "scrolled");
        assert_eq!(folded, open, "the last line with a fold of a hundred rows closed");
    }

    /// A `⋯` scrolled under the gutter takes no click there. It was placed
    /// with the text, scrolled, and only its paint was covered by the
    /// gutter's: a click on the number of its line went to it, and opened
    /// the fold (elundqvist/kvist#201).
    #[gpui::test]
    fn test_a_placeholder_under_the_gutter_takes_no_click(cx: &mut TestAppContext) {
        use crate::input::FoldRange;
        cx.update(crate::init);
        let view = InputView::build_editor(cx, |state| state.soft_wrap(false));
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                let mut text = numbered_lines(0..12);
                text.push_str(&"x".repeat(1000));
                state.set_value(text, window, cx);
                state.apply_highlighter_fold_candidates(vec![FoldRange::new(2, 6)], cx);
                state.display_map.set_folded(2, true);
            });
        });
        cx.run_until_parked();
        let (placeholder, gutter) = cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                let gutter = state.last_layout.as_ref().unwrap().line_number_width;
                assert!(gutter > px(20.), "the editor has line numbers");
                (state.fold_placeholders[0].1, gutter)
            })
        });
        // its middle a quarter of the way into the gutter, over the numbers
        let across = gutter * 0.25 - placeholder.center().x;
        assert!(across < px(0.));
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                state.set_scroll_offset(point(across, px(0.)), cx);
            })
        });
        cx.run_until_parked();
        let under = cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(state.scroll_handle.offset().x, across, "scrolled across");
                let (_, under) = state.fold_placeholders[0];
                assert!(under.center().x < state.input_bounds.left() + gutter);
                under
            })
        });
        cx.simulate_mouse_down(under.center(), MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_up(under.center(), MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert!(state.display_map.is_folded_at(2), "the click on the gutter left the fold closed");
            });
        });
    }

    #[gpui::test]
    fn test_number_step(cx: &mut TestAppContext) {
        let input = InputView::build(cx, |state| state).input;

        cx.update(|cx| {
            input.update(cx, |_state, cx| {
                assert_eq!(
                    NumberStep::from(5.).value(123., StepAction::Increment, cx),
                    5.
                );

                // The step can differ by direction at a boundary: at 1.0 it
                // is 0.1 going down and 0.5 going up.
                let step = NumberStep::by_value(|value, action, _cx| {
                    let below = match action {
                        StepAction::Increment => value < 1.0,
                        StepAction::Decrement => value <= 1.0,
                    };
                    if below { 0.1 } else { 0.5 }
                });
                assert_eq!(step.value(0.5, StepAction::Increment, cx), 0.1);
                assert_eq!(step.value(1.0, StepAction::Increment, cx), 0.5);
                assert_eq!(step.value(1.0, StepAction::Decrement, cx), 0.1);
                assert_eq!(step.value(2.0, StepAction::Decrement, cx), 0.5);
            });
        });
    }

    #[gpui::test]
    fn test_number_input_normalization(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| {
            state.mask_pattern(MaskPattern::Number {
                separator: None,
                fraction: None,
            })
        });
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        // Full-width digits and the ideographic full stop are normalized,
        // and the cursor is at the end (in normalized bytes, not the
        // original 12 bytes).
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "12。5", window, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(state.value(), "12.5");
                let cursor: Range<usize> = state.selected_range.into();
                assert_eq!(cursor, 4..4);
            });
        });

        // Non-numeric input is rejected.
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "abc", window, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(state.value(), "12.5");
            });
        });

        // A bare leading dot is kept as-is (normalized from the ideographic
        // full stop), not completed to "0.", so it stays editable.
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                let range = state.range_to_utf16(&(0..state.text.len()));
                state.replace_text_in_range(Some(range), "。", window, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(state.value(), ".");
                let cursor: Range<usize> = state.selected_range.into();
                assert_eq!(cursor, 1..1);
            });
        });
    }

    #[gpui::test]
    fn test_number_input_normalization_with_separator(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| {
            state.mask_pattern(MaskPattern::Number {
                separator: Some(','),
                fraction: Some(2),
            })
        });
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "1234", window, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert_eq!(state.value(), "1,234");
                assert_eq!(state.unmask_value(), "1234");
            });
        });
    }

    #[gpui::test]
    fn test_number_input_clamp_on_blur(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| {
            state
                .mask_pattern(MaskPattern::Number {
                    separator: None,
                    fraction: None,
                })
                .min(10.)
                .max(100.)
        });
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        // Out-of-range values are allowed while typing, and clamped on blur.
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "1000", window, cx);
                assert_eq!(state.value(), "1000");
                state.clamp_number_value(window, cx);
                assert_eq!(state.value(), "100");

                let range = state.range_to_utf16(&(0..state.text.len()));
                state.replace_text_in_range(Some(range), "1", window, cx);
                assert_eq!(state.value(), "1");
                state.clamp_number_value(window, cx);
                assert_eq!(state.value(), "10");
            });
        });
    }

    #[gpui::test]
    fn test_number_input_undo_with_mask(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| {
            state.mask_pattern(MaskPattern::Number {
                separator: Some(','),
                fraction: None,
            })
        });
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        // When the mask changes the text (regrouping separators), a
        // whole-document change is recorded, so undo/redo can restore it.
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "1234", window, cx);
                assert_eq!(state.value(), "1,234");
                state.replace_text_in_range(None, "5", window, cx);
                assert_eq!(state.value(), "12,345");

                // Each whole-document mask rewrite is an atomic undo step.
                // Before the whole-document history fix, undo produced a
                // corrupted value like "1,2344".
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "1,234");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "");
                state.redo(&Redo, window, cx);
                assert_eq!(state.value(), "1,234");
                state.redo(&Redo, window, cx);
                assert_eq!(state.value(), "12,345");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_coalesces_adjacent_typing_transactions(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "a", window, cx);
                state.replace_text_in_range(None, "b", window, cx);
                assert_eq!(state.value(), "ab");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_cursor_movement_splits_typing(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "a", window, cx);
                state.replace_text_in_range(None, "b", window, cx);
                state.left(&MoveLeft, window, cx);
                state.replace_text_in_range(None, "x", window, cx);
                assert_eq!(state.value(), "axb");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "ab");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_splits_backward_and_forward_delete(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("abcd", window, cx);
                state.set_selected_range(2..2, cx);
                state.backspace(&Backspace, window, cx);
                state.delete(&Delete, window, cx);
                assert_eq!(state.value(), "ad");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "acd");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "abcd");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_coalesces_directional_character_deletes(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("abcd", window, cx);
                state.backspace(&Backspace, window, cx);
                state.backspace(&Backspace, window, cx);
                assert_eq!(state.value(), "ab");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "abcd");
                assert_eq!(state.selected_range(), 4..4);

                state.set_value("abcd", window, cx);
                state.set_selected_range(1..1, cx);
                state.delete(&Delete, window, cx);
                state.delete(&Delete, window, cx);
                assert_eq!(state.value(), "ad");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "abcd");
                assert_eq!(state.selected_range(), 1..1);
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_atomic_paste_isolated_from_typing(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string("P".to_string()));
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "a", window, cx);
                state.paste(&Paste, window, cx);
                state.replace_text_in_range(None, "b", window, cx);
                assert_eq!(state.value(), "aPb");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "aP");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "a");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_programmatic_insert_is_atomic(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "a", window, cx);
                state.insert("P", window, cx);
                state.replace_text_in_range(None, "b", window, cx);
                assert_eq!(state.value(), "aPb");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "aP");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "a");
            });
        });
    }

    /// `replace_range` is one step of undo apart from the typing before it,
    /// and undoing it puts the caret back where it was rather than selecting
    /// the range it replaced.
    #[gpui::test]
    fn test_replace_range_is_one_step_that_puts_the_caret_back(cx: &mut TestAppContext) {
        let input_view = InputView::build_textarea(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "x = Has", window, cx);
                assert_eq!(state.selected_range(), 7..7);
                state.replace_range(0..7, "use m;\nx = HashMap", window, cx);
                assert_eq!(state.value(), "use m;\nx = HashMap");
                assert_eq!(state.selected_range(), 18..18, "after what was put in");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "x = Has");
                assert_eq!(state.selected_range(), 7..7, "the caret, not the range");
                state.redo(&Redo, window, cx);
                assert_eq!(state.value(), "use m;\nx = HashMap");
                state.undo(&Undo, window, cx);
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_selection_round_trip_splits_typing(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "a", window, cx);
                state.replace_text_in_range(None, "b", window, cx);
                state.select_all(window, cx);
                state.unselect(window, cx);
                state.replace_text_in_range(None, "c", window, cx);

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "ab");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_enter_is_atomic(cx: &mut TestAppContext) {
        let input_view = InputView::build_textarea(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "a", window, cx);
                state.enter(
                    &Enter {
                        secondary: false,
                        shift: false,
                    },
                    window,
                    cx,
                );
                state.replace_text_in_range(None, "b", window, cx);

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "a\n");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "a");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_single_line_return_commits_the_typing_session(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                for part in ["a", "b", "c"] {
                    state.replace_text_in_range(None, part, window, cx);
                }
                state.enter(
                    &Enter {
                        secondary: false,
                        shift: false,
                    },
                    window,
                    cx,
                );
                for part in ["d", "e", "f"] {
                    state.replace_text_in_range(None, part, window, cx);
                }
                assert_eq!(state.value(), "abcdef");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "abc");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_submit_on_enter_commits_the_textarea_session(cx: &mut TestAppContext) {
        let input_view = InputView::build_textarea(cx, |state| state.submit_on_enter(true));
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "before submit", window, cx);
                state.enter(
                    &Enter {
                        secondary: false,
                        shift: false,
                    },
                    window,
                    cx,
                );
                state.replace_text_in_range(None, " after submit", window, cx);
                assert_eq!(state.value(), "before submit after submit");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "before submit");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_blur_commits_the_typing_session(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "before blur", window, cx);
                state.on_blur(window, cx);
                state.on_focus(window, cx);
                state.replace_text_in_range(None, " after focus", window, cx);
                assert_eq!(state.value(), "before blur after focus");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "before blur");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_keeps_rapid_lines_in_distinct_transactions(cx: &mut TestAppContext) {
        let input_view = InputView::build_textarea(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                let enter = Enter {
                    secondary: false,
                    shift: false,
                };
                state.replace_text_in_range(None, "a", window, cx);
                state.enter(&enter, window, cx);
                state.replace_text_in_range(None, "b", window, cx);
                state.enter(&enter, window, cx);
                state.replace_text_in_range(None, "c", window, cx);
                assert_eq!(state.value(), "a\nb\nc");

                for expected in ["a\nb\n", "a\nb", "a\n", "a", ""] {
                    state.undo(&Undo, window, cx);
                    assert_eq!(state.value(), expected);
                }
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_coalesces_long_unicode_typing_without_a_timer(cx: &mut TestAppContext) {
        let input_view = InputView::build_textarea(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;
        let parts = [
            "The ",
            "quick ",
            "brown fox, ",
            "你好，世界 ",
            "🦀 jumps over 13 lazy dogs.",
        ];
        let expected = parts.concat();

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                for part in parts {
                    state.replace_text_in_range(None, part, window, cx);
                }
                assert_eq!(state.value(), expected);

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "");
                state.redo(&Redo, window, cx);
                assert_eq!(state.value(), expected);
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_long_multiline_sequence_has_structural_boundaries(
        cx: &mut TestAppContext,
    ) {
        let input_view = InputView::build_textarea(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;
        let enter = Enter {
            secondary: false,
            shift: false,
        };

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                for (index, line) in [
                    "first line with punctuation!",
                    "第二行包含 Unicode 🦀",
                    "third line has several words",
                ]
                .into_iter()
                .enumerate()
                {
                    for chunk in line.split_inclusive(' ') {
                        state.replace_text_in_range(None, chunk, window, cx);
                    }
                    if index < 2 {
                        state.enter(&enter, window, cx);
                    }
                }

                assert_eq!(
                    state.value(),
                    "first line with punctuation!\n第二行包含 Unicode 🦀\nthird line has several words"
                );
                state.undo(&Undo, window, cx);
                assert_eq!(
                    state.value(),
                    "first line with punctuation!\n第二行包含 Unicode 🦀\n"
                );
                state.undo(&Undo, window, cx);
                assert_eq!(
                    state.value(),
                    "first line with punctuation!\n第二行包含 Unicode 🦀"
                );
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "first line with punctuation!\n");
            });
        });
    }

    #[gpui::test]
    fn test_masked_input_keeps_its_value_out_of_the_clipboard(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("hunter2", window, cx);
                state.set_masked(true, window, cx);
                state.select_all(window, cx);
                cx.write_to_clipboard(ClipboardItem::new_string("sentinel".into()));

                state.copy(&Copy, window, cx);
                assert_eq!(
                    cx.read_from_clipboard().and_then(|item| item.text()),
                    Some("sentinel".to_string())
                );

                // Cut neither copies nor deletes.
                state.cut(&Cut, window, cx);
                assert_eq!(state.value(), "hunter2");
                assert_eq!(
                    cx.read_from_clipboard().and_then(|item| item.text()),
                    Some("sentinel".to_string())
                );

                // Revealing the value restores both.
                state.set_masked(false, window, cx);
                state.copy(&Copy, window, cx);
                assert_eq!(
                    cx.read_from_clipboard().and_then(|item| item.text()),
                    Some("hunter2".to_string())
                );
            });
        });
    }

    #[gpui::test]
    fn test_masked_input_collapses_word_boundaries(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("aaa bbb ccc", window, cx);
                state.set_masked(true, window, cx);
                state.set_selected_range(7..7, cx);

                // The mask hides word boundaries, so a word delete takes
                // everything before the caret and leaves the rest.
                state.delete_previous_word(&DeleteToPreviousWordStart, window, cx);
                assert_eq!(state.value(), " ccc");
                assert_eq!(state.selected_range(), 0..0);

                state.delete_next_word(&DeleteToNextWordEnd, window, cx);
                assert_eq!(state.value(), "");

                // A double click takes the whole value, not one word.
                state.set_value("aaa bbb ccc", window, cx);
                state.select_word(9, window, cx);
                assert_eq!(state.selected_range(), 0..11);

                // Unmasked, the same delete only takes one word.
                state.set_masked(false, window, cx);
                state.set_value("aaa bbb ccc", window, cx);
                state.set_selected_range(11..11, cx);
                state.delete_previous_word(&DeleteToPreviousWordStart, window, cx);
                assert_eq!(state.value(), "aaa bbb ");

                state.set_value("aaa bbb ccc", window, cx);
                state.select_word(9, window, cx);
                assert_eq!(state.selected_range(), 8..11);
            });
        });
    }

    #[gpui::test]
    fn test_masked_input_disables_the_copy_context_menu_items(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("hunter2", window, cx);
                state.select_all(window, cx);
                assert!(state.context_menu_capabilities().is_copyable());

                state.set_masked(true, window, cx);
                let capabilities = state.context_menu_capabilities();
                assert!(capabilities.is_masked());
                assert!(capabilities.has_selection());
                assert!(!capabilities.is_copyable());
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_cut_and_repeated_pastes_are_distinct_transactions(
        cx: &mut TestAppContext,
    ) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "alpha beta gamma", window, cx);
                state.set_selected_range(6..10, cx);
                state.cut(&Cut, window, cx);
                assert_eq!(state.value(), "alpha  gamma");

                state.paste(&Paste, window, cx);
                state.paste(&Paste, window, cx);
                assert_eq!(state.value(), "alpha betabeta gamma");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "alpha beta gamma");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "alpha  gamma");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "alpha beta gamma");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_word_and_line_deletes_do_not_coalesce(cx: &mut TestAppContext) {
        let input_view = InputView::build_textarea(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("one two three\nfour five", window, cx);
                state.set_selected_range(13..13, cx);
                state.delete_previous_word(&DeleteToPreviousWordStart, window, cx);
                assert_eq!(state.value(), "one two \nfour five");
                state.delete_to_end_of_line(&DeleteToEndOfLine, window, cx);
                assert_eq!(state.value(), "one two four five");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "one two \nfour five");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "one two three\nfour five");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_multiline_replacement_is_one_atomic_transaction(cx: &mut TestAppContext) {
        let input_view = InputView::build_textarea(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "before", window, cx);
                state.set_selected_range(0..6, cx);
                state.replace_text_in_range(None, "line one\nline two\n第三行", window, cx);
                assert_eq!(state.value(), "line one\nline two\n第三行");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "before");
                assert_eq!(state.selected_range(), 0..6);
                state.redo(&Redo, window, cx);
                assert_eq!(state.value(), "line one\nline two\n第三行");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_composition_isolated_from_long_typing(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "prefix ", window, cx);
                state.replace_and_mark_text_in_range(None, "n", None, window, cx);
                state.replace_and_mark_text_in_range(None, "ni", None, window, cx);
                state.replace_and_mark_text_in_range(None, "你", None, window, cx);
                state.unmark_text(window, cx);
                state.replace_text_in_range(None, " suffix", window, cx);
                assert_eq!(state.value(), "prefix 你 suffix");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "prefix 你");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "prefix ");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "");
            });
        });
    }

    /// Typing over a selection is one step of undo with what is typed after
    /// it, as VS Code and IntelliJ have it, and apart from what was typed
    /// before the selection was made. The character that replaced the
    /// selection was a step of its own: typing `"a"` over a snippet's
    /// placeholder or a word double-clicked, the first undo left the one
    /// quote where the selection had been (elundqvist/kvist#190). The undo
    /// selects what was replaced again, and a redo puts the typing back.
    #[gpui::test]
    fn test_undo_manager_typing_over_a_selection_is_one_step_with_what_follows(
        cx: &mut TestAppContext,
    ) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "let total = 0;", window, cx);
                state.set_selected_range(4..9, cx);
                for typed in ["\"", "a", "\""] {
                    state.replace_text_in_range(None, typed, window, cx);
                }
                assert_eq!(state.value(), "let \"a\" = 0;");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "let total = 0;");
                assert_eq!(state.selected_range(), 4..9, "what was replaced, selected");
                state.redo(&Redo, window, cx);
                assert_eq!(state.value(), "let \"a\" = 0;");
                state.undo(&Undo, window, cx);
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "");

                // typed over a selection just after other typing: two steps
                state.replace_text_in_range(None, "abc", window, cx);
                state.set_selected_range(1..2, cx);
                state.replace_text_in_range(None, "X", window, cx);
                state.replace_text_in_range(None, "z", window, cx);
                assert_eq!(state.value(), "aXzc");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "abc");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "");
            });
        });
    }

    #[gpui::test]
    fn test_number_input_leading_dot_editable(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| {
            state.mask_pattern(MaskPattern::Number {
                separator: None,
                fraction: None,
            })
        });
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "1.2", window, cx);

                // Delete the integer part "1": the value keeps the leading dot
                // (".2"), not completed to "0.2", so the digits before the dot
                // stay editable.
                let range = state.range_to_utf16(&(0..1));
                state.replace_text_in_range(Some(range), "", window, cx);
                assert_eq!(state.value(), ".2");
                let cursor: Range<usize> = state.selected_range.into();
                assert_eq!(cursor, 0..0);

                // The user can type a new integer part.
                state.replace_text_in_range(Some(0..0), "3", window, cx);
                assert_eq!(state.value(), "3.2");
            });
        });
    }

    #[gpui::test]
    fn test_number_input_escape_invalid_text(cx: &mut TestAppContext) {
        // A pre-existing invalid text (e.g. a `default_value` that does not
        // conform) must not trap the user, the edit is allowed to fix it.
        let input_view = InputView::build(cx, |state| {
            state
                .mask_pattern(MaskPattern::Number {
                    separator: None,
                    fraction: None,
                })
                .default_value("1,234")
        });
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                // Delete the last char, the pending text "1,23" is still
                // invalid, but the edit is allowed since the old text was
                // already invalid.
                let range = state.range_to_utf16(&(4..5));
                state.replace_text_in_range(Some(range), "", window, cx);
                assert_eq!(state.value(), "1,23");

                // Once the text becomes valid, the validation works as usual.
                let range = state.range_to_utf16(&(1..2));
                state.replace_text_in_range(Some(range), "", window, cx);
                assert_eq!(state.value(), "123");
                state.replace_text_in_range(None, "a", window, cx);
                assert_eq!(state.value(), "123");
            });
        });
    }

    /// After `set_value` on a single-line input the caret sits at the end (like
    /// HTML `<input>`), yet the view is scrolled back to the start so a long
    /// value shows its beginning instead of its tail.
    #[gpui::test]
    fn test_set_value_single_line_caret_at_end_view_at_start(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        // Long enough to overflow any reasonable single-line input width.
        let value = format!("https://example.com/v1/users?{}", "x=1&".repeat(120));
        let len = value.len();

        // Right after `set_value`, before the next paint consumes the deferred
        // offset: caret is at the end, and the view is forced back to the start.
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value(value.clone(), window, cx);

                assert_eq!(
                    state.selected_range,
                    Selection::new(len, len),
                    "single-line caret should be at the end after set_value"
                );
                assert_eq!(
                    state.deferred_scroll_offset,
                    Some(point(px(0.), px(0.))),
                    "the view should be forced back to the start"
                );
            });
        });

        // After a paint, the steady-state view stays at the start (x == 0) even
        // though the caret is at the far end.
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert!(
                    state.scroll_size.width > state.input_bounds.size.width,
                    "value must overflow the input width or this test is vacuous"
                );
                assert_eq!(
                    state.scroll_handle.offset().x,
                    px(0.),
                    "long value should display from its start, not its tail"
                );
            });
        });
    }

    /// `replace_all` on a single-line input replaces the text, puts the
    /// caret at the end, and — like `set_value` — snaps the view back to the
    /// start so a long value shows its beginning instead of its tail.
    #[gpui::test]
    fn test_replace_all_single_line(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        // Long enough to overflow any reasonable single-line input width.
        let value = format!("https://example.com/v1/users?{}", "x=1&".repeat(120));
        let len = value.len();

        // Right after `replace_all`, before the next paint consumes the
        // deferred offset: caret is at the end, and the view is forced back
        // to the start.
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("hello", window, cx);
                state.replace_all(value.clone(), window, cx);
                assert_eq!(state.value(), value);
                assert_eq!(
                    state.selected_range,
                    Selection::new(len, len),
                    "single-line caret should be at the end after replace_all"
                );
                assert_eq!(
                    state.scroll_handle.offset(),
                    point(px(0.), px(0.)),
                    "the scroll offset should be reset to the start"
                );
                assert_eq!(
                    state.deferred_scroll_offset,
                    Some(point(px(0.), px(0.))),
                    "single-line should set a deferred scroll offset to keep the start visible"
                );
            });
        });

        // After a paint, the steady-state view stays at the start (x == 0)
        // even though the caret is at the far end.
        cx.run_until_parked();
        cx.update(|_, cx| {
            input.read_with(cx, |state, _| {
                assert!(
                    state.scroll_size.width > state.input_bounds.size.width,
                    "value must overflow the input width or this test is vacuous"
                );
                assert_eq!(
                    state.scroll_handle.offset().x,
                    px(0.),
                    "long value should display from its start, not its tail"
                );
            });
        });
    }

    #[gpui::test]
    fn test_single_line_removes_newlines(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state.default_value("default\nvalue"));
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                assert_eq!(state.value(), "defaultvalue");

                state.set_value("first\nsecond\r\nthird\rfourth", window, cx);
                assert_eq!(state.value(), "firstsecondthirdfourth");

                state.set_value("", window, cx);
                state.insert("a\nb", window, cx);
                assert_eq!(state.value(), "ab");
            });

            cx.write_to_clipboard(ClipboardItem::new_string("a\r\nb\nc\rd".to_string()));
            input.update(cx, |state, cx| {
                state.set_value("", window, cx);
                state.paste(&Paste, window, cx);
                assert_eq!(state.value(), "abcd");
            });
        });

        cx.run_until_parked();
    }

    /// In a CRLF file the caret never stands between the `\r` and the `\n`:
    /// End and Shift+End stop before the `\r`, what is typed there goes
    /// before it, Left and Right step over the pair as one character, and
    /// Backspace and Delete take both bytes or neither.
    #[gpui::test]
    fn the_caret_never_splits_a_crlf(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let input_view = InputView::new(cx);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("ab\r\ncd\r\n", window, cx);

                state.set_selected_range(1..1, cx);
                state.end(&MoveEnd, window, cx);
                assert_eq!(state.cursor(), 2, "End stops before the \\r");
                state.insert("x", window, cx);
                assert_eq!(state.value(), "abx\r\ncd\r\n");
                state.enter(&Enter { secondary: false, shift: false }, window, cx);
                assert_eq!(state.value(), "abx\n\r\ncd\r\n", "Enter opens the line before the pair");

                state.set_selected_range(0..0, cx);
                state.select_to_end_of_line(&SelectToEndOfLine, window, cx);
                assert_eq!(state.selected_range, Selection::new(0, 3), "Shift+End stops before the \\r");

                state.set_value("ab\r\ncd\r\n", window, cx);
                state.set_selected_range(2..2, cx);
                state.right(&MoveRight, window, cx);
                assert_eq!(state.cursor(), 4, "Right steps over the pair");
                state.left(&MoveLeft, window, cx);
                assert_eq!(state.cursor(), 2, "Left steps back over it");
                state.set_selected_range(1..1, cx);
                state.right(&MoveRight, window, cx);
                assert_eq!(state.cursor(), 2, "Right from inside the line stops before the \\r");

                state.set_selected_range(4..4, cx);
                state.backspace(&Backspace, window, cx);
                assert_eq!(state.value(), "abcd\r\n", "Backspace at a line start joins the lines");
                state.set_value("ab\r\ncd\r\n", window, cx);
                state.set_selected_range(2..2, cx);
                state.delete(&Delete, window, cx);
                assert_eq!(state.value(), "abcd\r\n", "Delete at a line end joins the lines");

                state.set_value("ab\r\ncd\r\n", window, cx);
                state.set_selected_range(2..2, cx);
                state.delete_to_end_of_line(&DeleteToEndOfLine, window, cx);
                assert_eq!(state.value(), "abcd\r\n");
                state.set_value("ab\r\ncd\r\n", window, cx);
                state.set_selected_range(4..4, cx);
                state.delete_to_beginning_of_line(&DeleteToBeginningOfLine, window, cx);
                assert_eq!(state.value(), "abcd\r\n");

                // a double-click on the line's end takes the line's last word,
                // never the pair nor half of it (elundqvist/kvist#416); a
                // triple-click stops before the \r
                state.set_value("ab\r\ncd\r\n", window, cx);
                state.select_word(2, window, cx);
                assert_eq!(state.selected_range, Selection::new(0, 2), "a double-click on the \\r");
                state.select_word(3, window, cx);
                assert_eq!(state.selected_range, Selection::new(0, 2), "a double-click on the \\n");
                state.right(&MoveRight, window, cx);
                assert_eq!(state.cursor(), 2);
                state.select_line(1, window, cx);
                assert_eq!(state.selected_range, Selection::new(0, 2), "the triple-click stops before the \\r");

                // a character of more than one byte before the pair
                state.set_value("中\r\ncd\r\n", window, cx);
                state.set_selected_range(0..0, cx);
                state.right(&MoveRight, window, cx);
                assert_eq!(state.cursor(), 3);
                state.right(&MoveRight, window, cx);
                assert_eq!(state.cursor(), 5, "Right steps over the pair after a wide character");
                state.left(&MoveLeft, window, cx);
                assert_eq!(state.cursor(), 3);
                state.delete(&Delete, window, cx);
                assert_eq!(state.value(), "中cd\r\n");
                state.set_selected_range(5..5, cx);
                state.backspace(&Backspace, window, cx);
                state.backspace(&Backspace, window, cx);
                assert_eq!(state.value(), "中\r\n", "Backspace takes the pair, then the wide character");

                // a \r that no \n follows is text, and the caret goes round it as it does any character
                state.set_value("a\rb\n", window, cx);
                state.set_selected_range(0..0, cx);
                state.end(&MoveEnd, window, cx);
                assert_eq!(state.cursor(), 3);
                state.set_selected_range(1..1, cx);
                state.right(&MoveRight, window, cx);
                assert_eq!(state.cursor(), 2);
            });
        });
    }

    /// The caret goes a grapheme at a time, what is drawn as one character
    /// (elundqvist/kvist#456): a family of four is seven characters, a flag
    /// two and a thumb with its colour two, and Left, Right, Shift+Left and
    /// Delete each crossed one character of them, so a family took seven
    /// Backspaces and one Backspace split a flag. Backspace takes the whole
    /// of such an emoji and of any other grapheme its last character, an
    /// accent typed after its letter or a vowel sign after its consonant, as
    /// Kvist's own line edit does (elundqvist/kvist#451).
    #[gpui::test]
    fn the_caret_goes_by_grapheme(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let input_view = InputView::new(cx);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        const FAMILY: &str = "👨\u{200D}👩\u{200D}👧\u{200D}👦";
        const FLAG: &str = "🇳🇴";
        const THUMB: &str = "👍🏽";
        const KEYCAP: &str = "1\u{FE0F}\u{20E3}";
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                let text = format!("a{FAMILY}{FLAG}{THUMB}{KEYCAP}b");
                let ends = [
                    0,
                    1,
                    1 + FAMILY.len(),
                    1 + FAMILY.len() + FLAG.len(),
                    text.len() - 1 - KEYCAP.len(),
                    text.len() - 1,
                    text.len(),
                ];
                state.set_value(&text, window, cx);
                state.set_selected_range(0..0, cx);
                for &end in &ends[1..] {
                    state.right(&MoveRight, window, cx);
                    assert_eq!(state.cursor(), end, "Right crosses one grapheme");
                }
                for &end in ends[..ends.len() - 1].iter().rev() {
                    state.left(&MoveLeft, window, cx);
                    assert_eq!(state.cursor(), end, "Left crosses one grapheme");
                }
                state.set_selected_range(ends[3]..ends[3], cx);
                state.select_left(&SelectLeft, window, cx);
                assert_eq!(state.selected_range, Selection::new(ends[2], ends[3]), "Shift+Left takes the flag");
                state.set_selected_range(ends[3]..ends[3], cx);
                state.select_right(&SelectRight, window, cx);
                assert_eq!(state.selected_range, Selection::new(ends[3], ends[4]), "Shift+Right the thumb");

                state.set_selected_range(1..1, cx);
                state.delete(&Delete, window, cx);
                assert_eq!(state.value(), format!("a{FLAG}{THUMB}{KEYCAP}b").as_str(), "Delete takes the family");
                state.set_value(&text, window, cx);
                for (emoji, what) in [
                    (KEYCAP, "the keycap"),
                    (THUMB, "the thumb and its colour"),
                    (FLAG, "the flag"),
                    (FAMILY, "the family"),
                ] {
                    let at = state.value().len() - 1;
                    state.set_selected_range(at..at, cx);
                    state.backspace(&Backspace, window, cx);
                    assert_eq!(state.cursor(), at - emoji.len(), "one Backspace takes {what}");
                }
                assert_eq!(state.value(), "ab");

                // of a grapheme that is no emoji, the last character alone
                let text = "e\u{301}x\u{915}\u{93F}";
                state.set_value(text, window, cx);
                state.set_selected_range(0..0, cx);
                state.right(&MoveRight, window, cx);
                assert_eq!(state.cursor(), "e\u{301}".len(), "Right crosses a letter and its accent");
                state.set_selected_range(text.len()..text.len(), cx);
                state.left(&MoveLeft, window, cx);
                assert_eq!(state.cursor(), "e\u{301}x".len(), "Left crosses a syllable");
                state.set_selected_range(text.len()..text.len(), cx);
                state.backspace(&Backspace, window, cx);
                assert_eq!(state.value(), "e\u{301}x\u{915}", "Backspace takes the vowel sign, not the syllable");
                state.backspace(&Backspace, window, cx);
                state.backspace(&Backspace, window, cx);
                state.backspace(&Backspace, window, cx);
                assert_eq!(state.value(), "e", "then the consonant, the x and the accent");
                state.set_value(text, window, cx);
                state.set_selected_range(0..0, cx);
                state.delete(&Delete, window, cx);
                assert_eq!(state.value(), "x\u{915}\u{93F}", "Delete takes the letter with its accent");
            });
        });
    }

    /// A masked input, a password's, goes a character at a time, as it
    /// draws a bullet a character: a step a grapheme crossed seven bullets
    /// at once for a family, showing the shape of what it hides, where
    /// GTK's password entry steps a character at a time not to.
    #[gpui::test]
    fn a_masked_input_goes_by_character(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state.masked(true));
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        const FAMILY: &str = "👨\u{200D}👩\u{200D}👧\u{200D}👦";
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                let text = format!("a{FAMILY}🇳🇴");
                state.set_value(&text, window, cx);
                state.set_selected_range(text.len()..text.len(), cx);
                state.left(&MoveLeft, window, cx);
                assert_eq!(state.cursor(), text.len() - '🇴'.len_utf8(), "Left crosses one bullet");
                state.set_selected_range(1..1, cx);
                state.right(&MoveRight, window, cx);
                assert_eq!(state.cursor(), 1 + '👨'.len_utf8(), "Right too");
                state.set_selected_range(text.len()..text.len(), cx);
                state.backspace(&Backspace, window, cx);
                assert_eq!(state.value(), format!("a{FAMILY}🇳").as_str(), "Backspace takes one");
                state.set_selected_range(1..1, cx);
                state.delete(&Delete, window, cx);
                assert_eq!(
                    state.value(),
                    format!("a{}🇳", &FAMILY['👨'.len_utf8()..]).as_str(),
                    "Delete one"
                );
            });
        });
    }

    /// Shift+Up and Shift+Down go by column, as Up and Down do: from the
    /// middle of a line to the same column of the next, from the end of a
    /// line to the end of the next, and no further. Shift+Down went to the
    /// next line's start and one character on, whatever the column was, so
    /// what was typed over the selection ate that character; in a CRLF file
    /// the one character was the `\r`. Up and Down right after typing keep
    /// the column too: the edit drops the preferred column, since the layout
    /// is older than the text, and the move takes it from the caret's place.
    #[gpui::test]
    fn a_vertical_move_keeps_its_column(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let input_view = InputView::new(cx);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("/target\n*.swp\nabcdefgh\nij", window, cx);
            });
        });
        cx.run_until_parked();

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                // from the end of "/target", to the end of the shorter "*.swp"
                state.set_selected_range(7..7, cx);
                state.select_down(&SelectDown, window, cx);
                assert_eq!(state.selected_range, Selection::new(7, 13), "Shift+Down from a line end stops at the next line's end");
                // from column 2 to column 2, twice
                state.set_selected_range(2..2, cx);
                state.select_down(&SelectDown, window, cx);
                assert_eq!(state.selected_range, Selection::new(2, 10));
                state.select_down(&SelectDown, window, cx);
                assert_eq!(state.selected_range, Selection::new(2, 16), "and keeps the column");
                // up from column 5 of "abcdefgh", over the shorter line, to column 5 of the first
                state.set_selected_range(19..19, cx);
                state.select_up(&SelectUp, window, cx);
                assert_eq!(state.selected_range, Selection::new(13, 19), "Shift+Up stops at a shorter line's end");
                state.select_up(&SelectUp, window, cx);
                assert_eq!(state.selected_range, Selection::new(5, 19), "and keeps the column");
                // on the first line up reaches the start of the text, on the last line down its end
                state.select_up(&SelectUp, window, cx);
                assert_eq!(state.selected_range, Selection::new(0, 19));
                state.set_selected_range(24..24, cx);
                state.select_down(&SelectDown, window, cx);
                assert_eq!(state.selected_range, Selection::new(24, 25));

                // typing at the end of "/target" drops the preferred column
                state.set_selected_range(7..7, cx);
                state.insert("b", window, cx);
                assert_eq!(state.value(), "/targetb\n*.swp\nabcdefgh\nij");
            });
        });
        // the layout catches up with the text before the next key
        cx.run_until_parked();
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.down(&MoveDown, window, cx);
                assert_eq!(state.cursor(), 14, "Down after typing at a line end lands at the next line's end, not its start");
                state.set_selected_range(2..2, cx);
                state.insert("x", window, cx);
                assert_eq!(state.cursor(), 3);
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.down(&MoveDown, window, cx);
                assert_eq!(state.cursor(), 13, "Down after typing mid-line keeps the caret's column");
            });
        });
    }

    /// `replace_all` on a multi-line (non-code-editor) input clears the
    /// selection to `0..0` and resets the scroll offset, but does not set a
    /// deferred scroll offset (single-line only).
    #[gpui::test]
    fn test_replace_all_multi_line(cx: &mut TestAppContext) {
        let input_view = InputView::build_textarea(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("foo\nbar", window, cx);
                state.replace_all("baz\nqux", window, cx);
                assert_eq!(state.value(), "baz\nqux");
                assert_eq!(
                    state.selected_range,
                    Selection::new(0, 0),
                    "multi-line selection should be cleared after replace_all"
                );
                assert_eq!(
                    state.scroll_handle.offset(),
                    point(px(0.), px(0.)),
                    "the scroll offset should be reset to the start"
                );
                assert!(
                    state.deferred_scroll_offset.is_none(),
                    "multi-line should not set a deferred scroll offset"
                );
            });
        });
    }

    /// Unlike `set_value`, `replace_all` records the change so the user can
    /// undo it back to the previous text and redo to the new text.
    #[gpui::test]
    fn test_replace_all_preserves_undo_history(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                // Seed with a value and clear history so the baseline is clean.
                state.set_value("first", window, cx);
                assert!(
                    !state.undo_manager.has_undos(),
                    "history should be empty after set_value"
                );

                // replace_all records a single undoable change.
                state.replace_all("second", window, cx);
                assert_eq!(state.value(), "second");
                assert!(
                    state.undo_manager.has_undos(),
                    "replace_all should record an undo step"
                );

                // Undo restores the previous text.
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "first");

                // Redo reapplies the replacement.
                state.redo(&Redo, window, cx);
                assert_eq!(state.value(), "second");
            });
        });
    }

    /// `replace_all` on a code editor marks a pending update and resets LSP
    /// state, so diagnostics/completions refresh against the new text.
    #[gpui::test]
    fn test_replace_all_code_editor(cx: &mut TestAppContext) {
        let input_view = InputView::new(cx);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                // Plant a pending-update flag and some LSP state to verify reset.
                state.set_value("select 1", window, cx);
                state._pending_update = false;

                state.replace_all("select 2", window, cx);
                assert_eq!(state.value(), "select 2");
                assert!(
                    state._pending_update,
                    "replace_all on a code editor should request a pending update"
                );
            });
        });
    }

    /// A highlighter swapped by `set_highlighter` is made, and parses the
    /// text, at the next frame, with no edit after the swap.
    #[gpui::test]
    fn a_swapped_highlighter_is_made_at_the_next_frame(cx: &mut TestAppContext) {
        use crate::input::{FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter};

        /// Knows its language and counts the parses it was asked for.
        struct Named(SharedString, Rc<Cell<usize>>);

        impl InputHighlighter for Named {
            fn language(&self) -> SharedString {
                self.0.clone()
            }

            fn update(
                &mut self,
                _: Option<InputEdit>,
                _: &Rope,
                _: bool,
                _: &mut Window,
                _: &mut Context<crate::input::EditorState>,
            ) {
                self.1.set(self.1.get() + 1);
            }

            fn styles(
                &self,
                range: &Range<usize>,
                _: &dyn HighlightStyleResolver,
            ) -> Vec<(Range<usize>, gpui::HighlightStyle)> {
                vec![(range.clone(), gpui::HighlightStyle::default())]
            }

            fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
                Vec::new()
            }
        }

        let input_view = InputView::new(cx);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;
        let parses = Rc::new(Cell::new(0));

        let factory_parses = parses.clone();
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_highlighter_factory(
                    Rc::new(move |language: &str| {
                        Some(Box::new(Named(
                            SharedString::from(language.to_string()),
                            factory_parses.clone(),
                        )) as Box<dyn InputHighlighter>)
                    }),
                    cx,
                );
                state.set_value("-- a note\nselect 1", window, cx);
            });
        });
        cx.run_until_parked();
        let language = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                input.read_with(cx, |state, _| {
                    let highlighter = state.mode.highlighter()?.borrow();
                    highlighter.as_ref().map(|h| h.language())
                })
            })
        };
        assert_eq!(language(&mut cx), Some("sql".into()));
        let before = parses.get();

        // a swap with no edit after it, as when an edit elsewhere turns
        // the text into another language
        cx.update(|_, cx| input.update(cx, |state, cx| state.set_highlighter("json", cx)));
        cx.run_until_parked();
        assert_eq!(
            language(&mut cx),
            Some("json".into()),
            "the frame after the swap has the new highlighter"
        );
        assert!(parses.get() > before, "and it has parsed the text");
    }

    #[gpui::test]
    fn test_set_selected_range(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state.default_value("hello world"));
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|_, cx| {
            input.update(cx, |s, cx| {
                s.set_selected_range(0..5, cx);
                assert_eq!(s.selected_range(), 0..5);
                assert_eq!(s.selected_text().to_string(), "hello");

                s.set_selected_range(6..11, cx);
                assert_eq!(s.selected_text().to_string(), "world");

                // clamped + collapsed
                s.set_selected_range(100..100, cx);
                assert_eq!(s.selected_range(), 11..11);
            });
        });
    }

    #[gpui::test]
    fn test_set_selected_range_clips_to_utf8_boundaries(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state.default_value("éx"));
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_range(0..1, cx);
                assert_eq!(state.selected_range(), 0..2);
                state.copy(&Copy, window, cx);

                state.set_selected_range(1..1, cx);
                assert_eq!(state.selected_range(), 0..0);
            });
        });
    }

    #[gpui::test]
    fn test_ime_selection_is_relative_to_replacement_start(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state.default_value("你好 "));
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_range(7..7, cx);
                state.replace_and_mark_text_in_range(None, "s", Some(1..1), window, cx);
                state.replace_and_mark_text_in_range(None, "sh", Some(2..2), window, cx);

                assert_eq!(state.value(), "你好 sh");
                assert_eq!(state.selected_range(), 9..9);
                assert_eq!(state.ime_marked_range, Some((7..9).into()));
            });
        });
    }

    /// What is being composed is the marked range until it is committed, given up or
    /// ended: an editor that saves as it is typed in waits for it, where Kvist's wrote
    /// a dead key's ´ to disk while it composed (elundqvist/kvist#475).
    #[gpui::test]
    fn the_marked_range_is_what_is_being_composed(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state.default_value("ab"));
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_range(2..2, cx);
                assert_eq!(state.marked_range(), None);
                // a dead key's accent, and the letter it makes
                state.replace_and_mark_text_in_range(None, "´", None, window, cx);
                assert_eq!(state.value(), "ab´");
                assert_eq!(state.marked_range(), Some(2..4));
                state.replace_text_in_range(None, "é", window, cx);
                assert_eq!(state.value(), "abé");
                assert_eq!(state.marked_range(), None);
                // one Escape gives up
                state.replace_and_mark_text_in_range(None, "´", None, window, cx);
                assert_eq!(state.marked_range(), Some(4..6));
                state.replace_and_mark_text_in_range(None, "", None, window, cx);
                assert_eq!(state.value(), "abé");
                assert_eq!(state.marked_range(), None);
                // and one a click ends, kept as text
                state.replace_and_mark_text_in_range(None, "明", None, window, cx);
                assert_eq!(state.marked_range(), Some(4..7));
                state.unmark_text(window, cx);
                assert_eq!(state.value(), "abé明");
                assert_eq!(state.marked_range(), None);
            });
        });
    }

    /// A composition leaves an input that loses the focus, as it goes with the
    /// keyboard: a dead key's ´ stayed marked in Kvist's editor after a shortcut
    /// that opened another input, its letter typed there, and what was typed in the
    /// editor next took its place (elundqvist/kvist#482). The text and the
    /// selection an input has when nothing is marked stay as they are.
    #[gpui::test]
    fn a_composition_leaves_an_input_that_loses_the_focus(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state.default_value("ab"));
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_range(2..2, cx);
                state.replace_and_mark_text_in_range(None, "´", None, window, cx);
                assert_eq!(state.value(), "ab´");
                state.on_blur(window, cx);
                assert_eq!(state.value(), "ab");
                assert_eq!(state.marked_range(), None);
                assert_eq!(state.selected_range(), 2..2);

                // typed here next, it goes where it is typed
                state.on_focus(window, cx);
                state.set_selected_range(0..0, cx);
                state.replace_text_in_range(None, "c", window, cx);
                assert_eq!(state.value(), "cab");

                // nothing marked, the selection stays
                state.set_selected_range(0..2, cx);
                state.on_blur(window, cx);
                assert_eq!(state.value(), "cab");
                assert_eq!(state.selected_range(), 0..2);
            });
        });
    }

    /// An edit of the input's own, a shortcut's, lets go of a dead key's ´
    /// first, as GTK's text widgets reset their input method before one, and
    /// is made where it was asked for and is a step of undo of its own: a
    /// shortcut after a dead key runs with the ´ marked, and Ctrl+Delete's
    /// word, merged into the step the ´ had open, was in no step, so an undo
    /// did not bring it back (elundqvist/kvist#482).
    #[gpui::test]
    fn an_edit_of_the_inputs_own_lets_go_of_a_composition_first(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("hello world!", window, cx);

                // Ctrl+Delete: the word after the ´, and an undo brings it back
                state.set_selected_range(6..6, cx);
                state.replace_and_mark_text_in_range(None, "´", None, window, cx);
                assert_eq!(state.value(), "hello ´world!");
                state.delete_next_word(&DeleteToNextWordEnd, window, cx);
                assert_eq!(state.value(), "hello !");
                assert_eq!(state.marked_range(), None);
                assert_eq!(state.selected_range(), 6..6);
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "hello world!");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "hello world!", "the ´ was in no step");

                // a paste goes in where the ´ was, the ´ gone, one step
                state.set_selected_range(5..5, cx);
                state.replace_and_mark_text_in_range(None, "´", None, window, cx);
                state.paste_item(Some(ClipboardItem::new_string(",".into())), window, cx);
                assert_eq!(state.value(), "hello, world!");
                assert_eq!(state.selected_range(), 6..6);
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "hello world!");

                // an undo takes back the step before the ´, the ´ gone
                state.set_selected_range(12..12, cx);
                state.replace_text_in_range(None, "?", window, cx);
                state.replace_and_mark_text_in_range(None, "´", None, window, cx);
                assert_eq!(state.value(), "hello world!?´");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "hello world!");
                assert_eq!(state.marked_range(), None);
                state.redo(&Redo, window, cx);
                assert_eq!(state.value(), "hello world!?");

                // an edit of the selection: the selection's text, not the ´,
                // which went for it
                state.set_selected_range(0..0, cx);
                state.replace_and_mark_text_in_range(None, "´", None, window, cx);
                state.set_selected_range(8..13, cx);
                assert_eq!(state.value(), "´hello world!?");
                state.backspace(&Backspace, window, cx);
                assert_eq!(state.value(), "hello !?");
                assert_eq!(state.selected_range(), 6..6);
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_composition_is_one_undo_group(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("a", window, cx);
                state.replace_and_mark_text_in_range(None, "s", None, window, cx);
                state.replace_and_mark_text_in_range(None, "sh", None, window, cx);
                state.replace_text_in_range(None, "是", window, cx);
                assert_eq!(state.value(), "a是");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "a");
                state.redo(&Redo, window, cx);
                assert_eq!(state.value(), "a是");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_consecutive_compositions_are_separate_groups(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                // First composition: "jin" -> "今天"
                state.replace_and_mark_text_in_range(None, "j", None, window, cx);
                state.replace_and_mark_text_in_range(None, "jin", None, window, cx);
                state.replace_text_in_range(None, "今天", window, cx);
                // Second composition: "wo" -> "我们"
                state.replace_and_mark_text_in_range(None, "w", None, window, cx);
                state.replace_and_mark_text_in_range(None, "wo", None, window, cx);
                state.replace_text_in_range(None, "我们", window, cx);
                assert_eq!(state.value(), "今天我们");
                assert_eq!(state.selected_range(), 12..12);

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "今天");
                assert_eq!(state.selected_range(), 6..6);

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "");
                assert_eq!(state.selected_range(), 0..0);

                state.redo(&Redo, window, cx);
                assert_eq!(state.value(), "今天");
                assert_eq!(state.selected_range(), 6..6);

                state.redo(&Redo, window, cx);
                assert_eq!(state.value(), "今天我们");
                assert_eq!(state.selected_range(), 12..12);
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_typing_after_composition_is_a_separate_group(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_and_mark_text_in_range(None, "n", None, window, cx);
                state.replace_text_in_range(None, "你", window, cx);
                state.undo_manager.pending_intent = Some(EditIntent::Typing);
                state.replace_text_in_range(None, "a", window, cx);
                state.undo_manager.pending_intent = Some(EditIntent::Typing);
                state.replace_text_in_range(None, "b", window, cx);
                assert_eq!(state.value(), "你ab");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "你");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_composition_cancel_leaves_no_entry(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("a", window, cx);
                state.replace_and_mark_text_in_range(None, "s", None, window, cx);
                state.replace_and_mark_text_in_range(None, "", None, window, cx);

                assert_eq!(state.value(), "a");
                assert!(!state.undo_manager.has_undos());
            });
        });
    }

    /// An empty composition where nothing is marked, then its end, leaves
    /// the text and its selection be, as the X11 client hands them over
    /// for a composition it knows of as the window loses the keyboard: the
    /// focus may have moved to a field with nothing marked, whose selection
    /// was deleted. With something marked, the mark goes, as before
    /// (elundqvist/kvist#435).
    #[gpui::test]
    fn test_an_empty_mark_with_nothing_marked_leaves_the_selection(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("rename me", window, cx);
                state.set_selected_range(0..6, cx);
                state.replace_and_mark_text_in_range(None, "", None, window, cx);
                state.unmark_text(window, cx);
                assert_eq!(state.value(), "rename me");
                assert_eq!(state.selected_range(), 0..6);
                assert!(!state.undo_manager.has_undos());

                state.set_selected_range(9..9, cx);
                state.replace_and_mark_text_in_range(None, "ni", None, window, cx);
                state.replace_and_mark_text_in_range(None, "", None, window, cx);
                state.unmark_text(window, cx);
                assert_eq!(state.value(), "rename me");
                assert_eq!(state.selected_range(), 9..9);
            });
        });
    }

    /// A composition whose mark ends with its text kept, as a click or
    /// Escape ends one, is an edit as a commit is, and says so: marking
    /// says nothing, and Kvist's editor, which saves as it is typed in, said
    /// "Saved" of a file without the text (elundqvist/kvist#455). An end
    /// with nothing marked is none, nor is the one macOS sends after a
    /// commit, which said so itself.
    #[gpui::test]
    fn test_a_composition_kept_as_its_mark_ends_is_a_change(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;
        let changes = std::rc::Rc::new(std::cell::Cell::new(0));
        cx.update(|_, cx| {
            let changes = changes.clone();
            cx.subscribe(&input, move |_, event: &InputEvent, _| {
                if matches!(event, InputEvent::Change) {
                    changes.set(changes.get() + 1);
                }
            })
            .detach();
        });
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("a", window, cx);
                state.set_selected_range(1..1, cx);
            })
        });
        let before = changes.get();

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_and_mark_text_in_range(None, "日", None, window, cx);
                state.replace_and_mark_text_in_range(None, "明", None, window, cx);
            })
        });
        assert_eq!(changes.get(), before, "a mark is no change");
        cx.update(|window, cx| input.update(cx, |state, cx| state.unmark_text(window, cx)));
        assert_eq!(changes.get(), before + 1, "the text kept is");
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                assert_eq!(state.value(), "a明");
                state.unmark_text(window, cx);
            })
        });
        assert_eq!(changes.get(), before + 1, "an end with nothing marked is none");

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_and_mark_text_in_range(None, "日", None, window, cx);
                state.replace_text_in_range(None, "日", window, cx);
                state.unmark_text(window, cx);
                assert_eq!(state.value(), "a明日");
            })
        });
        assert_eq!(changes.get(), before + 2, "a commit is one change, its end none");
    }

    #[gpui::test]
    fn test_undo_manager_selection_restored_by_undo_and_redo(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("abc", window, cx);
                state.set_selected_range(1..2, cx);
                state.replace_text_in_range(None, "X", window, cx);

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "abc");
                assert_eq!(state.selected_range(), 1..2);

                state.redo(&Redo, window, cx);
                assert_eq!(state.value(), "aXc");
                assert_eq!(state.selected_range(), 2..2);
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_forward_delete_restores_cursor(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("abc", window, cx);
                state.set_selected_range(1..1, cx);
                state.delete(&Delete, window, cx);

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "abc");
                assert_eq!(state.selected_range(), 1..1);
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_selection_movement_preserves_redo(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "ab", window, cx);
                state.undo(&Undo, window, cx);
                state.set_selected_range(0..0, cx);
                state.redo(&Redo, window, cx);
                assert_eq!(state.value(), "ab");

                state.undo(&Undo, window, cx);
                state.replace_text_in_range(None, "x", window, cx);
                state.redo(&Redo, window, cx);
                assert_eq!(state.value(), "x");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_noop_edit_preserves_redo(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "a", window, cx);
                state.undo(&Undo, window, cx);
                state.backspace(&Backspace, window, cx);
                state.redo(&Redo, window, cx);
                assert_eq!(state.value(), "a");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_noop_edit_breaks_coalescing_without_clearing_history(
        cx: &mut TestAppContext,
    ) {
        let input_view = InputView::build(cx, |state| state);
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.replace_text_in_range(None, "alpha", window, cx);
                state.replace_text_in_range(None, "", window, cx);
                state.replace_text_in_range(None, "beta", window, cx);
                assert_eq!(state.value(), "alphabeta");

                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "alpha");
                state.undo(&Undo, window, cx);
                assert_eq!(state.value(), "");
            });
        });
    }

    #[gpui::test]
    fn test_undo_manager_masked_redo_restores_actual_cursor(cx: &mut TestAppContext) {
        let input_view = InputView::build(cx, |state| {
            state.mask_pattern(MaskPattern::Number {
                separator: Some(','),
                fraction: None,
            })
        });
        let mut cx = VisualTestContext::from_window(input_view.window_handle.into(), cx);
        let input = input_view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("12345", window, cx);
                state.set_selected_range(2..2, cx);
                state.replace_text_in_range(None, "9", window, cx);
                let selection_after_edit = state.selected_range();
                assert_ne!(selection_after_edit.end, state.value().len());

                state.undo(&Undo, window, cx);
                state.redo(&Redo, window, cx);
                assert_eq!(state.selected_range(), selection_after_edit);
            });
        });
    }

    /// Unfolding at a position opens exactly the folds hiding it.
    ///
    /// A fold keeps its own first and last line visible, so a position on
    /// either of them opens nothing. Nested folds all open at once, sibling
    /// folds stay closed, and the opened ranges stay fold candidates.
    #[gpui::test]
    fn test_unfold_at(cx: &mut TestAppContext) {
        use crate::input::{FoldRange, Position};

        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;

        // An outer fold over lines 0..=5, a fold nested inside it, and a
        // sibling fold that must never be touched.
        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl", window, cx);
                state.apply_highlighter_fold_candidates(
                    vec![
                        FoldRange::new(0, 5),
                        FoldRange::new(2, 4),
                        FoldRange::new(7, 10),
                    ],
                    cx,
                );
                state.display_map.set_folded(0, true);
                state.display_map.set_folded(2, true);
                state.display_map.set_folded(7, true);
            });
        });

        // The outer fold's own first and last line stay visible, so neither
        // position opens anything.
        for line in [0, 5] {
            cx.update(|_, cx| {
                input.update(cx, |state, cx| {
                    assert!(!state.display_map.is_buffer_line_hidden(line));
                    assert!(!state.unfold_at(Position::new(line as u32, 0), cx));
                });
                input.read_with(cx, |state, _| {
                    assert!(state.display_map.is_folded_at(0));
                    assert!(state.display_map.is_folded_at(2));
                    assert!(state.display_map.is_folded_at(7));
                });
            });
        }

        // Line 3 is hidden by both the outer and the nested fold, so both
        // open; the sibling fold does not.
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                assert!(state.unfold_at(Position::new(3, 0), cx));
            });
            input.read_with(cx, |state, _| {
                assert!(!state.display_map.is_buffer_line_hidden(3));
                assert!(!state.display_map.is_folded_at(0));
                assert!(!state.display_map.is_folded_at(2));
                assert!(state.display_map.is_folded_at(7));
                // The opened ranges are still candidates for refolding.
                assert!(state.display_map.is_fold_candidate(0));
                assert!(state.display_map.is_fold_candidate(2));
            });
        });

        // Nothing is hidden there any more, so a second call is a no-op.
        cx.update(|_, cx| {
            input.update(cx, |state, cx| {
                assert!(!state.unfold_at(Position::new(3, 0), cx));
            });
        });
    }

    /// Losing focus hides the hover popover but keeps the decorations.
    ///
    /// Both used to be dropped by one call, so clicking away threw away
    /// decorations the application had installed and never asked to remove.
    #[gpui::test]
    fn test_blur_keeps_decorations(cx: &mut TestAppContext) {
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_value("select 1", window, cx);
                let _collection = state.create_decorations_collection(
                    vec![crate::input::TextDecoration::new(
                        0..6,
                        gpui::HighlightStyle {
                            font_weight: Some(gpui::FontWeight::BOLD),
                            ..Default::default()
                        },
                    )],
                    cx,
                );
                state.present_hover(
                    0..6,
                    lsp_types::Hover {
                        contents: lsp_types::HoverContents::Scalar(
                            lsp_types::MarkedString::String("docs".into()),
                        ),
                        range: None,
                    },
                    cx,
                );
                assert!(state.hover_popover().is_some());

                state.on_blur(window, cx);

                assert!(
                    state.hover_popover().is_none(),
                    "blur should hide the hover popover"
                );
                let decorations = state.extras.decoration_layers();
                assert!(
                    decorations.iter().any(|layer| !layer.is_empty()),
                    "blur must not discard decorations"
                );
            });
        });
    }

    /// The mode marker is the only source of truth for the kind of input.
    ///
    /// An auto-growing textarea capped at one row used to report itself as
    /// single-line, because the answer was derived from the row counts.
    #[gpui::test]
    fn test_kind_does_not_follow_the_row_count(cx: &mut TestAppContext) {
        let view = InputView::build_textarea(cx, |state| state.auto_grow(1, 1));
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        view.input.read_with(&mut cx, |state, _| {
            assert!(state.is_multi_line());
            assert!(!state.is_single_line());
            assert!(!state.is_code_editor());
        });
    }

    /// A value read off the UI thread is set as `set_value` sets the same
    /// text in an input that does not wrap: the text, its lines, the caret
    /// at the start and nothing to undo, and so after the frame drawn.
    #[gpui::test]
    fn a_prepared_value_is_set_as_set_value_sets_it(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let view = InputView::build_editor(cx, |state| state.soft_wrap(false));
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        type Seen = (
            String,
            Vec<(usize, Vec<Range<usize>>)>,
            Range<usize>,
            bool,
            usize,
        );
        let seen = |cx: &mut VisualTestContext| -> Seen {
            input.read_with(cx, |state, _| {
                let map = &state.display_map;
                let lines = (0..map.buffer_line_count())
                    .map(|row| {
                        let line = map.line(row).unwrap();
                        (line.len(), line.wrapped_lines.to_vec())
                    })
                    .collect();
                (
                    state.value().to_string(),
                    lines,
                    state.selected_range.into(),
                    state.undo_manager.has_undos(),
                    map.display_row_count(),
                )
            })
        };
        let draw = |cx: &mut VisualTestContext| cx.update(|window, cx| window.draw(cx).clear(cx));
        for text in [
            "",
            "one",
            "one\ntwo\n",
            "a\r\nb\r\n\r\nc",
            "lone\rcr\n\n",
            "中文\nx",
        ] {
            cx.update(|window, cx| input.update(cx, |state, cx| state.set_value(text, window, cx)));
            let by_value = seen(&mut cx);
            draw(&mut cx);
            let drawn = seen(&mut cx);
            cx.update(|window, cx| {
                input.update(cx, |state, cx| {
                    state.set_value("something else\nentirely", window, cx);
                    state.insert("typed", window, cx);
                    state.set_prepared_value(PreparedValue::new(text), cx);
                })
            });
            assert_eq!(seen(&mut cx), by_value, "{text:?}");
            draw(&mut cx);
            assert_eq!(seen(&mut cx), drawn, "{text:?} drawn");
        }
        fn is_send<T: Send>() {}
        is_send::<PreparedValue>();
    }

    /// The room `gutter_extra` keeps is laid out between the line numbers and
    /// the fold icons, the text moved right by as much, and it is said where
    /// it is once drawn; without it there is none.
    #[gpui::test]
    fn gutter_extra_keeps_room_between_the_line_numbers_and_the_fold_icons(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::init);
        let drawn = |view: &InputView<EditorMode>, cx: &mut TestAppContext| {
            let mut vcx = VisualTestContext::from_window(view.window_handle.into(), cx);
            vcx.update(|window, cx| {
                view.input.update(cx, |state, cx| {
                    state.set_value("fn main() {}\n", window, cx)
                })
            });
            vcx.update(|window, cx| window.draw(cx).clear(cx));
            view.input.read_with(&mut vcx, |state, _| {
                let origin = state.input_bounds().origin.x;
                let text = state.range_to_bounds(&(0..0)).unwrap().origin.x - origin;
                let room = state
                    .gutter_extra_bounds()
                    .map(|b| (b.left() - origin, b.right() - origin, b.size.height));
                (text, room)
            })
        };
        let plain = InputView::build_editor(cx, |state| state);
        let roomy = InputView::build_editor(cx, |state| state.gutter_extra(px(16.)));
        let (plain_text, none) = drawn(&plain, cx);
        let (roomy_text, room) = drawn(&roomy, cx);
        assert_eq!(none, None);
        assert_eq!(roomy_text - plain_text, px(16.));
        let (left, right, height) = room.expect("the room is laid out");
        assert_eq!(right - left, px(16.));
        assert!(left > px(0.) && height > px(0.), "{left:?} {height:?}");
        // the fold icons' 18 px and the 10 px margin lie between it and the text
        assert_eq!(roomy_text - right, px(18.) + px(10.));
    }

    /// Soft wrap is on by default, for every mode that can wrap.
    ///
    /// The default lives in the shared constructor, where a mode-specific
    /// `new` can silently fail to restore it; this pins it down.
    #[gpui::test]
    fn test_soft_wrap_is_enabled_by_default(cx: &mut TestAppContext) {
        let textarea = InputView::build_textarea(cx, |state| state);
        let mut textarea_cx = VisualTestContext::from_window(textarea.window_handle.into(), cx);
        textarea
            .input
            .read_with(&mut textarea_cx, |state, _| assert!(state.soft_wrap));

        let editor = InputView::<EditorMode>::new(cx);
        let mut editor_cx = VisualTestContext::from_window(editor.window_handle.into(), cx);
        editor
            .input
            .read_with(&mut editor_cx, |state, _| assert!(state.soft_wrap));
    }

    /// A view holding an input, docked as a cached view, as Kvist docks a
    /// drawer: and how often it is drawn.
    struct Holder<M: InputModeKind> {
        input: Entity<InputBaseState<M>>,
        drawn: Rc<Cell<usize>>,
        width: Pixels,
    }

    impl<M: InputModeKind> Render for Holder<M> {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.drawn.set(self.drawn.get() + 1);
            div().w(self.width).h(px(60.)).child(self.input.clone())
        }
    }

    struct Dock<M: InputModeKind>(Entity<Holder<M>>);

    impl<M: InputModeKind> Render for Dock<M> {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                self.0
                    .clone()
                    .cached(gpui::StyleRefinement::default().size_full()),
            )
        }
    }

    /// An input in a view of its own, docked as a cached view, and how
    /// often that view is drawn.
    struct Docked<M: InputModeKind> {
        cx: VisualTestContext,
        dock: Entity<Dock<M>>,
        holder: Entity<Holder<M>>,
        input: Entity<InputBaseState<M>>,
        drawn: Rc<Cell<usize>>,
    }

    impl<M: InputModeKind> Docked<M> {
        fn new(
            cx: &mut TestAppContext,
            make: impl FnOnce(&mut Window, &mut Context<InputBaseState<M>>) -> InputBaseState<M>
                + 'static,
        ) -> Self {
            let drawn = Rc::new(Cell::new(0));
            let mut parts = None;
            let window = cx.update(|cx| {
                let drawn = drawn.clone();
                cx.open_window(Default::default(), |window, cx| {
                    cx.set_global(Theme::default());
                    super::super::init(cx);
                    let input = cx.new(|cx| make(window, cx));
                    let holder = cx.new(|_| Holder {
                        input: input.clone(),
                        drawn,
                        width: px(300.),
                    });
                    let dock = cx.new(|_| Dock(holder.clone()));
                    parts = Some((dock.clone(), holder, input));
                    dock
                })
                .unwrap()
            });
            let (dock, holder, input) = parts.unwrap();
            let cx = VisualTestContext::from_window(window.into(), cx);
            Self { cx, dock, holder, input, drawn }
        }

        /// One frame of the window, the dock drawn in it as it is with each.
        fn draw(&mut self) {
            let dock = self.dock.clone();
            self.cx.update(|window, cx| {
                dock.update(cx, |_, cx| cx.notify());
                window.draw(cx).clear(cx);
            })
        }

        /// Frames until the view is left, and how often it was drawn by then.
        fn settles(&mut self, what: &str) -> usize {
            self.draw();
            self.draw();
            let settled = self.drawn.get();
            for _ in 0..3 {
                self.draw();
            }
            assert_eq!(self.drawn.get(), settled, "drawn again with frames {what}");
            settled
        }
    }

    /// The input `make` makes, in a view of its own docked as a cached
    /// view: drawn again where it changed and not with each frame of the
    /// window, a frame the dock above it is drawn in each time; after a
    /// change it is left as soon as the frame after the change has drawn
    /// it with what the change's paint laid out.
    fn drawn_only_at_its_changes<M: InputModeKind>(
        cx: &mut TestAppContext,
        make: impl FnOnce(&mut Window, &mut Context<InputBaseState<M>>) -> InputBaseState<M> + 'static,
    ) {
        let mut docked = Docked::new(cx, make);
        let first = docked.settles("after the first");
        assert!(first > 0);
        let input = docked.input.clone();
        docked
            .cx
            .update(|window, cx| input.update(cx, |state, cx| state.insert("typed", window, cx)));
        let typed = docked.settles("after a key");
        assert!(typed > first, "a key draws it again");
        let holder = docked.holder.clone();
        holder.update(&mut docked.cx, |holder, cx| {
            holder.width = px(40.);
            cx.notify();
        });
        let narrow = docked.settles("after it was made narrower");
        assert!(narrow > typed);
    }

    /// The text input's paint tells its state's observers only where it
    /// moved something (the layout, the bounds, the caret, the scroll):
    /// told at every paint, a drawer with a text box, docked as a cached
    /// view, was drawn again with every frame the window drew
    /// (elundqvist/kvist#602). A single-line box, a drawer's filter, and a
    /// multi-line one, the assistant's prompt.
    #[gpui::test]
    fn a_paint_that_moves_nothing_draws_no_view_the_input_is_in_again(cx: &mut TestAppContext) {
        cx.update(crate::init);
        drawn_only_at_its_changes(cx, |window, cx| crate::input::InputState::new(window, cx));
        drawn_only_at_its_changes(cx, |window, cx| {
            crate::input::TextareaState::new(window, cx)
        });
        drawn_only_at_its_changes(cx, |window, cx| {
            crate::input::EditorState::new(window, cx).language("sql")
        });
    }

    /// The scroll offset kept is told to the state's observers where it
    /// changed, and not where it is kept as it was, as the paint keeps it
    /// in every frame (elundqvist/kvist#602).
    #[gpui::test]
    fn the_scroll_offset_is_told_only_where_it_changed(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let view = InputView::<EditorMode>::new(cx);
        let mut cx = VisualTestContext::from_window(view.window_handle.into(), cx);
        let input = view.input;
        let text: String = (0..200).map(|i| format!("line {i}\n")).collect();
        cx.update(|window, cx| input.update(cx, |state, cx| state.set_value(text, window, cx)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let told = Rc::new(Cell::new(0));
        let _observing = cx.update(|_, cx| {
            let told = told.clone();
            cx.observe(&input, move |_, _| told.set(told.get() + 1))
        });
        let scroll = |cx: &mut VisualTestContext, y: f32| {
            input.update(cx, |state, cx| {
                state.update_scroll_offset(Some(point(px(0.), px(y))), cx)
            });
            cx.run_until_parked();
        };
        scroll(&mut cx, -100.);
        assert_eq!(told.get(), 1, "scrolled");
        assert_eq!(
            input.read_with(&mut cx, |state, _| state.scroll_handle.offset().y),
            px(-100.)
        );
        scroll(&mut cx, -100.);
        assert_eq!(told.get(), 1, "kept where it was");
        scroll(&mut cx, -120.);
        assert_eq!(told.get(), 2, "scrolled again");
    }

    /// A scroll the paint applies (a few pixels, with the same lines in
    /// sight, the caret where it was) is told to the views the editor is in
    /// like any other change, and by the bounds alone: the scrollbar's
    /// prepaint has kept the offset in the handle by the time of the paint,
    /// and `update_scroll_offset` finds nothing to tell
    /// (elundqvist/kvist#602).
    #[gpui::test]
    fn a_scroll_the_paint_applied_draws_the_view_again(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let mut docked = Docked::new(cx, |window, cx| {
            crate::input::EditorState::new(window, cx).language("sql")
        });
        let text: String = (0..60).map(|i| format!("select {i};\n")).collect();
        let input = docked.input.clone();
        docked
            .cx
            .update(|window, cx| input.update(cx, |state, cx| state.set_value(text, window, cx)));
        docked.settles("after the text");
        let before = docked.drawn.get();
        docked.cx.update(|_, cx| {
            input.update(cx, |state, cx| state.set_scroll_offset(point(px(0.), px(-3.)), cx))
        });
        // the frame that applies the scroll, and the one its paint asks for
        docked.draw();
        docked.draw();
        assert_eq!(
            input.read_with(&docked.cx, |state, _| state.scroll_offset().y),
            px(-3.)
        );
        assert!(
            docked.drawn.get() >= before + 2,
            "drawn {} times since the scroll, once for it and once for what its paint moved",
            docked.drawn.get() - before
        );
        docked.settles("after a scroll");
    }
}

/// Methods that only a single-line input offers.
impl InputBaseState<crate::input::InputMode> {
    /// Create a single-line text input state.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_in_mode(window, cx)
    }

    /// Set a custom step function of the [`super::NumberInput`].
    ///
    /// The `f` receives the current value and the [`StepAction`], and returns
    /// the step to apply, so the step can vary with the value.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // At the boundary 1.0 the step is 0.1 going down and 0.5 going up.
    /// InputState::new(window, cx).step_by(|value, action, _cx| match action {
    ///     StepAction::Increment => if value < 1.0 { 0.1 } else { 0.5 },
    ///     StepAction::Decrement => if value <= 1.0 { 0.1 } else { 0.5 },
    /// })
    /// ```
    pub fn step_by(mut self, f: impl Fn(f64, StepAction, &mut App) -> f64 + 'static) -> Self {
        self.number_step = Some(NumberStep::by_value(f));
        self
    }

    /// Set with password masked state.
    pub fn masked(mut self, masked: bool) -> Self {
        self.masked = masked;
        self
    }

    /// Set the password masked state of the input field.
    pub fn set_masked(&mut self, masked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.masked = masked;
        cx.notify();
    }

    /// Set the regular expression pattern of the input field.
    pub fn pattern(mut self, pattern: regex::Regex) -> Self {
        self.pattern = Some(pattern);
        self
    }

    /// Set the regular expression pattern of the input field with reference.
    pub fn set_pattern(
        &mut self,
        pattern: regex::Regex,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.pattern = Some(pattern);
    }

    /// Set the validation function of the input field.
    pub fn validate(mut self, f: impl Fn(&str, &mut App) -> bool + 'static) -> Self {
        self.validate = Some(Box::new(f));
        self
    }

    pub fn set_validator(
        &mut self,
        validate: impl Fn(&str, &mut App) -> bool + 'static,
        _cx: &mut Context<Self>,
    ) {
        self.validate = Some(Box::new(validate));
    }

    /// Set the step value of the [`super::NumberInput`] for increment/decrement.
    ///
    /// If any of `step`, `min`, `max` is set, the [`super::NumberInput`] will
    /// update the value internally (step by `step`, default 1, clamp to the
    /// `min`/`max` range and emit [`InputEvent::Change`]) instead of emitting
    /// [`super::NumberInputEvent::Step`].
    ///
    /// See also [`Self::step_by`] to calculate the step value
    /// based on the current value.
    pub fn step(mut self, step: impl Into<NumberStep>) -> Self {
        self.number_step = Some(step.into());
        self
    }

    /// Set the minimum value of the [`super::NumberInput`].
    ///
    /// The value will be clamped to the minimum value on stepping and on
    /// blur (only if the clamped value passes the `pattern`/`validate` check).
    /// See also [`Self::step`].
    pub fn min(mut self, min: f64) -> Self {
        self.number_min = Some(min);
        self
    }

    /// Set the maximum value of the [`super::NumberInput`].
    ///
    /// The value will be clamped to the maximum value on stepping and on
    /// blur (only if the clamped value passes the `pattern`/`validate` check).
    /// See also [`Self::step`].
    pub fn max(mut self, max: f64) -> Self {
        self.number_max = Some(max);
        self
    }

    /// Update the step value after construction, `None` to fall back to
    /// emitting [`super::NumberInputEvent::Step`] (if `min`, `max` are unset).
    ///
    /// See [`Self::step`] and [`Self::step_by`].
    pub fn set_step(
        &mut self,
        step: impl Into<Option<NumberStep>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
        self.number_step = step.into();
    }

    /// Update the minimum value after construction. See [`Self::min`].
    pub fn set_min(&mut self, min: Option<f64>, _: &mut Window, _: &mut Context<Self>) {
        self.number_min = min;
    }

    /// Update the maximum value after construction. See [`Self::max`].
    pub fn set_max(&mut self, max: Option<f64>, _: &mut Window, _: &mut Context<Self>) {
        self.number_max = max;
    }

    /// Set true to show spinner at the input right.
    pub fn set_loading(&mut self, loading: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.loading = loading;
        cx.notify();
    }
}

/// Methods shared by the two multi-line modes, and reachable on neither a
/// single-line input nor anything else.
impl<M: crate::input::MultiLineMode> InputBaseState<M> {
    /// Set this input is searchable, default is false (Default true for Code Editor).
    #[doc(hidden)]
    pub fn searchable(mut self, searchable: bool) -> Self {
        self.searchable = searchable;
        self
    }

    pub fn set_searchable(&mut self, searchable: bool, cx: &mut Context<Self>) {
        self.searchable = searchable;
        cx.notify();
    }

    /// Set the soft wrap mode, default is true.
    #[doc(hidden)]
    pub fn soft_wrap(mut self, wrap: bool) -> Self {
        self.soft_wrap = wrap;
        self
    }

    /// Update the soft wrap mode, default is true.
    pub fn set_soft_wrap(&mut self, wrap: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.soft_wrap = wrap;
        if wrap {
            let wrap_width = self
                .last_layout
                .as_ref()
                .and_then(|b| b.wrap_width)
                .unwrap_or(self.input_bounds.size.width);

            self.display_map.on_layout_changed(Some(wrap_width), cx);

            // Reset scroll to left 0
            let mut offset = self.scroll_handle.offset();
            offset.x = px(0.);
            self.scroll_handle.set_offset(offset);
        } else {
            self.display_map.on_layout_changed(None, cx);
        }
        cx.notify();
    }

    /// Set how soft-wrapped continuation lines are indented, default is [`WrappingIndent::Same`]
    #[doc(hidden)]
    pub fn wrapping_indent(mut self, wrapping_indent: WrappingIndent) -> Self {
        self.wrapping_indent = wrapping_indent;
        self
    }

    /// Update how soft-wrapped continuation lines are indented.
    pub fn set_wrapping_indent(
        &mut self,
        wrapping_indent: WrappingIndent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.wrapping_indent = wrapping_indent;
        self.display_map.set_wrapping_indent(wrapping_indent, cx);
        cx.notify();
    }
}

/// Methods that only ordinary multi-line text offers.
impl InputBaseState<crate::input::TextareaMode> {
    /// Create a multi-line text state.
    ///
    /// Being multi-line is carried by the mode, not by the layout, so the
    /// default plain-text layout needs no adjustment here.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_in_mode(window, cx)
    }

    pub fn set_auto_grow(&mut self, min_rows: usize, max_rows: usize, cx: &mut Context<Self>) {
        self.mode = LayoutMode::auto_grow(min_rows, max_rows.max(min_rows));
        cx.notify();
    }

    /// Set the number of rows for the multi-line Textarea.
    ///
    /// This is only used when `multi_line` is set to true.
    ///
    /// default: 2
    #[doc(hidden)]
    pub fn rows(mut self, rows: usize) -> Self {
        match &mut self.mode {
            LayoutMode::PlainText { rows: r, .. } | LayoutMode::CodeEditor { rows: r, .. } => {
                *r = rows
            }
            LayoutMode::AutoGrow {
                max_rows: max_r,
                rows: r,
                ..
            } => {
                *r = rows;
                *max_r = rows;
            }
        }
        self
    }

    pub fn set_rows(&mut self, rows: usize, cx: &mut Context<Self>) {
        match &mut self.mode {
            LayoutMode::PlainText { rows: value, .. }
            | LayoutMode::CodeEditor { rows: value, .. } => *value = rows,
            LayoutMode::AutoGrow {
                rows: value,
                max_rows,
                ..
            } => {
                *value = rows;
                *max_rows = rows;
            }
        }
        cx.notify();
    }

    /// Grow with the content from `min_rows` through `max_rows`.
    pub fn auto_grow(mut self, min_rows: usize, max_rows: usize) -> Self {
        self.mode = LayoutMode::auto_grow(min_rows, max_rows);
        self
    }
}

/// Methods that only a source-code editor offers.
impl InputBaseState<crate::input::EditorMode> {
    /// Keep `width` of room in the gutter, between the line numbers and the
    /// fold icons, for the caller to paint in: a run button or a breakpoint
    /// beside a line, which the editor itself knows nothing of. The text
    /// moves right by as much. [`Self::gutter_extra_bounds`] says where the
    /// room is once the editor has been laid out.
    pub fn gutter_extra(mut self, width: Pixels) -> Self {
        self.gutter_extra = width.max(px(0.));
        self
    }

    /// Change the room [`Self::gutter_extra`] keeps.
    pub fn set_gutter_extra(&mut self, width: Pixels, cx: &mut Context<Self>) {
        let width = width.max(px(0.));
        if width != self.gutter_extra {
            self.gutter_extra = width;
            cx.notify();
        }
    }

    /// Where the room [`Self::gutter_extra`] keeps was laid out in the last
    /// frame, in window coordinates, as tall as the input: `None` before the
    /// first layout, or with no room kept. It does not move when the text
    /// scrolls sideways, as the line numbers do not; a row's place in it is
    /// the row's own, which [`Self::range_to_bounds`] answers.
    pub fn gutter_extra_bounds(&self) -> Option<Bounds<Pixels>> {
        if self.gutter_extra <= px(0.) {
            return None;
        }
        let layout = self.last_layout.as_ref()?;
        let left = super::element::gutter_extra_left(
            layout.line_number_width,
            self.gutter_extra,
            self.mode.is_folding(),
        );
        Some(Bounds::new(
            point(
                self.input_bounds.origin.x + left,
                self.input_bounds.origin.y,
            ),
            gpui::size(self.gutter_extra, self.input_bounds.size.height),
        ))
    }

    /// Create a source-code editor state.
    ///
    /// Default options: line numbers on, tab size 2 with soft tabs, indent
    /// guides on, multi-line, and search enabled. Set the language for syntax
    /// highlighting with [`Self::language`]; without one the text is shown
    /// unhighlighted.
    ///
    /// The editor aims at simple code editing or display, not at being a
    /// full-featured code editor. It offers syntax highlighting, auto indent,
    /// line numbers, and handles large text up to about 50K lines.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut state = Self::new_in_mode(window, cx);
        state.mode = LayoutMode::code_editor();
        state.searchable = true;
        state
    }

    /// Set the language to highlight, e.g. `"rust"`.
    ///
    /// See [`Self::set_highlighter`] to change it after construction.
    pub fn language(mut self, language: impl Into<SharedString>) -> Self {
        if let LayoutMode::CodeEditor {
            language: l,
            highlighter,
            ..
        } = &mut self.mode
        {
            *l = language.into();
            *highlighter.borrow_mut() = None;
        }
        self
    }

    /// Set enable/disable code folding.
    ///
    /// Default: true
    #[doc(hidden)]
    pub fn folding(mut self, folding: bool) -> Self {
        if let LayoutMode::CodeEditor { folding: f, .. } = &mut self.mode {
            *f = folding;
        }
        self
    }

    /// Set code folding at runtime.
    ///
    /// When disabling, all existing folds are cleared.
    pub fn set_folding(&mut self, folding: bool, _: &mut Window, cx: &mut Context<Self>) {
        if let LayoutMode::CodeEditor { folding: f, .. } = &mut self.mode {
            *f = folding;
        }
        if !folding {
            self.display_map.clear_folds();
        }
        cx.notify();
    }

    /// Unfold any folded ranges that hide the given position.
    ///
    /// Use this to reveal a position before acting on it (e.g. before
    /// [`Self::set_cursor_position`], which stops at a fold boundary),
    /// without touching folds elsewhere in the buffer. Fold candidates are
    /// kept, so the opened ranges can be folded again from the gutter.
    ///
    /// A fold keeps its own first and last line visible, so a position on
    /// either of them opens nothing. Nested folds all open, since opening
    /// only the outermost would leave the position hidden.
    ///
    /// Returns whether any fold was opened.
    pub fn unfold_at(&mut self, position: impl Into<Position>, cx: &mut Context<Self>) -> bool {
        let offset = self.text.position_to_offset(&position.into());
        let line = self.text.offset_to_point(offset).row;
        // A fold hides start_line + 1 ..= end_line - 1, so a line is hidden
        // exactly when some folded range strictly contains it.
        let covering: Vec<usize> = self
            .display_map
            .folded_ranges()
            .iter()
            .filter(|fold| line > fold.start_line && line < fold.end_line)
            .map(|fold| fold.start_line)
            .collect();
        if covering.is_empty() {
            return false;
        }

        for start_line in covering {
            self.display_map.set_folded(start_line, false);
        }
        cx.notify();
        true
    }

    /// Set enable/disable line number.
    #[doc(hidden)]
    pub fn line_number(mut self, line_number: bool) -> Self {
        if let LayoutMode::CodeEditor { line_number: l, .. } = &mut self.mode {
            *l = line_number;
        }
        self
    }

    /// Set line number.
    pub fn set_line_number(&mut self, line_number: bool, _: &mut Window, cx: &mut Context<Self>) {
        if let LayoutMode::CodeEditor { line_number: l, .. } = &mut self.mode {
            *l = line_number;
        }
        cx.notify();
    }
}
