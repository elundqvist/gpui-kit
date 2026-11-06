# cosmic-text in this fork

This crate is cosmic-text 0.19.0 as crates.io serves it, pop-os's
shaper and line layout, which gpui-pre-wgpu's text system lays out and
shapes every line of text with on Linux. It is carried in
elundqvist/gpui-kit, as `crates/zed-xim` is, so that fixes to it can
sit on top of it. The commit that brought it in, "chore: vendor
cosmic-text 0.19.0 as published", is the published crate file for
file, with its normalized Cargo.toml, the Cargo.toml.orig beside it,
the lock it was published with, its test fonts and images and its
licences, less what the package holds of the repository it came from:
cargo's `.cargo_vcs_info.json`, its `.github` workflows, its
`.gitignore`, and its `.gitattributes`, which keeps the fonts and
images in Git LFS, which this repository does not use. Each change
since is a commit of its own.

Name and version stay cosmic-text 0.19.0. gpui-pre-wgpu takes it by
path (`path = "../cosmic-text"` beside its version), so an application
that patches gpui-pre-wgpu in from this repository takes this crate
with it, from the same commit, and needs no patch of its own for it.

## What is changed

Each change is listed here by file; a source file changed says so at
its top. The crate is under the MIT or Apache 2.0 licence, at the
user's choice, its `LICENSE-MIT` and `LICENSE-APACHE` beside this file.

## Tests

The crate is not a member of the fork's workspace (the root
`Cargo.toml` excludes it) and builds with the lock it was published
with. From this directory:

    cargo test --locked --lib
