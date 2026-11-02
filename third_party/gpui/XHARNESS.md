# gpui 0.2.2, patched for xharness

This is gpui 0.2.2 from crates.io with one change, so Windows builds work when
cross-compiled (from macOS or Linux, with MinGW) and need no Windows SDK:

- `src/platform/windows/directx_renderer.rs` (`shader_resources`): the HLSL is
  compiled into the binary (`include_str!`) and compiled with `D3DCompile` when
  the renderer starts. Upstream embeds bytecode made by `fxc.exe` at build time
  (only possible when building on Windows), and debug builds read the `.hlsl`
  files from the build machine's disk.
- `build.rs`: the `fxc.exe` step is off.

Nothing else differs; macOS and Linux builds are unchanged. Remove the
`[patch.crates-io]` entry in the root Cargo.toml to go back to upstream.

- `src/taffy.rs` line 312: explicit `f32` literals (silences a warning that
  shows now that gpui builds as a path dependency).
- `src/platform/mac/platform.rs` (`set_cursor_style`): leaves the cursor
  alone while the mouse is over a native view that isn't GPUI's (a Browser
  window's web page sets its own: the I-beam over text, the hand over
  links). `src/platform/mac/window.rs` exposes GPUI's view class for that
  check (`view_class`).
- Zoomed subtrees (`Window::with_zoom`, `ElementTransform` in
  `src/window.rs`, with `src/scene.rs`, `src/view.rs` and
  `src/interactive.rs`): a canvas window lays out at 100% and is drawn
  zoomed; hit testing and events stay in its own units.
- `src/elements/svg.rs` (`paint`): a transformed SVG (a rotated disclosure
  arrow) turns around its drawn center inside a zoomed subtree, not its
  laid-out one, which put it off its place at any zoom but 100%.
