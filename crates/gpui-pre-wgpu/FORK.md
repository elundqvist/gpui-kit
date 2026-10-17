# gpui-pre-wgpu in this fork

This crate is gpui-pre-wgpu 0.3.3 as crates.io serves it, zed's
`gpui_wgpu` at zed@5b055fa: gpui's wgpu renderer and the cosmic-text
text system the Linux platform shapes and rasterises text with. It is
carried in elundqvist/gpui-kit, as `crates/gpui-pre` and
`crates/gpui-pre-linux` are, so that fixes to it can sit on top of it
until they are upstream in zed-industries/zed. The commit that brought
it in, "chore: vendor gpui-pre-wgpu 0.3.3 as published", is the
published crate file for file; each change since is a commit of its
own.

Name and version stay gpui-pre-wgpu 0.3.3, so gpui-pre-linux 0.3.3,
which asks for `=0.3.3`, takes it when an application patches it in.

## What is changed

None yet. Each file changed from the published crate will say so at
its top and be listed here, as the Apache License 2.0 asks (section
4(b)).

## Tests

The crate is not a member of the fork's workspace (the root
`Cargo.toml` excludes it) and builds with the lock it was published
with. From this directory:

    cargo test --locked --lib

It builds on gpui-pre and the other gpui-pre-* crates from crates.io,
at the versions in its own `Cargo.lock`, not on `crates/gpui-pre`, so
its tests never run on that crate's fixes. An application gets both
through `[patch.crates-io]`, as Kvist does. The text system's tests
include zed's IBM Plex Sans from `../../../assets/fonts`, which the
fork keeps in `assets/fonts` with its licence.
