# Third-party components

XHarness's own code is under the [XHarness License](LICENSE). These parts are
someone else's (or licensed separately) and keep their own licenses.

## In this repository

| What | Where | License |
| --- | --- | --- |
| [GPUI](https://github.com/zed-industries/zed) 0.2.2, by Zed Industries, with XHarness's changes listed in `third_party/gpui/XHARNESS.md` | `third_party/gpui/` | Apache-2.0 (`third_party/gpui/LICENSE-APACHE`) |
| [xattr](https://github.com/Stebalien/xattr) 0.2.3, by Steven Allen, with the fix in `third_party/xattr/XHARNESS.md` | `third_party/xattr/` | MIT or Apache-2.0 (`third_party/xattr/LICENSE-MIT`, `LICENSE-APACHE`) |
| [Lucide](https://lucide.dev) icons | `assets/icons/*.svg` (all but `github.svg`) | ISC |
| GitHub's mark, from [Octicons](https://github.com/primer/octicons) | `assets/icons/github.svg` | MIT; the mark is GitHub's trademark, used to link to GitHub |

## In builds

| What | License |
| --- | --- |
| [lino](https://github.com/astralyxdev/lino), cloned and built by `build.rs`, bundled in the binary | MIT (Astralyx), and its Go dependencies' licenses |
| Rust crates from crates.io (`Cargo.lock`): GPUI's dependencies, alacritty_terminal, syntect and two-face (with their grammars), snow, serde and others | Mostly MIT and/or Apache-2.0; also BSD-2/3-Clause, ISC, Zlib, Unicode-3.0, CC0-1.0, Unlicense, and MPL-2.0 (`cbindgen` at build time, `dwrote` on Windows, `option-ext`). Dual-licensed crates are used under their permissive option. |

`cargo metadata --format-version 1` lists every crate with its license.
Release archives should carry these crates' license texts; generating them
(for example with `cargo about`) isn't automated yet.
