# Contributing

Thanks for looking. dbc is a one-maintainer project; small, focused
changes get in fastest.

## Before you start

- Open an issue first for anything bigger than a bug fix, so we agree on
  the shape before you spend an evening on it.
- The UI is Czech on purpose and stays Czech. Commit messages, code and
  comments are English.

## Building

Rust stable (see `rust-toolchain.toml`). Windows is the only platform the
app is built and tested on today.

```powershell
cargo build -p dbc-ui
cargo run -p dbc-ui            # dev profile: data in .\data, not %APPDATA%
cargo test --workspace         # no Docker needed; container tests are #[ignore]
```

A first clean build compiles GPUI and bundled DuckDB and takes a while.
Nothing to do about it; it is a one-time cost.

## What a change needs

- **Tests.** Logic lives in pure functions with unit tests next to them
  (`grep -rn "#\[cfg(test)\]" crates/dbc-ui/src` shows the pattern). GPUI
  rendering is not unit-tested; the pure decision behind it is. Several
  source-scan audits in `crates/dbc-ui/src/main.rs` fail on purpose when
  a convention is broken — read the message, it says what to do.
- **A commit message that explains the why.** Title: `feat:`/`fix:` plus
  one plain sentence about what the user sees. Body: what changed and
  the decision behind it. Look at `git log` for the house style.
- **No new dependency without a reason in the commit.**

## Pull requests

One change per PR. CI runs `cargo test --workspace` on Windows; it must
be green. The maintainer reviews everything (`.github/CODEOWNERS`).

## Releases

Maintainer only: bump `version` in the workspace `Cargo.toml`, commit
`chore: vX.Y.Z`, tag `vX.Y.Z`, push the tag. The release workflow builds
and attaches the zip; `CHANGELOG.md` is updated in the same commit.
