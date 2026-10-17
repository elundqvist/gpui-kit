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

The files changed from the published crate, as the Apache License 2.0
asks (section 4(b)); a source file says so at its top:

- `src/cosmic_text_system.rs`: an emoji in a font that has none was
  drawn from Noto Sans Symbols2, a monochrome outline in the text's
  colour, where the machine has Noto Color Emoji, and a keycap such as
  `1️⃣` as an empty box (elundqvist/kvist#329). With no fallback chain
  of the caller's, such an emoji went down cosmic-text's own list,
  which names Symbols2 before Noto Color Emoji; and a chain that named
  Noto Color Emoji took the font out of the database instead, since
  `load_family` removes every face of a family it loads that has no
  `m`. A face that is a known emoji font or has a colour table (`CBDT`,
  `sbix`, `COLR` or `SVG `) is kept now. A grapheme that asks for
  emoji presentation, and that neither the run's font nor its chain
  has, is laid out in the colour emoji font, matched at that face's
  own weight and style (`compute_emoji_run_spans`). It asks by its
  first code point's `Emoji_Presentation`, or, after an emoji, by VS16
  or by being one of UTS #51's sequences, a skin tone, a ZWJ, the
  keycap or a tag after the first code point, as Chromium lays them
  out; not by VS15. VS16 and a sequence pass over the run's font,
  whose glyph would be the text presentation. A symbol of text
  presentation, `↔` or `☺`, still goes down cosmic-text's list. The
  emoji font is the first face in the database, in the order
  fontconfig's 60-generic.conf prefers the families for `emoji` (a
  user's fontconfig setting is not read), of Noto Color Emoji, Apple
  Color Emoji, Segoe UI Emoji, Twemoji Mozilla and JoyPixels by their
  PostScript names (`EMOJI_FONTS`) whose colour glyphs swash draws:
  bitmaps, or `COLR` version 0 records, so not a COLRv1 build nor the
  SVG families of that list. It is found without `load_family`, which
  would take a face it does not want out of the database, once and
  again after `add_fonts`, and only for a line with an emoji in it.
  The same test has a glyph drawn as a colour sprite
  (`is_emoji_font`, in place of `check_is_known_emoji_font`, which
  knew Noto Color Emoji alone); a glyph of such a font with no colour
  falls through to its outline, which is given to the colour atlas at
  four bytes a pixel, not one; and the upstream hack that leaves glyph
  3 out is kept to Noto Color Emoji, for which it was written. And a
  line has the ascent and descent of the fonts its runs asked for
  whenever a glyph came from another font, so a line with an emoji or
  a symbol of cosmic-text's list has the baseline of one without: in a
  code editor of fixed rows, the text of a line with a colour emoji
  stood a pixel above its neighbours', with Symbols2's cat two, and a
  line of an emoji alone had the emoji font's baseline. Tests of the
  spans, of the gate and of the colour tables, of an outline drawn as
  an emoji, and, where the machine has the fonts, of a cat in a line
  of IBM Plex Sans laid out and rasterised in colour from Noto Color
  Emoji, of sequences without VS16 laid out whole from it, of the
  baseline of a line with an emoji or a fallback, of the font kept
  when a chain names it, of the font found when it is added after a
  cat was laid out from Symbols2, and of a family of the list drawn in
  no colour neither taken nor removed; all but the last of those on
  the machine's fonts, and the outline's, fail on the published crate.
- `Cargo.toml` and `Cargo.lock`: `unicode-properties`, which the lock
  already holds for rustybuzz, with its `emoji` tables, for
  `Emoji_Presentation`.
- `Cargo.toml` and `Cargo.lock`: `naga`, at the version wgpu already
  brings, as a dev-dependency with `wgsl-in`. The renderer's tests
  validate its shaders with it, and the published manifest, which
  leaves out zed's dev-dependencies, did not let them build.

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
