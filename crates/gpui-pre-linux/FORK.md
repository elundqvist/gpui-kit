# gpui-pre-linux in this fork

This crate is gpui-pre-linux 0.3.3 as crates.io serves it, zed's
`gpui_linux` at zed@5b055fa: gpui's Linux platform, its X11 and Wayland
clients. It is carried in elundqvist/gpui-kit, as `crates/gpui-pre` is,
so that fixes to the platform can sit on top of it until they are
upstream in zed-industries/zed. The commit that brought it in, "chore:
vendor gpui-pre-linux 0.3.3 as published", is the published crate file
for file; each change since is a commit of its own.

Name and version stay gpui-pre-linux 0.3.3, so gpui-pre-platform 0.3.3,
which asks for `=0.3.3`, takes it when an application patches it in.

## What is changed

The files changed from the published crate, as the Apache License 2.0
asks (section 4(b)); each says so at its top:

- `src/linux/x11/client.rs`: the X11 client kept one XDND drag for all
  its windows, set its type at an `XdndEnter` only when the drag offered
  one gpui takes, and cleared it only at an `XdndLeave` or `XdndDrop`. A
  drag that offered none, or an `XdndPosition` with no `XdndEnter`
  before it, had its selection asked for in the type 0, which the X
  server answers with BadAtom ("Failed to convert selection for drag and
  drop", logged as an error), and was told the drop was accepted
  (elundqvist/kvist#218). Now each `XdndEnter` begins a drag of its own
  over its window (`XdndDrags`); a drag that offers no type gpui takes,
  or a message about a drag the window never saw enter, is answered
  `XdndStatus` not accepted and `XdndFinished` not accepted, its data is
  never asked for, gpui hears nothing of it, and nothing is logged. An
  `XdndEnter` that names no source begins no drag and has no type list
  read, where window 0's was, a BadWindow logged as a warning. One that
  takes the place of a drag gpui heard of, whose source went without an
  `XdndLeave`, tells gpui that drag has gone, as the leave would have.
  A drag's data is asked for once, at its first position, and an answer
  is taken only as the one to that request, by its type and time: a
  late answer for a drag that has gone is no other drag's; a request the
  source refused gives gpui nothing, where the client read the previous
  drag's data, still in the property; and the data is read whole and the
  property deleted, as ICCCM has the requestor do, where the first 4 KB
  were read and the property kept. The
  answers go to the source the message names, where they went to the
  last `XdndEnter`'s, which is window 0, the pointer's window, when
  there was none. Tests of the drag state.

  A line of a drag's data that names no file, a line of text, a comment
  or a URL of another scheme, is passed over and counted at debug level
  (`xdnd_paths`): each was logged as an error, "relative URL without a
  base" or "Failed turn … into a file path", so a drag of text, which
  gpui takes as well, or of a link filled the log with them. Tested
  with a listener for what is logged.

  A window's `XdndAware` is of the type `ATOM`, as the XDND protocol
  has it, where it had the type of an atom interned by the name
  "XA_ATOM" (`xprop` showed `XdndAware(XA_ATOM)`), which a source that
  reads the property by its type would not find.

  A drop is finished once its data has come (elundqvist/kvist#380): the
  client answered every drop `XdndFinished` at once and told gpui it
  was dropped, so data that came after the drop, from a source slow to
  answer, was no drag's and gpui had nothing to drop. A drop before
  the data now waits for it (`XdndDropped::Waiting`), and the source is
  told the drop is done, and gpui handed the files and told of the
  drop, once it has come (`XdndCame`); a drop whose data the source
  would not give, that could not be read, or that names no file gpui
  can open, is finished not accepted and gpui told the drag has gone,
  where it was accepted and dropped with nothing, and the drag is
  refused at its positions from then on. A drop still waiting for its
  data when another drag enters in its place is finished not accepted.
  And data too large for one property, which the source
  hands over in pieces (ICCCM's INCR), is gathered piece by piece from
  the property's new values until the empty one (`XdndDrags::piece`),
  where the answer was read as the data, its size in four bytes, which
  named no file. Tests of the drop before and after the data, of the
  pieces, of data that names no file, of a drop another drag takes the
  place of, and of a drop refused.
- `src/linux/x11/window.rs`: the atoms have `INCR`, which the client
  reads a drag's data in pieces by.
- `src/linux/x11/clipboard.rs`: reading the clipboard slept a
  millisecond each time it found no event, so a selection handed over
  in pieces (ICCCM's INCR), 4 KB a piece as Tk and others hand them,
  took a millisecond and more a piece on the drawing thread: 0.16 to
  0.44 s to paste 1.4 MB from another program (elundqvist/kvist#384).
  It waits on the connection's socket now (`wait_for_server`), and
  wakes as the next piece comes.

  The clipboard another program owns is read off the drawing thread
  (elundqvist/kvist#401): the read, on a connection of its own, held
  the frame about 35 ms for a paste of 1.4 MB while the owner handed it
  over. `ClipboardReader` reads it from another thread, and is not
  counted among the clipboard's owners while it reads, so that the app
  that quits meanwhile still hands the clipboard manager its copy. An
  owner that does not answer for its targets in the four seconds a read
  waits is given up (`Error::TimedOut`): the reader went on to ask it
  for each of fifteen formats, four seconds apiece, and a paste waited a
  minute.

  A selection the app owns that is larger than the server's largest
  request, less room for the request's fields (262,040 bytes as servers
  usually have it), is handed over in pieces of that size, as ICCCM
  2.7.2 has an owner do (elundqvist/kvist#424): it was set whole, and a
  requestor that reads a property with a bound of its own, as Tk does,
  could not take it, "selection property too large". The requestor is
  answered `INCR` and the size, its property changes are listened to,
  and each time it deletes the property the next piece goes in, an
  empty one ending it (`IncrSend`); a requestor that takes no piece for
  10 s is given up. Handed to the clipboard manager as the app quits,
  the contents are written once the last piece has gone, and the app
  waits on while the manager takes them: a manager that says it is done
  first, as KDE's does, had the window go under its first piece. A
  read connects to the display of the clipboard it reads. Tests of the
  pieces, of a requestor that reads 1.4 MB as Tk does, of a manager that
  answers first, and of a read from an owner that does not answer, on
  an X server of their own: an Xvfb the test starts (`-displayfd`), or
  the one `GPUI_X11_TEST_DISPLAY` names, never the desktop's, whose
  clipboard they would take; with neither they are skipped.

  Text the app owns is handed over as each target the owner lists
  (`served_as`): the two MIME types of UTF-8 were listed and refused,
  and `STRING`, the Latin-1 that older programs ask for, a character
  outside it as `?`, was not there: Tk asked for it by name said `form
  "STRING" not defined`. In pieces too where it is large. Tests of each
  target, on an X server of its own.
- `src/linux/x11/client.rs` and `src/linux/platform.rs`:
  `read_from_clipboard_async` reads the clipboard on a thread of its
  own where another program owns it, not on a worker of the background
  executor, which a read that waits on its owner would take, and gives
  the app's own copy at once (elundqvist/kvist#401); the Wayland and
  headless clients read it as before.
- `src/linux/x11/client.rs` and `src/linux/x11/window.rs`: a
  composition the window loses the keyboard in, one the client knows of
  from its input method or a dead key, is deleted by the marked text
  replaced with nothing and the mark ended, handed over behind what came
  before them (elundqvist/kvist#435): the marked range was asked of the
  input handler as the focus went, and where the window's input was
  held behind a paste waiting for the clipboard (the fork's gpui-pre),
  a composition begun meanwhile was not marked yet, so the deletion took
  the range marked before it, other text or none. With no composition
  known, what the handler has marked goes, as before. The handler is to
  take an empty mark where nothing is marked as no edit, as the fork's
  gpui-base input does: the focus may have moved to a field whose
  selection it would delete. Tests of a field whose input is held, and
  of one with a selection and nothing marked.
- `src/linux/x11/client.rs`: a read of the clipboard whose owner did not
  answer in time, or stopped handing its text over, gives an error,
  `ClipboardReadError::Unavailable` (`read_for_paste`,
  elundqvist/kvist#435), where it gave nothing, as an empty clipboard
  does, and a paste that waited four seconds ended with nothing to say
  why. A test of what each read gives.
- `src/linux/x11/client.rs`: the cursor hidden over a window while it is
  typed into is given back only to a window still there, and is hidden
  and set only over one (`still_there`, elundqvist/kvist#444). A
  window's drop sends DestroyWindow at once and the client lets go of
  the window later (`drop_window`), so a window closed with its cursor
  hidden had the next event of another window's, the pointer coming
  onto the window under it or the focus moving on, ask the server to
  restore the cursor of a window it no longer had: BadWindow, logged as
  an error ("Failed to restore cursor style after hide"); a key or a
  cursor style in between would have asked the same of hiding or
  setting it. A test of which windows are asked about; the requests
  themselves need gpui's own windows on a connection, and were checked
  under Kvist's harness on Xvfb.
- `src/linux/x11/client.rs` and `src/linux/x11/xim_handler.rs`: a click
  while an input method composes keeps the composition as text where it
  was drawn, in the window it is in, and drops what the input method
  commits for it before the next key goes to it (`keep_composition`,
  elundqvist/kvist#455): ibus answers the reset the click asks for with
  an empty preedit and a commit of the composition, which went in again
  where the click had put the caret, so a 明 typed and a click gave two.
  Chromium's fields, VS Code's among them, keep a composition a click
  ends so and pass over that commit. A composition under way as the
  input method's server goes is kept as text likewise, where the next
  key typed replaced it, and the keys sent to the server since it last
  said anything, which went to its window after it was destroyed, are
  handled as if there were no input method, where the first was lost.
  The fork's gpui-base input takes the end of a mark with its text kept
  as an edit, as a commit is. Tests of what the handler hands on.
- `src/linux/platform.rs`, `src/linux/x11/client.rs`,
  `src/linux/wayland/client.rs` and the two clients' `window.rs`: a
  dead key's or the Compose key's sequence is followed by one function
  for both clients (`compose_key`, elundqvist/kvist#482). A key with
  Ctrl, Alt or Super held is not fed to it, and the sequence goes on
  past the key, as GTK's input method passes a key with Control or Mod1
  by: a key gpui types no text for was fed and composed, Ctrl+K after a
  dead key a ḱ, so the shortcut never ran, nothing was typed, and the ´
  stayed marked with no sequence behind it. A sequence whose ´ the
  focused input had as a shortcut passed it by, and has no more, ends
  before the key after it (`has_marked_text` asks the input): the
  shortcut's edit took it, an undo or a paste, or the focus it moved,
  as GTK's text widgets reset their input method at such an edit, where
  the sequence went on unseen, the next letter typed with an accent
  drawn nowhere and an Escape swallowed with nothing to cancel. The
  input is asked only after a shortcut, so a ´ the window holds back
  behind a paste waiting for the clipboard is not taken for one let go
  of. Escape in a sequence ends it with nothing left where it was
  marked, and goes no further, as GTK's and ibus's do, where the
  sequence cancelled committed the ´ as text and the Escape went on to
  close what it closes. A dead key that begins a sequence after one it
  cancelled is known to be marked, where the X11 client marked it and
  kept none; and a click ends the sequence whether or not anything is
  marked for it (`end_compose`), where the Compose key's, which marks
  nothing, went on past it. A mark a key or a click ends is ended with
  what the input has marked kept where it is drawn, in the window that
  has the keyboard, as the input method's is at a click: the client
  committed its own copy of the ´, at the caret of an input that had
  let go of it, the palette's field a shortcut had opened over it, or,
  on X11, of the window clicked. Tests of each on a compose table of
  their own, with an input that lets go of its mark.
- `src/linux/platform.rs`: the compose table both clients follow is the
  locale's that `setlocale` takes for the character type, `LC_ALL`'s,
  else `LC_CTYPE`'s, else `LANG`'s, one set empty counting as none
  (`compose_locale`, elundqvist/kvist#498), as xkbcommon asks its
  callers to take it. `LC_CTYPE`'s was read alone, so with `LANG` set
  and no `LC_CTYPE`, as a desktop session often leaves it, the table
  was the C locale's, which xkbcommon takes for en_US.UTF-8's, whatever
  `LANG` named, and a locale `LC_ALL` set was passed over: in Brazilian
  Portuguese a dead ´ and a c gave ć, where the locale's own table has
  ç, as Portugal's has, and Greek, Czech, Finnish, Russian, Serbian,
  Chinese, Japanese, Korean, Khmer and Amharic have tables of their
  own. A locale of another codeset, or of none, as `LANG=en_US` names
  Latin-1, is tried in its UTF-8 form first (`utf8_form`): xkbcommon
  reads tables in UTF-8 alone, logged a dozen errors on the Latin-1
  table at every start, and took the C locale's, which lost Brazilian
  Portuguese's ç in Latin-1. Tests of the order, of the UTF-8 form, and
  of the pt_BR tables, where the system's are as the test takes them
  and no compose file of the user's is read in their place.
- `src/linux/x11/window.rs` and `src/linux/x11/client.rs`: an X11
  window takes the app's zoom over the display's factor
  (`PlatformWindow::set_zoom`, the fork's gpui-pre,
  elundqvist/kvist#563). Its content is laid out and drawn at the
  display's factor times the zoom from the next frame on
  (`scale_factor`, `content_size`), while its bounds, the displays and
  what the window manager is told stay in the display's logical pixels
  (`display_scale_factor`), as before: a window is placed as it was,
  and a resize comes in the display's pixels. Each position of the
  pointer in a window goes by that window's factor
  (`X11WindowStatePtr::scale_factor`), where every window's went by the
  client's one, and the input method's spot too (`update_ime_position`
  takes the factor). The least size the window opened with is told to
  the window manager again at each zoom, in its pixels, the display's
  times the zoom (`set_size_hints`): it was told once in logical pixels
  unscaled, so at a display factor other than 1.0 the window could be
  made smaller than the least it asked for. It is no more than the
  screen either way: a least size past the screen, 640 by 480 at 200 %
  on a screen 900 high, would have the window manager grow the window
  past the screen's bottom. The client keeps the last zoom a window was
  given, which a window opened after it starts at. The windows and the
  server it takes cannot be had in a test; checked under Kvist's harness
  on Xvfb.
- `src/linux/wayland/window.rs` and `src/linux/wayland/client.rs`: a
  Wayland window takes the zoom the same way (elundqvist/kvist#563):
  its content is laid out at the output's scale times the zoom
  (`scale_factor`, `content_size`), its bounds, its buffer and what the
  compositor is told stay in the compositor's logical pixels, a resize
  gpui asks for is in content pixels, the pointer's and a drag's
  positions come divided by the window's zoom
  (`WaylandWindowStatePtr::zoom`) and the input method's rectangle
  multiplied by it, and the least size is told to the compositor again
  in its pixels at each zoom, no more than the output the window is on.
  Written to the X11 window's pattern and built with the `wayland`
  feature, as Kvist builds it; not run on a compositor, there being
  none at hand.
- `Cargo.toml`: an `[[example]]` entry for `examples/xdnd_source.rs`,
  which the fork adds (the published manifest names its targets and
  has no examples). And zed-xim, the XIM client, is taken by path from
  `crates/zed-xim`, where the fork carries it with fixes of its own
  (its FORK.md says what they are), beside the version crates.io
  serves it at; `Cargo.lock` names it with no source. And gpui-pre is
  patched to `crates/gpui-pre`, the fork's, whose `PlatformWindow` has
  `set_zoom` (elundqvist/kvist#563), which the X11 window implements:
  on the published gpui-pre the crate no longer builds. `Cargo.lock`
  names it with no source too. And gpui-pre-wgpu is patched to
  `crates/gpui-pre-wgpu`, the fork's, whose renderer reads a frame back
  (elundqvist/kvist#577), with `image` for the image it returns.
- `src/linux/x11/window.rs`: the X11 window implements
  `PlatformWindow::render_to_image` with its renderer's read back, so an
  app can take a screenshot of its own window, whatever is stacked over
  it (elundqvist/kvist#577); a window whose GPU device was lost says so.
  The Wayland window does not yet, and says it is not implemented.
- `src/linux/x11.rs`, `src/linux/x11/held_keys.rs` and
  `src/linux/x11/client.rs`: a key down on X11 says whether the key is
  held, a press of a key that is down being its repeat
  (`KeyDownEvent::is_held`, elundqvist/kvist#613), where every press was
  one of its own, so that an app can let a held key's repeats go nowhere
  once its press moved the keyboard, as IntelliJ and VS Code do: Return
  that opened a file typed newlines into it. The client asks for XKB's
  detectable auto-repeat, under which a held key sends its press again
  and again and its release once, as it is let go, and drops the release
  before a repeat only where the server does not grant it, as it dropped
  every release within 20 ms of a press of the same key: a script's two
  quick presses were one press held. The keys down are heard as the
  server sends them, before an input method sees them, and the presses
  that were repeats are kept until the client handles them
  (`HeldKeys`), so that a key the input method keeps for itself, its
  press or its release, leaves no key down for good; the window losing
  the keyboard lets every key go, their releases going to the window
  that has it, whether it is still there to handle the event or not.
  The windows select `KEYMAP_STATE`, and the keys down as the server
  has them, which it sends right after each `FocusIn`, replace what was
  heard, so that a key held as the keyboard comes to the window (a file
  chooser closed with Return held) has its repeats known for repeats,
  and a bit left by a release the window never heard goes. Tests of a
  repeat, of one handled after others, of the keys let go, of the
  keymap and of the releases dropped.

## Trying a drag by hand

`examples/xdnd_source.rs` is an XDND drag source, since `xdotool`
cannot be one: given a window id and the types to offer, it enters the
window, moves to its centre, answers the request for the drag's data,
prints whether the window accepted, and leaves or drops. On a display
with a gpui window on it:

    DISPLAY=:99 cargo run --locked --example xdnd_source -- \
        "$(DISPLAY=:99 xdotool search --name <title> | head -1)" image/png
    DISPLAY=:99 cargo run --locked --example xdnd_source -- <window id> \
        text/uri-list --data file:///tmp/x.txt --drop

The first should print `status: not accepted` and the application log
nothing about it; the second `status: accepted`, `selection asked for
in text/uri-list: answered` and `finished: accepted`. `--no-enter`
sends the `XdndPosition` with no `XdndEnter` before it, and
`--type-list` offers the types in `XdndTypeList`. `--late` answers
the request for the data only after the drop, and `--incr <bytes>`
hands the data over in pieces of that many bytes:

    DISPLAY=:99 cargo run --locked --example xdnd_source -- <window id> \
        text/uri-list --data file:///tmp/x.txt --drop --late --incr 8

should print `finished: accepted`, the window having taken the file.

## Tests

The crate is not a member of the fork's workspace (the root
`Cargo.toml` excludes it) and builds with its own lock. From this
directory:

    cargo test --locked --lib

It builds on `crates/gpui-pre` and `crates/gpui-pre-wgpu`, the fork's,
through the `[patch.crates-io]` in its `Cargo.toml` (the X11 window
implements that crate's `PlatformWindow::set_zoom` and
`render_to_image`, the latter with the fork's renderer), and on the
other gpui-pre-* crates from crates.io, at the versions in its
`Cargo.lock`. An application
gets all of them through its own `[patch.crates-io]`, as Kvist does.
