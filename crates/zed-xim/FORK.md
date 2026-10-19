# zed-xim in this fork

This crate is zed-xim 0.4.0-zed as crates.io serves it, zed's fork of
Riey's xim-rs (zed-industries/xim-rs@471c10c): the X Input Method
client gpui-pre-linux's X11 client speaks to an input method with,
ibus, fcitx or any other XIM server. It is carried in
elundqvist/gpui-kit, as `crates/gpui-pre-linux` is, so that fixes to
it can sit on top of it. The commit that brought it in, "chore: vendor
zed-xim 0.4.0-zed as published", is the published crate file for file,
less what the package holds of the repository it came from: cargo's
`.cargo_vcs_info.json`, its `.github` workflow, its `.vscode` settings,
and its `.gitignore`, which would leave the lock out. Each change since
is a commit of its own.

Name and version stay zed-xim 0.4.0-zed. gpui-pre-linux takes it by
path (`path = "../zed-xim"` beside its version), so an application
that patches gpui-pre-linux in from this repository takes this crate
with it, from the same commit, and needs no patch of its own for it.

## What is changed

Nothing yet. Each change is listed here by file as it comes; a source
file changed says so at its top. The crate is under the MIT licence,
its `LICENSE` beside this file.

## Tests

The crate is not a member of the fork's workspace (the root
`Cargo.toml` excludes it) and builds with the lock it was published
with. From this directory:

    cargo test --locked --lib --features client
