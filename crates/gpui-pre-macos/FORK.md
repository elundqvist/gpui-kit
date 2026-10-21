# gpui-pre-macos in this fork

This crate is gpui-pre-macos 0.3.3 as crates.io serves it, zed's
`gpui_macos` at zed@5b055fa: gpui's macOS platform, its AppKit window
and Metal renderer. It is carried in elundqvist/gpui-kit, as
`crates/gpui-pre` and `crates/gpui-pre-linux` are, so that fixes to the
platform can sit on top of it until they are upstream in
zed-industries/zed. The commit that brought it in, "chore: vendor
gpui-pre-macos 0.3.3 as published", is the published crate file for
file; each change since is a commit of its own.

Name and version stay gpui-pre-macos 0.3.3, so gpui-pre-platform 0.3.3,
which asks for `=0.3.3`, takes it when an application patches it in.

## What is changed

The files changed from the published crate, as the Apache License 2.0
asks (section 4(b)); each says so at its top:

- `src/window.rs`: the macOS window takes the app's zoom over the
  display's backing scale factor (`PlatformWindow::set_zoom`, the fork's
  gpui-pre, elundqvist/kvist#563), as the X11 and Wayland windows do;
  the published crate leaves the trait's no-op, so on a Mac the zoom
  changed nothing. On macOS the view's points are gpui's pixels, and
  the backing factor is applied only when the layer is drawn. Zoomed,
  gpui lays the window out in the view's points over the zoom
  (`content_size`) and draws at the backing factor times the zoom
  (`scale_factor`), the same device pixels: the layer's contents scale
  and the drawable stay the view's points at the backing factor
  (`display_scale_factor`, `view_size`, in `update_window_scale_factor`
  and `set_frame_size`). The window's bounds and the displays stay in
  points, as before, so windows are placed as they were. The pointer's
  position in every event (`zoom_input`), a wheel's pixel delta,
  `mouse_position` and a drag's position (`drag_event_position`) are
  divided by the zoom as they come, the input method's rectangle
  (`first_rect_for_character_range`) and a resize gpui asks for
  (`resize`) multiplied by it, and the least content size the window
  opened with (`min_size`) is told to AppKit again in points at each
  zoom, so a 640 by 480 window at 150 % is 960 by 720 on screen. The
  lock is not held while AppKit is told: it may grow the window to the
  new least size and call the delegate at once. The window cannot be
  had in a test; checked on a Mac under Kvist.

## Running its tests

From this directory, `cargo test`; they build with the Cargo.lock the
crate was published with. On a machine that is not a Mac the crate does
not build: `cargo check --target aarch64-apple-darwin` needs the macOS
SDK for gpui-pre-media's bindings.
