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

Each change is listed here by file; a source file changed or added
says so at its top. The crate is under the MIT licence, its `LICENSE`
beside this file.

- `src/ctext.rs` (added), `src/client.rs`, `src/lib.rs` and
  `Cargo.toml`: the text an input method draws as its preedit, commits,
  or hands back as an input context is reset is decoded from
  COMPOUND_TEXT whatever it holds (`compound_text_to_utf8_lossy`), where
  the client called `.expect("Encoding Error")` on xim-ctext 0.3's
  decoder, and an application panicked as soon as a field had a
  preedit under ibus with cangjie5 (elundqvist/kvist#455): ibus encodes
  with libX11, which writes a letter of Latin-1 as one byte in the right
  half, and Korean and Chinese as KS C 5601 and GB 2312, and xim-ctext
  0.3 decodes only a whole string of UTF-8 or of JIS X 0208. The
  decoder reads ISO 2022 as compound text has it, a side at a time:
  ASCII and JIS X 0201 in GL, Latin-1 and the other parts of ISO 8859
  and TIS 620 in GR, GB 2312, JIS X 0208, JIS X 0212 and KS C 5601 in
  either, UTF-8 segments, and extended segments of Big5, GBK, GB 18030
  and KOI8, by encoding_rs (which xim-ctext already built on, so no
  crate is added to a lock). A byte or a charset it has no table for,
  CNS 11643 among them, is U+FFFD, and a sequence it does not know,
  the direction of the text among them, is passed over: nothing fails.
  A string with no escape in it that is UTF-8 is still read as UTF-8,
  as xim-ctext 0.3 read every one with no escape, for an input method
  that sends UTF-8 as it is. xim-ctext 0.4.1, the newest, decodes more
  but still gives errors (an unknown set, a UTF-8 segment that is not
  UTF-8), overflows on GB 2312 beside a byte of GR, and xim 0.5 still
  unwraps its result. Tests of what libX11 and ibus write, of segments
  of several charsets, of extended segments, of what does not decode,
  and of a preedit, a commit and a reset that reach the handler
  decoded.

  The client's `handle_reset_ic` hands back the handler's result, which
  it dropped, and `build_ic_attributes` and `build_im_attributes` name
  the lifetime of the builder they return: today's rustc warned of
  both, and the crate builds as a path dependency of gpui-pre-linux,
  whose warnings an application patching it in by path is shown.
- `src/transport.rs` (added), `src/x11rb.rs`, `src/xlib.rs` and
  `src/lib.rs`: a request longer than the 20 bytes of a ClientMessage
  goes in a property of the other side's window, as one at or over the
  TRANSPORT_MAX the server announced already went (`transport`), where
  one between 21 bytes and a limit above 20 ended the application on
  `todo!("multi-CM")` (elundqvist/kvist#473). The protocol's multi-CM,
  a request in several ClientMessages, is still not sent: IMdkit and
  xcb-imdkit, which ibus and fcitx build their servers on, read a
  property for any request whatever limit they announce, and ibus and
  fcitx announce none above 20. Tests of where a request of each length
  goes, and of the requests the client sends a server with a limit of
  4096 (an open, its encoding, the input context gpui-pre-linux
  creates and the spot it moves, a sync's reply), through
  `client::tests`' client that keeps what it is asked to send.

## Tests

The crate is not a member of the fork's workspace (the root
`Cargo.toml` excludes it) and builds with the lock it was published
with. From this directory:

    cargo test --locked --lib --features client
