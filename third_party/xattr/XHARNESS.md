# xattr 0.2.3, patched for xharness

gpui's HTTP client unpacks archives with `zed-async-tar`, which uses xattr
0.2.3 on Unix. That version reads `libc::ENOATTR` on Linux, which current libc
no longer defines there, so the Linux app didn't compile.

- `src/sys/mod.rs`: `ENOATTR` is `libc::ENODATA` on Linux and Android (the
  value Linux returns for a missing attribute), and `libc::ENOATTR` on macOS
  and the BSDs, as before.
- `src/sys/linux_macos/macos.rs`: `u32` for libc's deprecated `uint32_t`
  (warnings show now that it builds as a path dependency).
- `Cargo.toml`: the dev-dependency on `tempfile` is gone (its tests aren't
  copied).

Nothing else differs. Remove the `xattr` line under `[patch.crates-io]` in the
root Cargo.toml once zed-async-tar moves to xattr 1.x.
