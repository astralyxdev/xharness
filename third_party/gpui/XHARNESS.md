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
