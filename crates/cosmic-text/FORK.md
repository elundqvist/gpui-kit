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

- `src/shape.rs`: the glyphs a fallback font has for the clusters a
  run's font lacks are put in their place in one pass (`take_fallback`,
  elundqvist/kvist#626). `shape_run` did it a cluster at a time, each
  looked for from the run's start, its glyphs removed and inserted one
  by one, and the clusters still missing searched as a list, so a word
  of a script the asked font lacks, one run to the shaper, took time in
  the square of its length: in gpui's text system, release build, a
  word of 16 000 Hebrew letters in JetBrains Mono, drawn from a
  fallback font, took 0.38 s, 32 000 1.5 s and 80 000 10.2 s, Arabic and
  Devanagari the same, against 7 ms for 80 000 Latin letters; now
  24 ms. A run no font has took time in the square of its length too,
  each fallback tried searching the clusters it lacks as a list. The
  clusters taken, where they go, the order of the glyphs and what is
  left missing are as they were: tests merge Hebrew, Arabic with its
  marks, the two in one run and mixed with Latin in fonts that lack
  them and then in fonts that have them, and runs the fonts cut into
  clusters at other places, both ways, the published merge kept in the
  test, and one shapes words of 20 000 and 80 000 Hebrew letters in
  Noto Sans, the longer in less than eight times the time.

## Tests

The crate is not a member of the fork's workspace (the root
`Cargo.toml` excludes it) and builds with the lock it was published
with. From this directory:

    cargo test --locked --lib
