# gpui-pre in this fork

This crate is gpui-pre 0.3.3 as crates.io serves it, zed's gpui at
zed@5b055fa, carried in elundqvist/gpui-kit so that fixes to gpui itself
can sit on top of it until they are upstream in zed-industries/zed. The
commit that brought it in, "chore: vendor gpui-pre 0.3.3 as published",
is the published crate file for file; each change since is a commit of
its own.

## What is changed

The files changed from the published crate, as the Apache License 2.0
asks (section 4(b)); each says so at its top:

- `src/elements/div.rs`: a mouse-down ends a tooltip still waiting to
  show, and the check the window makes of a shown tooltip before each
  frame tests the hitbox its element inserted in that frame rather than
  its bounds, so a tooltip whose element something now covers, like a
  modal's backdrop, hides, a hoverable one at once (elundqvist/kvist#115).
  Tests of both.
- `src/elements/text.rs`: `InteractiveText` answers that check in the
  form `div.rs` asks for now; its tooltips still test its bounds.
- `src/window.rs`: `HitboxId::is_hovered_during_prepaint` and
  `HitboxId::is_covered_during_prepaint`, which the check uses.
- `src/text_system/line_wrapper.rs`: `+` is a word character, as `-`
  and `=` are, so a key (`Ctrl+Space`, `Shift+F7`) or a name (`C++`) is
  wrapped whole, before it, where it was wrapped at its plus, "Ctrl" at
  the end of a line and "+Space" at the start of the next
  (elundqvist/kvist#386). A plus with spaces around it is still a place
  to break. Tests of both.
- `src/window.rs`: `Window::hold_input`, `Window::release_input` and
  `Window::input_held` (elundqvist/kvist#401). While a hold
  (`InputHold`) is out, the keys that come to the window, a key down, a
  key up or a change of the modifiers, wait, and so do the mouse's
  presses, releases, wheel and drops, the text the platform hands the
  input handler and what an input method composes and commits; the
  pointer's moves with no button down pass while no click waits, so
  that what is under it lights up. Once the last hold is released they
  are dispatched in the order they came, one an effect cycle, as the
  platform hands each in an update of its own, so that what a key's
  action emits is heard, and the focus it moves has moved, before the
  key after it; a key down that no handler stopped gives its character
  to the input handler next, as the Linux clients do. A key that holds
  the input again, a second paste, leaves the rest waiting behind it; a
  hold dropped is released a turn of the foreground executor later. An
  editor whose paste waits for another program's clipboard holds its
  window's input meanwhile, so that what is typed and clicked after the
  paste lands after it. Tests of the order, of what a key sets off
  before the key after it, of a click among the keys and a move that
  passes, of a key that holds again, and of nested and dropped holds.
- `src/platform.rs`: `PlatformInputHandler` hands the window the text,
  the composition and its end while the window's input is held, and
  the handler what waited once it is released.
- `src/window.rs`: `Window::hold_input_with_keys` (elundqvist/kvist#435),
  a hold that is handed each key pressed while it is out, as the key
  comes: one it takes goes no further, and the rest wait as under any
  hold. A paste that waits for a program that does not answer is given
  up with Escape that way, where nothing reached the app until the read
  gave up four seconds later. A hold released as it takes a key is
  given up, and the keys held behind it like that one, or like the key
  being dispatched as it was taken, go with it and their key ups: an
  Escape pressed before the paste could be given up came after it and
  closed the dialog it was asked for in, and a second Ctrl+V waited
  again. The hold most recently taken is asked first, and a hold
  released looks at no more keys. Tests of a key taken and the rest let
  go, of a key taken under another hold, and of the like keys dropped.
- `src/text_system/line_layout.rs`: `LineLayout::closest_index_for_x`
  goes to the nearer edge of the cluster under the pointer
  (elundqvist/kvist#474). It took each glyph's start for a place to
  stand, and the glyphs of one cluster share its index: a grapheme
  the font draws as two glyphs, 👧‍👦 in Noto Color Emoji, which has no
  glyph of its own for it, is one cluster of two, and a click on the
  left of its second glyph, its right half, went to its start. The last
  cluster ends at the line's width, so a click on its left half goes
  before it, where any click past its start went to the line's end.
  Tests of both.

## Tests

The crate is not a member of the fork's workspace (the root
`Cargo.toml` excludes it) and builds with the lock it was published
with. From this directory:

    cargo test --locked --features test-support --lib --tests

`test-support` brings in what the tests use (backtrace, proptest,
`FakeHttpClient`). `--lib --tests` leaves out the published examples,
whose dev-dependencies the published manifest does not list. The
svg_renderer tests include two of zed's fonts from
`../../../assets/fonts`, which the fork keeps in `assets/fonts` with
their licences.

The fork's own crates do not build on this crate: they take gpui-pre
from crates.io, at the version in the fork's `Cargo.lock`, so their
tests never run on these fixes. An application gets it through
`[patch.crates-io]`, as Kvist does.
