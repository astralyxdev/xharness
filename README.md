# XHarness

[![CI](https://github.com/astralyxdev/xharness/actions/workflows/ci.yml/badge.svg)](https://github.com/astralyxdev/xharness/actions/workflows/ci.yml)

A native canvas for each project: terminals that outlive the app, editors with
syntax highlighting and language servers, Finder-style explorers, a GitHub
Desktop-style git client, and [lino](https://github.com/astralyxdev/lino) for
agents. Built with [GPUI](https://www.gpui.rs) and the Astralyx UI design tokens.

## Client and server

The app is a client. The work happens in a background process, the **server**
(the same binary, started with `--server`): every terminal's shell, and every
file, git (with its GitHub sign-in) and lino operation. Quit the app and the
server keeps going (shells, agents, a push in progress); open it again and
you're back where they are. Language servers stay in the app.

This Mac's server listens on a Unix socket in the data folder. It starts when
you click **Start** (in the footer, or on a project that needs it) and runs
until you **Stop** it; stopping ends its terminals, so it asks first. While it
runs, its X sits in the menu bar, with Open XHarness and Stop server.

The footer shows the selected project's server: running or not, its address,
ping, OS, terminals and uptime.

### Remote servers

A project can live on another machine. Run the server there:

```sh
tar xzf xharness-server-0.1.0-linux-x64.tar.gz   # from scripts/build/server.sh
./xharness-server --listen 0.0.0.0:38888
```

It prints a key, `xhk1_…`: the address, a name, and a 256-bit secret. In
XHarness, **New project → Remote server**, paste the key, and type a folder on
that machine. The project's terminals, files, git and lino then run there.

- Connections use Noise (`NNpsk0`, ChaCha20-Poly1305, BLAKE2s), keyed with the
  secret: without the key nothing gets through, and everything after the
  handshake is encrypted. Anyone with the key can run anything as the server's
  user, so keep it like a password.
- The key survives restarts (`server-key.json`, 0600); `--new-key` makes a new
  one (old keys stop working), `--host` sets the address in the key (when the
  machine's own name isn't how you reach it), `--name` its name,
  `--stop` stops it.
- Open the port to your machine only: a firewall rule, a VPN, or no open port
  at all with SSH (`--listen 127.0.0.1:38888 --host localhost` on the server,
  `ssh -L 38888:localhost:38888 server` here).
- Language servers are off for remote projects (they run in the app), and
  dropping files from Finder doesn't reach them.

## Layout

```
src/
  main.rs            startup: key bindings, menus, the window
  app/               the window's shell
    mod.rs             title bar, panels, menus and dialogs (AppShell)
    sidebar.rs         pages, projects and their open windows
    home.rs            the Home page
    marketplace.rs     mods and language servers to install
    project_dialog.rs  new project / edit project / edit window (local or remote)
    servers.rs         which servers answer: pinged every few seconds
    footer.rs          the selected project's server, Start / Stop
  canvas/            a project's infinite canvas (pan, zoom, move, resize)
    geometry.rs        resize, zoom-around and fit arithmetic
    scaled.rs          renders a window's contents at the zoomed rem size
  windows/           the kinds of windows — each one a mod (mod.rs)
    terminal.rs        terminals (alacritty's VT, sessions in the host)
    explorer.rs        folders as lists; file moves, copies and drops
    editor/            the editor
      mod.rs             editing, undo, language-server features
      text.rs            positions, the line buffer, columns and UTF-16
      file.rs            reading, writing, watching the file
      syntax.rs          syntect highlighting into theme tokens
      completion.rs      completions and diagnostics' squiggles
      element.rs         painting lines, gutter, caret, diagnostics
    git.rs             changes, commits, branches, history graph
    git/remotes.rs     remotes, a new GitHub repository, GitHub sign-in
    lino.rs            lino's live stats
  services/          no UI
    projects.rs        projects, windows and layouts, saved per folder
    server.rs          the server: its socket (and port), terminals, and calls
    transport.rs       keys, and the encrypted connection to a remote server
    rpc.rs             calls from the app to the server (route, dispatch)
    files.rs           listing, searching, moving, reading and writing files
    pty.rs             a shell on a PTY: alacritty's (Unix) or ConPTY (Windows)
    shell.rs           zsh/bash (PowerShell on Windows) with the app's own setup
    git.rs             git through its command line
    github.rs          GitHub sign-in for git, new repositories
    lino.rs            the bundled lino and its JSON commands
    lsp.rs             the language server client
    language_servers.rs  the catalog, installing, finding binaries
    paths.rs           the data folder, your shell's PATH
  ui/                the design system
    theme.rs           Astralyx tokens, light and dark
    assets.rs          Lucide icons and the logo, compiled in
    mod.rs             buttons, sidebar pieces, dialogs
    text_input.rs, context_menu.rs, alert_dialog.rs, actions.rs
  platform/macos.rs  trackpad pinch, the Dock icon, the server's menu bar item
build.rs             clones lino at its pinned version, builds it for the target
third_party/gpui     gpui 0.2.2, patched to build for Windows anywhere
scripts/build/       release builds per platform
.github/             CI, releases, issue and pull request templates
```

## Building

You need Rust, Go and git:

```sh
git clone https://github.com/astralyxdev/xharness.git
cd xharness
cargo run                        # a development build
```

lino comes with every build: `build.rs` clones
[its repository](https://github.com/astralyxdev/lino) at the pinned version
(`LINO_TAG` and `LINO_COMMIT` in `build.rs`; the commit is checked, so a moved
tag can't change what's built), once per build folder, and builds it with Go
for the target's OS and architecture. Cross builds bundle the right binaries.
To move to a new lino, change those two lines.

- `XHARNESS_LINO_SRC=/path/to/lino` builds your own checkout instead.
- `XHARNESS_LINO_REPOSITORY=<url>` clones from a mirror.
- `XHARNESS_SKIP_LINO=1` builds without lino (the app then uses one on your
  PATH).

Release builds:

```sh
scripts/build/macos.sh universal   # dist/macos/XHarness.app (+ .zip); or arm64 / x86_64
scripts/build/linux.sh             # dist/linux/xharness-<version>-<arch>.tar.gz (run on Linux)
scripts/build/windows.sh           # dist/windows/xharness-<version>-x64.zip (cross-built with MinGW-w64)
scripts/build/server.sh all        # dist/server/xharness-server-<version>-<os>-<arch>.tar.gz
scripts/build/all.sh               # whatever this machine can build
```

`server.sh` builds the server alone (no app, no GPUI) for `linux-x64`,
`linux-arm64`, `macos-arm64` or `macos-x64` (`all`: both Linux ones and this
Mac's). The Linux ones are static (musl), so they run on any distribution, and
they cross-build from a Mac with nothing to install: Rust's own linker links
them. lino is inside the binary.

Each build compiles lino for the same platform and architecture as the app,
except Windows: lino doesn't support it yet, so Windows builds go without.

Windows builds cross-compile from macOS or Linux with MinGW-w64 (`brew install
mingw-w64`), no Windows SDK needed: gpui is vendored in `third_party/gpui` with
its shaders compiled at startup (see `third_party/gpui/XHARNESS.md`). On Windows,
terminals run on ConPTY with PowerShell, and the server uses Unix sockets
(Windows 10 and later have them).

## Git remotes and GitHub

In the Git window, the remote button (or **Add remote** when there's none)
opens Remotes & GitHub: add, edit, test or remove remotes, make a new
repository on GitHub and publish to it, and sign git in to GitHub.

Signing in hands a token to git's own credential helper on the project's
server (the macOS keychain, Git Credential Manager on Windows,
`~/.git-credentials` on Linux, or the GitHub CLI when git is set up to ask
it), so `git push` to github.com over HTTPS uses it everywhere, terminals
included. Three ways:

- **Sign in with GitHub**: a one-time code to enter on github.com/login/device.
  It goes through the GitHub CLI (`gh`) on the server, or through XHarness's
  own GitHub OAuth app when the build has one: register an OAuth app on GitHub
  with Device Flow on, and build with `XHARNESS_GITHUB_CLIENT_ID=<its client
  ID>` (or set it in the server's environment).
- **An account the GitHub CLI already has** (`gh auth status`): one click.
- **A personal access token** (classic, with `repo` and `workflow`).

## CI and releases

Every push to `main` and every pull request runs `.github/workflows/ci.yml`:
the macOS app (build and tests), the server on Linux (tests and the static
x64/arm64 builds), and the Windows app (cross-built). The Linux app and a
native Windows build run too, as experiments that don't block yet.

Pushing a tag `v<version>` (matching `Cargo.toml`) runs
`.github/workflows/release.yml`, which builds every package and drafts a GitHub
release with them. [CONTRIBUTING.md](CONTRIBUTING.md) has the steps.

## Where things are kept

- `~/Library/Application Support/xharness/` (Linux: `~/.local/share/xharness/`,
  Windows: `%APPDATA%\xharness\`):
  the project list (with remote projects' keys), the server's socket and log
  (`server-2.sock`, `server.log`), a listening server's key (`server-key.json`),
  the unpacked lino, installed language servers, `mods.json`.
- The GitHub token: with git's credential helper, on the server's machine.
- `<project>/.xharness/project.json`: the project's windows and layout (share
  it); `state.json` next to it is this machine's (sessions, folders).

## License

Source-available, not open source: you may read XHarness, build it to try it
and to develop changes, and contribute them, but not copy, redistribute or
reuse it. See [LICENSE](LICENSE) for the terms, [CONTRIBUTING.md](CONTRIBUTING.md)
to send changes, and [THIRD_PARTY.md](THIRD_PARTY.md) for the parts under
other licenses (GPUI is Apache-2.0; lino is MIT).
