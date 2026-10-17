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
through `[patch.crates-io]`, as Kvist does.
