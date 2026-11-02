# Contributing

Thanks for helping. XHarness is source-available, not open source: you can
read it, build it to try changes, and send them in, but not copy or
redistribute it (see [LICENSE](LICENSE)). By opening a pull request you submit
your changes under the license's contribution terms (section 3).

## Getting set up

You need Rust (stable, via [rustup](https://rustup.rs)), Go
([go.dev/dl](https://go.dev/dl), or `brew install go`) and git. On macOS,
Xcode's command line tools are enough; on Linux, GPUI's libraries (the list is
in `scripts/build/linux.sh`).

```sh
git clone https://github.com/astralyxdev/xharness.git
cd xharness
cargo run
```

The first build clones lino at its pinned version and builds it (see
`build.rs`). Working on lino too? Point the build at your checkout:
`XHARNESS_LINO_SRC=../lino cargo run`. No network, or no Go:
`XHARNESS_SKIP_LINO=1` builds without it.

The app talks to a server process that keeps running after the app quits (it
owns your terminals). After changing server code (`src/services/`), stop the
server (footer → Stop) and start it again, or it keeps running the old
binary.

## Before a pull request

```sh
cargo test                          # the app and everything else
cargo test --no-default-features    # the server on its own, as remote servers run it
```

- Keep changes focused: one pull request, one change. Open an issue first for
  anything big, so we can agree on the approach before you build it.
- Match the code around you. Comments say why, in plain sentences.
- Anything that runs work should go through the server (`rpc::route`), so it
  works on remote projects too. Note anything that only works locally.
- UI changes: add before/after screenshots.
- CI runs on every pull request (`.github/workflows/ci.yml`): the macOS app,
  the Linux server, the Windows app and the Linux app must pass.

## Reporting problems

Bugs and ideas: [issues](https://github.com/astralyxdev/xharness/issues).
Security problems: privately, as [SECURITY.md](SECURITY.md) says.

## Releasing (maintainers)

1. Set `version` in `Cargo.toml`, run `cargo build` (it updates
   `Cargo.lock`), and merge that to `main`.
2. Tag it and push the tag:
   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```
3. The Release workflow builds the macOS app (universal), the Windows app,
   the Linux app (when it builds), and the server for Linux (x64, arm64) and
   macOS, then drafts a GitHub release with them and their SHA-256 sums.
   Look it over and publish it.

The macOS app isn't signed or notarized yet, so Gatekeeper asks before
opening it the first time.
