// Added in elundqvist/gpui-kit to gpui-pre-linux 0.3.3 as published to crates.io: FORK.md at the
// crate's root lists the changes.
use std::collections::VecDeque;

/// The keys down as the X11 client has heard them, for `KeyDownEvent::is_held`
/// (elundqvist/kvist#613): a press of a key already down is the keyboard's
/// repeat of it. The client asks the server for detectable auto-repeat, under
/// which a held key sends its press again and again and its release once, as
/// it is let go; a server that does not grant it sends a release before each
/// repeat, which the client drops where the press comes within
/// `REPEAT_RELEASE_MS` of it (`is_repeat_release`).
///
/// The keys are heard as the server sends them, before an input method sees
/// them, and the presses that were repeats are kept until the client handles
/// them: one the input method forwards back comes later, and a key it keeps
/// for itself, its press or its release, would otherwise leave the key down
/// or up for good.
///
/// A key held when the keyboard comes to the window has no press here: the
/// server tells which keys are down in the `KeymapNotify` it sends right
/// after the `FocusIn` (`heard_keymap`), so the repeats of such a key are
/// repeats too.
#[derive(Default)]
pub(crate) struct HeldKeys {
    /// One bit for each keycode down, as the server's `QueryKeymap` has them.
    down: [u8; 32],
    /// The presses heard that were repeats and are not handled yet, by
    /// keycode and server time, oldest first.
    repeats: VecDeque<(u8, u32)>,
}

/// How many repeats are kept for the client to handle: an input method
/// forwards each key back before the next comes, so a few are plenty.
const REPEATS_KEPT: usize = 16;

/// How close a release has to come before the press of the same key for
/// the two to be the server's repeat, where it grants no detectable
/// auto-repeat: a hand that lets a key go and presses it again takes far
/// longer.
pub(crate) const REPEAT_RELEASE_MS: u32 = 20;

impl HeldKeys {
    /// A press of `keycode` heard at `time`: whether the key was down
    /// already, which makes it a repeat.
    pub(crate) fn heard_press(&mut self, keycode: u8, time: u32) -> bool {
        let repeat = self.is_down(keycode);
        self.down[usize::from(keycode / 8)] |= 1 << (keycode % 8);
        if repeat {
            if self.repeats.len() == REPEATS_KEPT {
                self.repeats.pop_front();
            }
            self.repeats.push_back((keycode, time));
        }
        repeat
    }

    /// A release of `keycode`.
    pub(crate) fn heard_release(&mut self, keycode: u8) {
        self.down[usize::from(keycode / 8)] &= !(1 << (keycode % 8));
    }

    /// Whether the press of `keycode` at `time` being handled was a repeat,
    /// as it was heard. The repeats heard before it are handled already, or
    /// were kept by an input method, and go.
    pub(crate) fn take_repeat(&mut self, keycode: u8, time: u32) -> bool {
        let Some(at) = self
            .repeats
            .iter()
            .position(|&(code, at)| code == keycode && at == time)
        else {
            return false;
        };
        self.repeats.drain(..=at);
        true
    }

    /// Whether `keycode` is down.
    pub(crate) fn is_down(&self, keycode: u8) -> bool {
        self.down[usize::from(keycode / 8)] & (1 << (keycode % 8)) != 0
    }

    /// The window lost the keyboard: the releases of the keys down go to
    /// the window that has it now, so none is down as far as the client
    /// knows until the server says which are (`heard_keymap`).
    pub(crate) fn let_go(&mut self) {
        *self = Self::default();
    }

    /// The server's `KeymapNotify`, which it sends right after each
    /// `FocusIn`: the keys down as it has them, in the order of the events,
    /// so no press or release heard before it is missing and none heard
    /// after it is counted twice. The 31 bytes are the keycodes 8 to 255
    /// (the first byte is 8 to 15), where `down` has 0 to 255. A key down
    /// that this window never heard pressed, one held while the keyboard
    /// was elsewhere, has its repeats known for repeats; a bit left over
    /// from a release lost is cleared, and the repeats kept for the client
    /// to handle are from before it.
    pub(crate) fn heard_keymap(&mut self, keys: &[u8; 31]) {
        self.down = [0; 32];
        self.down[1..].copy_from_slice(keys);
        self.repeats.clear();
    }
}

/// Whether a release of a key at `release_time`, followed by a press of
/// the same key at `press_time`, is the server's between a held key's
/// repeats, which the client drops: only where the server grants no
/// detectable auto-repeat, under which it sends none.
pub(crate) fn is_repeat_release(
    detectable_auto_repeat: bool,
    release: (u8, u32),
    press: (u8, u32),
) -> bool {
    !detectable_auto_repeat
        && release.0 == press.0
        && press.1.saturating_sub(release.1) <= REPEAT_RELEASE_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENTER: u8 = 36;
    const A: u8 = 38;

    #[test]
    fn a_press_while_the_key_is_down_is_a_repeat() {
        let mut keys = HeldKeys::default();
        assert!(!keys.heard_press(ENTER, 100), "the press");
        assert!(keys.heard_press(ENTER, 760), "its first repeat");
        assert!(keys.heard_press(ENTER, 800), "and the next");
        assert!(!keys.heard_press(A, 810), "another key's press");
        keys.heard_release(ENTER);
        assert!(!keys.is_down(ENTER));
        assert!(keys.is_down(A));
        assert!(!keys.heard_press(ENTER, 900), "a press after the release");
        assert!(keys.take_repeat(ENTER, 760));
        assert!(keys.take_repeat(ENTER, 800));
        assert!(!keys.take_repeat(ENTER, 900), "not a repeat");
        assert!(!keys.take_repeat(ENTER, 100), "nor the first press");
    }

    #[test]
    fn a_repeat_is_known_when_it_is_handled_after_others() {
        let mut keys = HeldKeys::default();
        keys.heard_press(ENTER, 100);
        keys.heard_press(ENTER, 760);
        keys.heard_press(ENTER, 800);
        keys.heard_press(ENTER, 840);
        // the input method kept the repeat at 760 for itself
        assert!(keys.take_repeat(ENTER, 800));
        assert!(!keys.take_repeat(ENTER, 760), "gone with those before");
        assert!(keys.take_repeat(ENTER, 840));
        // a key held for long keeps only the last repeats
        for t in 0..100 {
            keys.heard_press(ENTER, 1000 + t);
        }
        assert!(!keys.take_repeat(ENTER, 1000));
        assert!(keys.take_repeat(ENTER, 1099));
    }

    #[test]
    fn no_key_is_down_once_the_window_lost_the_keyboard() {
        let mut keys = HeldKeys::default();
        keys.heard_press(ENTER, 100);
        keys.heard_press(ENTER, 760);
        keys.let_go();
        assert!(!keys.is_down(ENTER));
        assert!(!keys.take_repeat(ENTER, 760));
        assert!(!keys.heard_press(ENTER, 2000), "a press of its own");
        assert!(!keys.heard_press(255, 2010));
        assert!(keys.heard_press(255, 2050), "the last keycode");
    }

    /// `KeymapNotify`'s keys with the keycodes in `down` set.
    fn keymap(down: &[u8]) -> [u8; 31] {
        let mut keys = [0; 31];
        for &keycode in down {
            keys[usize::from(keycode / 8) - 1] |= 1 << (keycode % 8);
        }
        keys
    }

    #[test]
    fn a_key_held_when_the_keyboard_came_has_its_presses_known_for_repeats() {
        let mut keys = HeldKeys::default();
        // Return was pressed in another window, a file chooser, and is held
        // as the keyboard comes to this one
        keys.heard_keymap(&keymap(&[ENTER, 255, 8]));
        assert!(keys.is_down(ENTER));
        assert!(keys.is_down(255), "the last keycode");
        assert!(keys.is_down(8), "the first the server says");
        assert!(!keys.is_down(A));
        assert!(keys.heard_press(ENTER, 5000), "its first repeat here");
        assert!(keys.take_repeat(ENTER, 5000));
        assert!(!keys.heard_press(A, 5010), "another key's press");
        keys.heard_release(ENTER);
        assert!(
            !keys.heard_press(ENTER, 6000),
            "pressed again after the release"
        );
    }

    #[test]
    fn the_keymap_replaces_what_was_heard() {
        let mut keys = HeldKeys::default();
        // a release the window never heard left the bit set, and a repeat
        // is kept that was heard before the keyboard left
        keys.heard_press(ENTER, 100);
        keys.heard_press(ENTER, 760);
        keys.heard_keymap(&keymap(&[A]));
        assert!(!keys.is_down(ENTER), "a stale bit is cleared");
        assert!(keys.is_down(A));
        assert!(
            !keys.take_repeat(ENTER, 760),
            "the repeats kept are from before"
        );
        assert!(!keys.heard_press(ENTER, 900), "so a press of it is its own");
    }

    #[test]
    fn a_release_just_before_the_press_is_the_servers_without_detectable_repeat() {
        assert!(is_repeat_release(false, (ENTER, 100), (ENTER, 100)));
        assert!(is_repeat_release(false, (ENTER, 100), (ENTER, 120)));
        assert!(
            !is_repeat_release(false, (ENTER, 100), (ENTER, 121)),
            "a hand's"
        );
        assert!(
            !is_repeat_release(false, (ENTER, 100), (A, 100)),
            "another key"
        );
        assert!(
            !is_repeat_release(true, (ENTER, 100), (ENTER, 100)),
            "with detectable auto-repeat every release is a release"
        );
    }
}
