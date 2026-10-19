// Changed in elundqvist/gpui-kit from gpui-pre-linux 0.3.3 as published to crates.io: FORK.md at
// the crate's root lists the changes.
use std::default::Default;

use x11rb::protocol::{Event, xproto};
use xim::{AHashMap, AttributeName, Client, ClientError, ClientHandler, InputStyle};

pub enum XimCallbackEvent {
    XimXEvent(x11rb::protocol::Event),
    XimPreeditEvent(xproto::Window, String),
    XimCommitEvent(xproto::Window, String),
}

pub struct XimHandler {
    pub im_id: u16,
    pub ic_id: u16,
    pub connected: bool,
    pub window: xproto::Window,
    pub last_callback_event: Option<XimCallbackEvent>,
    /// A click kept the composition where it was, as text, and reset the
    /// input context: what the input method commits before the next key
    /// goes to it is that composition again (elundqvist/kvist#455).
    composition_kept: bool,
    /// The keys sent to the input method since it last said anything,
    /// which a server gone meanwhile never had (elundqvist/kvist#455).
    keys_unanswered: Vec<Event>,
}

/// How many keys sent and not yet answered are kept for a server that
/// goes: a typist's burst, not every key an input method that answers
/// none of them was sent.
const KEYS_UNANSWERED_KEPT: usize = 16;

impl XimHandler {
    pub fn new() -> Self {
        Self {
            im_id: Default::default(),
            ic_id: Default::default(),
            connected: false,
            window: Default::default(),
            last_callback_event: None,
            composition_kept: false,
            keys_unanswered: Vec::new(),
        }
    }

    /// A click ended the composition and kept it, as text, where it was
    /// drawn; the input context is reset behind it.
    ///
    /// Added in elundqvist/gpui-kit (elundqvist/kvist#455): ibus answers
    /// the reset by drawing an empty preedit and committing the
    /// composition, and the commit went where the click had put the caret,
    /// so a click while composing gave the text twice, once where it was
    /// typed and once where the click went. What it commits now, before a
    /// key goes to it, is dropped: the field keeps what it showed, in
    /// place, as Chromium's fields, VS Code's among them, keep a
    /// composition a click ends and pass over what the input method
    /// commits for it after the reset. An input method that hands the composition back in the
    /// reset's reply instead, as the XIM protocol has it, has that reply
    /// passed over likewise, which the handler's default does; one that
    /// commits nothing leaves the kept text as it is.
    pub fn keep_composition(&mut self) {
        self.composition_kept = true;
    }

    /// A key, pressed or let go, went to the input method: what it commits
    /// after a key pressed is what the key made, and goes into the field.
    pub fn key_sent(&mut self, key: &Event) {
        if matches!(key, Event::KeyPress(_)) {
            self.composition_kept = false;
        }
        if self.keys_unanswered.len() == KEYS_UNANSWERED_KEPT {
            self.keys_unanswered.remove(0);
        }
        self.keys_unanswered.push(key.clone());
    }

    /// The input method said something: the keys sent before reached it.
    pub fn heard(&mut self) {
        self.keys_unanswered.clear();
    }

    /// The keys sent since the input method last said anything, in the
    /// order they were sent, for a server that has gone to have them
    /// handled as if there were none: they went to its window after it
    /// was destroyed.
    pub fn take_keys_unanswered(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.keys_unanswered)
    }

    fn commit(&mut self, text: &str) {
        if self.composition_kept {
            log::debug!("XIM commit of a composition a click kept, dropped: {text:?}");
            return;
        }
        self.last_callback_event = Some(XimCallbackEvent::XimCommitEvent(
            self.window,
            String::from(text),
        ));
    }

    fn preedit(&mut self, text: &str) {
        // a preedit drawn after the reset begins a composition of its own,
        // whose commit is the field's
        if !text.is_empty() {
            self.composition_kept = false;
        }
        self.last_callback_event = Some(XimCallbackEvent::XimPreeditEvent(
            self.window,
            String::from(text),
        ));
    }
}

impl<C: Client<XEvent = xproto::KeyPressEvent>> ClientHandler<C> for XimHandler {
    fn handle_connect(&mut self, client: &mut C) -> Result<(), ClientError> {
        client.open("C")
    }

    fn handle_open(&mut self, client: &mut C, input_method_id: u16) -> Result<(), ClientError> {
        self.im_id = input_method_id;

        client.get_im_values(input_method_id, &[AttributeName::QueryInputStyle])
    }

    fn handle_get_im_values(
        &mut self,
        client: &mut C,
        input_method_id: u16,
        _attributes: AHashMap<AttributeName, Vec<u8>>,
    ) -> Result<(), ClientError> {
        let ic_attributes = client
            .build_ic_attributes()
            .push(AttributeName::InputStyle, InputStyle::PREEDIT_CALLBACKS)
            .push(AttributeName::ClientWindow, self.window)
            .push(AttributeName::FocusWindow, self.window)
            .build();
        client.create_ic(input_method_id, ic_attributes)
    }

    fn handle_create_ic(
        &mut self,
        _client: &mut C,
        _input_method_id: u16,
        input_context_id: u16,
    ) -> Result<(), ClientError> {
        self.connected = true;
        self.ic_id = input_context_id;
        Ok(())
    }

    fn handle_commit(
        &mut self,
        _client: &mut C,
        _input_method_id: u16,
        _input_context_id: u16,
        text: &str,
    ) -> Result<(), ClientError> {
        self.commit(text);
        Ok(())
    }

    fn handle_forward_event(
        &mut self,
        _client: &mut C,
        _input_method_id: u16,
        _input_context_id: u16,
        _flag: xim::ForwardEventFlag,
        xev: C::XEvent,
    ) -> Result<(), ClientError> {
        match xev.response_type {
            x11rb::protocol::xproto::KEY_PRESS_EVENT => {
                self.last_callback_event = Some(XimCallbackEvent::XimXEvent(Event::KeyPress(xev)));
            }
            x11rb::protocol::xproto::KEY_RELEASE_EVENT => {
                self.last_callback_event =
                    Some(XimCallbackEvent::XimXEvent(Event::KeyRelease(xev)));
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_close(&mut self, client: &mut C, _input_method_id: u16) -> Result<(), ClientError> {
        client.disconnect()
    }

    fn handle_preedit_draw(
        &mut self,
        _client: &mut C,
        _input_method_id: u16,
        _input_context_id: u16,
        _caret: i32,
        _chg_first: i32,
        _chg_len: i32,
        _status: xim::PreeditDrawStatus,
        preedit_string: &str,
        _feedbacks: Vec<xim::Feedback>,
    ) -> Result<(), ClientError> {
        // XIMReverse: 1, XIMPrimary: 8, XIMTertiary: 32: selected text
        // XIMUnderline: 2, XIMSecondary: 16: underlined text
        // XIMHighlight: 4: normal text
        // XIMVisibleToForward: 64, XIMVisibleToBackward: 128, XIMVisibleCenter: 256: text align position
        // XIMPrimary, XIMHighlight, XIMSecondary, XIMTertiary are not specified,
        // but interchangeable as above
        // Currently there's no way to support these.
        self.preedit(preedit_string);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{KEYS_UNANSWERED_KEPT, XimCallbackEvent, XimHandler};
    use std::ops::Range;
    use x11rb::protocol::{Event, xproto::KeyPressEvent};

    fn key(detail: u8) -> Event {
        Event::KeyPress(KeyPressEvent {
            detail,
            ..Default::default()
        })
    }

    fn key_up(detail: u8) -> Event {
        Event::KeyRelease(KeyPressEvent {
            detail,
            ..Default::default()
        })
    }

    /// A field as gpui-base's input takes what the window hands it: a
    /// preedit marks its text where text is marked, else at the caret, and
    /// an empty one where nothing is marked is no edit; a commit replaces
    /// what is marked, else goes in at the caret; an unmark keeps the
    /// marked text as text.
    #[derive(Default)]
    struct Field {
        text: String,
        caret: usize,
        marked: Option<Range<usize>>,
    }

    impl Field {
        fn take(&mut self, handler: &mut XimHandler) {
            let (text, marks) = match handler.last_callback_event.take() {
                Some(XimCallbackEvent::XimPreeditEvent(_, text)) => (text, true),
                Some(XimCallbackEvent::XimCommitEvent(_, text)) => (text, false),
                _ => return,
            };
            if marks && text.is_empty() && self.marked.is_none() {
                return;
            }
            let range = self.marked.take().unwrap_or(self.caret..self.caret);
            self.text.replace_range(range.clone(), &text);
            self.caret = range.start + text.len();
            self.marked = (marks && !text.is_empty()).then(|| range.start..self.caret);
        }
    }

    /// ibus answers the reset a click while composing asks for with an
    /// empty preedit and a commit of the composition, which went where the
    /// click had put the caret, after the field had kept the composition
    /// as text: one 明 typed was two (elundqvist/kvist#455). The commit
    /// before the next key is dropped, and the field has it once, where it
    /// was typed, the caret where the click put it.
    #[test]
    fn a_composition_a_click_ends_is_kept_once_where_it_is() {
        let mut handler = XimHandler::new();
        let mut field = Field {
            text: "ab".into(),
            caret: 1,
            ..Default::default()
        };
        for preedit in ["日", "明"] {
            handler.preedit(preedit);
            field.take(&mut handler);
        }
        assert_eq!((field.text.as_str(), field.marked.clone()), ("a明b", Some(1..4)));

        // the click: the composition kept as text, the caret at the end
        handler.keep_composition();
        field.marked = None;
        field.caret = field.text.len();
        handler.preedit("");
        field.take(&mut handler);
        handler.commit("明");
        field.take(&mut handler);
        assert_eq!((field.text.as_str(), field.caret), ("a明b", 5));

        // what a key makes after it is the field's
        handler.key_sent(&key(38));
        handler.preedit("日");
        field.take(&mut handler);
        handler.commit("日");
        field.take(&mut handler);
        assert_eq!((field.text.as_str(), field.marked.clone()), ("a明b日", None));
    }

    /// A composition the input method begins after the reset, before any
    /// key, is one of its own, and what it commits goes into the field.
    #[test]
    fn a_composition_begun_after_the_click_is_committed() {
        let mut handler = XimHandler::new();
        let mut field = Field::default();
        handler.keep_composition();
        handler.preedit("日");
        field.take(&mut handler);
        handler.commit("日");
        field.take(&mut handler);
        assert_eq!(field.text, "日");
    }

    /// A key let go is no new input: the composition a click kept is still
    /// what the input method commits.
    #[test]
    fn a_key_let_go_after_the_click_keeps_the_composition_kept() {
        let mut handler = XimHandler::new();
        handler.keep_composition();
        handler.key_sent(&key_up(38));
        handler.commit("明");
        assert!(handler.last_callback_event.is_none());
    }

    /// The keys a server that has gone was sent since it last said anything
    /// are given back in order, to be handled as if there were no input
    /// method: they went to a window destroyed, and the first after ibus
    /// ended was lost (elundqvist/kvist#455). Those it answered are not.
    #[test]
    fn the_keys_a_server_never_answered_are_given_back() {
        let mut handler = XimHandler::new();
        handler.key_sent(&key(38));
        handler.key_sent(&key_up(38));
        handler.heard();
        for detail in [53, 29] {
            handler.key_sent(&key(detail));
            handler.key_sent(&key_up(detail));
        }
        let details: Vec<(bool, u8)> = handler
            .take_keys_unanswered()
            .into_iter()
            .map(|event| match event {
                Event::KeyPress(key) => (true, key.detail),
                Event::KeyRelease(key) => (false, key.detail),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(details, [(true, 53), (false, 53), (true, 29), (false, 29)]);
        assert!(handler.take_keys_unanswered().is_empty());

        // a server that answers none keeps the last of them alone
        for detail in 0..40 {
            handler.key_sent(&key(detail));
        }
        let kept = handler.take_keys_unanswered();
        assert_eq!(kept.len(), KEYS_UNANSWERED_KEPT);
        assert!(matches!(kept.last(), Some(Event::KeyPress(key)) if key.detail == 39));
    }
}
