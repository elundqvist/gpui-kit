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
`--type-list` offers the types in `XdndTypeList`.

## Tests

The crate is not a member of the fork's workspace (the root
`Cargo.toml` excludes it) and builds with the lock it was published
with. From this directory:

    cargo test --locked --lib

It builds on gpui-pre and the other gpui-pre-* crates from crates.io,
at the versions in its own `Cargo.lock`, not on `crates/gpui-pre`, so
its tests never run on that crate's fixes. An application gets both
through `[patch.crates-io]`, as Kvist does.
