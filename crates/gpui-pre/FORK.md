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
- `src/text_system/line_wrapper.rs`: `+` is a word character, as `-`
  and `=` are, so a key (`Ctrl+Space`, `Shift+F7`) or a name (`C++`) is
  wrapped whole, before it, where it was wrapped at its plus, "Ctrl" at
  the end of a line and "+Space" at the start of the next
  (elundqvist/kvist#386). A plus with spaces around it is still a place
  to break. Tests of both.
- `src/window.rs`: `Window::hold_input`, `Window::release_input` and
  `Window::input_held` (elundqvist/kvist#401). While a hold
  (`InputHold`) is out, the keys that come to the window, a key down, a
  key up or a change of the modifiers, wait, and so do the mouse's
  presses, releases, wheel and drops, the text the platform hands the
  input handler and what an input method composes and commits; the
  pointer's moves with no button down pass while no click waits, so
  that what is under it lights up. Once the last hold is released they
  are dispatched in the order they came, one an effect cycle, as the
  platform hands each in an update of its own, so that what a key's
  action emits is heard, and the focus it moves has moved, before the
  key after it; a key down that no handler stopped gives its character
  to the input handler next, as the Linux clients do. A key that holds
  the input again, a second paste, leaves the rest waiting behind it; a
  hold dropped is released a turn of the foreground executor later. An
  editor whose paste waits for another program's clipboard holds its
  window's input meanwhile, so that what is typed and clicked after the
  paste lands after it. Tests of the order, of what a key sets off
  before the key after it, of a click among the keys and a move that
  passes, of a key that holds again, and of nested and dropped holds.
- `src/platform.rs`: `PlatformInputHandler` hands the window the text,
  the composition and its end while the window's input is held, and
  the handler what waited once it is released.
- `src/window.rs`: `Window::hold_input_with_keys` (elundqvist/kvist#435),
  a hold that is handed each key pressed while it is out, as the key
  comes: one it takes goes no further, and the rest wait as under any
  hold. A paste that waits for a program that does not answer is given
  up with Escape that way, where nothing reached the app until the read
  gave up four seconds later. A hold released as it takes a key is
  given up, and the keys held behind it like that one, or like the key
  being dispatched as it was taken, go with it and their key ups: an
  Escape pressed before the paste could be given up came after it and
  closed the dialog it was asked for in, and a second Ctrl+V waited
  again. The hold most recently taken is asked first, and a hold
  released looks at no more keys. Tests of a key taken and the rest let
  go, of a key taken under another hold, and of the like keys dropped.
- `src/text_system/line_layout.rs`: `LineLayout::closest_index_for_x`
  goes to the nearer edge of the cluster under the pointer
  (elundqvist/kvist#474). It took each glyph's start for a place to
  stand, and the glyphs of one cluster share its index: a grapheme
  the font draws as two glyphs, 👧‍👦 in Noto Color Emoji, which has no
  glyph of its own for it, is one cluster of two, and a click on the
  left of its second glyph, its right half, went to its start. The last
  cluster ends at the line's width, so a click on its left half goes
  before it, where any click past its start went to the line's end.
  Tests of both.
- `src/text_system/line_wrapper.rs`: `LineWrapper::truncate_wrapped_line`,
  the cut of `line_clamp` with an ellipsis, cuts the text on the rows it
  is drawn in (elundqvist/kvist#485): each paragraph is shaped in its
  runs' fonts and wrapped by the window's own rule, and the last row
  allowed is cut where it and the ellipsis fit as shaped, a cluster at a
  time. It added each character's width shaped on its own, and a
  character the font lacks falls back to a font by the script of the
  characters around it: a lone `。`, of no script, came out half the
  width it is drawn at among Chinese, so the rows it worked out held
  more than the rows drawn, and the cut fell past the last, which the
  clamp then cut mid-row, its ellipsis over the edge. A paragraph is
  shaped only as far as the rows allowed and two more could reach
  (`shape_rows`), and one no wider than a row is not walked for where
  it wraps, so a long text clamped to two rows costs what two rows
  cost. Tests with a text system that draws `。` so, and that counts
  what it shapes.
- `src/elements/text.rs` and `src/text_system.rs`: the text element
  has that cut shape through the window's line layout cache
  (`WindowTextSystem::layout_font_runs`), as it shapes what it draws:
  measured every frame, and several times a frame, the cut shaped the
  text anew each time.
- `src/text_system/line_layout.rs`: `LineLayout::compute_wrap_boundaries`
  is crate-visible, for the cut above.
- `src/text_system/line_wrapper.rs` and `src/text_system/line_layout.rs`:
  no row of wrapped text starts with a closing bracket or quote, a full
  stop, a comma, a colon, a question or exclamation mark, a middle dot,
  an iteration mark, the prolonged sound mark, a small kana or a unit
  after a number, and none ends with an opening bracket or quote or a
  sign before a number (`LineWrapper::may_break_between`,
  elundqvist/kvist#497): Chinese and Japanese, which are not spaced,
  could break before any character, so a row started with `，` or `。`
  and one ended with `「`. The character before goes to the next row
  with the mark, as UAX #14 and JIS X 4051 have it and browsers and VS
  Code wrap; the sets are VS Code's, with JIS X 4051's other brackets
  and marks, and ASCII is left as `is_word_char` has it, but for `(`,
  `[` and `{`, which end no row either, as in VS Code. A word of Latin
  letters may begin a row after a Chinese comma or closing bracket, as
  after a space (`is_closing_mark`), so `bug，update` is not one word.
  `wrap_line` and the window's own wrap (`compute_wrap_boundaries`) keep
  the rules alike, `wrap_line` by the character before even where it
  wraps a row, where its `prev_c` stays as it was. A row with nowhere
  else to break still breaks. Tests of both.
- `src/text_system/line_wrapper.rs`: `wrap_line`, which gpui-base's
  editor and its text with images wrap by, and `truncate_line`, at the
  end, the start or the middle, measure a closing mark with the
  character before it and an opening mark with the one after it
  (`width_beside`, elundqvist/kvist#497), as they are shaped together
  where they are drawn: a lone `、` or `。`, which a font is found for by
  the characters it is shaped with, came out half the width it is drawn
  at among Chinese, so the rows `wrap_line` made were wider than the
  width drawn, and `truncate_line` kept more than fits and drew its
  ellipsis past the edge, or none at all (`，`, a full-width form, is as
  wide alone). The mark is shaped beside one character of its
  neighbour's script, Han, kana or Hangul (`of_its_script`), so once a
  script and a wrapper, not once a neighbour. Tests with the text system
  above, which draws `、`, `。` and the brackets so, and of what is shaped.
- `src/platform.rs` and `src/window.rs`: `PlatformWindow::set_zoom`, a
  no-op unless the platform has one, and `Window::set_zoom`, the app's
  zoom over the display's scale factor (elundqvist/kvist#563), as a
  browser zooms a page. gpui lays a window out in logical pixels and
  draws them through the window's scale factor, the display's, read
  once; the zoom is that factor changed while the app runs. The window
  hands it to the platform window and takes the factor and the content
  size back (`bounds_changed`), so from the next frame on it is laid out
  at the display's factor times the zoom, the glyphs and the SVGs
  rasterised anew at it, which the atlas keys by the factor already, and
  nothing scaled as a bitmap; the mouse and the input method's positions
  come divided by it. The window's bounds and the displays' stay the
  display's, so windows are placed as before. Each window has its own,
  and an app zooms all of its alike. A zoom the window has already, or
  one the platform has no zoom for, asks for no frame: the window is laid
  out again only where its factor moved, so a key held at the end of an
  app's range does not have every window redrawn.
- `src/platform/test/window.rs`: the test window takes a zoom over its
  factor of 2.0, its content size its bounds over the zoom, as the X11
  window's is. A test of a frame's layout at 1.5, 1.5 times the one at
  1.0, and back, with a root that fills the window drawn as wide as the
  window at either, and no frame asked for a zoom the window has.
- `src/platform.rs` and `src/window.rs`: `PlatformWindow::render_to_image`
  and `Window::render_to_image` are there in every build, not in a
  `test-support` one alone, so an app can take a screenshot of its own
  window (elundqvist/kvist#577): the frame last presented, drawn again
  into a texture and read back, with nothing of a window above it. A
  platform with no read back still says so, as before.
- `src/window.rs` and `src/platform.rs`: a frame, its layout and its
  paint, its present, each input and each action handler are `tracing`
  spans of target `gpui::perf` at trace level (elundqvist/kvist#578),
  for an app that times what its drawing thread does: `frame` (its
  `window`) around a draw (`Window::draw`), wherever it is made: in the
  frame request, or ahead of an input that finds the window dirty, which
  puts it inside that input's span; `layout` from the views' render
  through their prepaint and the tooltip's, `paint` from there to the
  scene's end, both inside the frame; `present`, a span of its own after
  the frame or alone, where the renderer's submit and the swap chain's
  wait are; `input` (its `kind`, as `PlatformInput::kind_name` names it,
  `text` for text the input handler is handed and `compose` for a
  composition) around an input's dispatch and the effects it flushes,
  and `action` (its `name`) around an action's handlers. Each is made
  only where tracing's max level is trace (`perf_spans`), so with no
  subscriber, or one that takes less, a span's site costs one relaxed
  load and allocates nothing. A test of the spans and their order.
- `src/view.rs`, `src/window.rs` and `src/text_system/line_layout.rs`:
  a cached view drawn anew draws anew only those of the cached views
  inside it that changed, and takes the rest from the frame before
  (elundqvist/kvist#611). Upstream had the window refresh every cached
  view inside one drawn anew (`window.refreshing`), so a cached view
  holding cached views, a drawer whose parts are views of their own,
  laid them all out again whenever anything of it changed, a frame of
  one part's output the others too. It could not take them as they
  were, since a view's ranges index the frame it was last drawn in,
  and one taken whole into the next frame lands elsewhere in it: the
  cached views inside it kept ranges into the frame before, and were
  taken from where something else now was. Now a view taken whole from
  the frame before (`Window::reuse_prepaint`, `Window::reuse_paint`)
  carries the cached views among the element states it holds to where
  their parts land (`ViewElementState::carry_prepaint`, `carry_paint`,
  each index moved by as much as its range's start, as every part is
  taken whole and in order), and each view's ranges say which frame
  they index, by the window's count of its draws (`Window::draws`), so
  that a view is taken only from the frame drawn last, and drawn anew
  where its ranges index another: a prepaint begun again in
  `Window::transact` no longer takes a nested view from the ranges of
  the try it gave up, and neither does a view prepainted and not
  painted, under a hidden element, take a paint range that is not there
  once it is shown (the paint's frame is none until the view is
  painted, and a view is taken only where both ranges index the frame
  drawn last). The whole window drawn anew, a refresh, still draws every
  cached view anew. Tests of a nested view not drawn again with its
  parent, drawn again at its own change and at a refresh, of one taken
  from a frame its parent was taken whole into, at a new place in it,
  which keeps its box and takes its clicks, of one prepainted again in
  a transaction that gave up its first try, and of one shown again
  after it was hidden, which draws its box.
- `src/scene.rs` and `src/bounds_tree.rs`: a stretch of the frame before
  taken whole into a scene (`Scene::replay`, a cached view's paint) keeps
  the orders it was drawn at, all moved up by as much, so that it is
  drawn over the highest order under it, and goes into the bounds tree as
  one leaf that covers it at its highest order (`Stretch`,
  `BoundsTree::insert_at`), so that what is drawn after it over any of it
  is drawn over all of it (elundqvist/kvist#611; `insert_at` checks in a
  debug build the order it is given, and the plain `insert` that every
  primitive of a scene takes does not search the tree twice). Each of
  its primitives was ordered again against the whole scene, a bounds
  tree insert each: a cached panel of some 1300 primitives, a table of
  containers, taken into every frame cost 0.3 ms a frame, 0.04 ms now.
  A stretch inside a layer, whose primitives take the layer's order and
  go into no tree, or one whose layers do not close within it, is taken
  a primitive at a time as before. Tests of a stretch drawn over what it
  covers and under what covers it, its own primitives in their order,
  and of the two taken a primitive at a time.
- `src/app.rs` and `src/window.rs`: `KeystrokeEvent::is_held`, the key
  down's own, so that an interceptor can tell a held key's repeats from
  its press (elundqvist/kvist#613), and a key an interceptor stopped goes
  to no key listener, where the first capture listener on the focus path
  was still handed it. `src/key_dispatch.rs`: a test of a held key an
  interceptor stops, which reaches neither its binding nor a listener,
  and of the interceptor told whether a key is held.
- `src/text_system/line.rs`: a line's glyphs are drawn at their
  positions' distance from their row's first glyph (`GlyphRow`), where
  each was drawn at the one before it moved by the difference of their
  positions, a sum that drifts in `f32` by as much as each step rounds
  off (elundqvist/kvist#617): a line of 80 KB drawn scrolled to its end,
  its origin half a million pixels to the left of the view, had its last
  glyphs drawn hundreds of pixels from where they are laid out, out of
  the view, and a line of 240 KB had them end short of the caret. Its
  background and its underline and strikethrough the same. A test of a
  line of 240 000 glyphs drawn at its end, its background and underline
  where its glyphs are laid out.
- `src/window.rs` and `src/view.rs`: a cached view's element states are
  kept apart from the rest of the frame's, with the view's own state
  (`Frame::cached_views`, `CachedViewStates`), and a frame that takes the
  view whole from the frame before takes them over as one map
  (`Window::reuse_view_element_states`) (elundqvist/kvist#620). gpui kept
  one map of every element state a frame used, and moved each state a
  part of the frame taken whole had used into the new frame's map at the
  frame's end (`Frame::finish`), a hash of its element's path each: an
  open panel taken into each frame paid that for every element in it.
  Now a state is taken from the frame before as it is used, as before,
  into the map of the innermost cached view being drawn anew
  (`Window::with_view_element_states`, a stack of them), or the frame's
  own outside every one; a deferred draw keeps its states with the view
  it was deferred in, so they go with it; and a frame's ranges index the
  cached views it drew or took (`accessed_cached_views`) rather than
  every state, so a view taken whole takes the views inside it, each
  with its states, carrying their ranges as #611 did. `Frame::finish`
  moves nothing. Tests of a state inside a cached view, in its tree and
  deferred, kept through frames that take the view whole, its own and
  its parent's, found once the view is drawn anew, and let go once the
  view drawn anew no longer draws it.

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
