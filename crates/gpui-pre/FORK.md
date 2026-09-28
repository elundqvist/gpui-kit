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
