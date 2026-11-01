# Security

XHarness runs shells and git for you, on this machine or on a server you
connect to with a key. A problem there can let someone else run commands as
you, so please report it privately.

## Reporting

Use **[Report a vulnerability](https://github.com/astralyxdev/xharness/security/advisories/new)**
(the repository's Security tab), not a public issue. Say what's affected, how
to reproduce it, and what someone could do with it. The answer comes in the
same advisory, where the fix is worked out with you before anything is made
public.

## What matters most

- The remote server: its key (`xhk1_…`), the Noise handshake, and anything
  that lets a connection without the key run calls (`src/services/server.rs`,
  `transport.rs`, `rpc.rs`).
- This machine's server socket, which has no key: anything that lets another
  user on the machine reach it.
- The GitHub sign-in: tokens leaking into logs, arguments, or the wrong
  machine (`src/services/github.rs`).
- Opening a project or a file that makes XHarness run something on its own.

Keys and tokens in reports: replace them with placeholders.
