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
- `Cargo.toml`: an `[[example]]` entry for `examples/xdnd_source.rs`,
  which the fork adds (the published manifest names its targets and
  has no examples).

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
`Cargo.toml` excludes it) and builds with the lock it was published
with. From this directory:

    cargo test --locked --lib

It builds on gpui-pre and the other gpui-pre-* crates from crates.io,
at the versions in its own `Cargo.lock`, not on `crates/gpui-pre`, so
its tests never run on that crate's fixes. An application gets both
through `[patch.crates-io]`, as Kvist does.
