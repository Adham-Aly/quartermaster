---
name: bump-toolchain
description: Bump the pinned Rust toolchain (rust-toolchain.toml channel, the workspace rust-version floor) and fix the new clippy and rustfmt findings, or upgrade a dependency in [workspace.dependencies]. Use when a new stable Rust is out, when CI and a Mac disagree about lints, or when updating crates.
---

# Bumping the Rust toolchain

`rust-toolchain.toml` is the single source of truth: one exact release, with `rustfmt` and
`clippy`, `profile = "minimal"`. rustup installs it the first time `cargo` runs in the checkout,
on every Mac and in CI and the release workflow (no workflow names a version). So clippy and
rustfmt are identical everywhere and `-D warnings` can't diverge. Homebrew's `rust` ignores the
file; building needs rustup.

`rust-version` under `[workspace.package]` in `Cargo.toml` is the floor the code needs (edition
2024); the pin decides what actually builds it.

## Steps

1. In its own commit: set `channel` in `rust-toolchain.toml` to the exact new release (`1.NN.P`,
   never `stable`).
2. Run `cargo xtask check`. New lints show up as errors under `-D warnings`. Fix the code; don't
   add `allow`s to get past them. If a lint truly doesn't fit the codebase, allow it once in
   `[workspace.lints.clippy]` in `Cargo.toml` with a comment saying why (the pedantic exceptions
   there are the model). Proper names `doc_markdown` trips on go in `clippy.toml`.
3. `cargo fmt --all` if rustfmt's output changed, as a separate commit or clearly in the message.
4. Watch for deprecations that the new compiler flags (an API renamed): use the replacement only
   if it exists at `rust-version`, or raise `rust-version` in the same change.
5. `cargo xtask ci`, then `cargo xtask install` and launch it once (skill `run-by-hand`).
6. Commit as `build(toolchain): pin Rust 1.NN.P` (fixes as `fix(lint): …` or in the same
   commit), and push so CI confirms on a clean runner.

## Dependencies

- Versions live once in `[workspace.dependencies]`; crates use `foo.workspace = true` and enable
  only the features they use.
- Pick an exact version at least two weeks old. `cargo update -p <crate>` for lock-only updates;
  `--locked` everywhere else in `cargo xtask` means the lock never changes by accident.
- Check the binary still links only system libraries afterwards (`otool -L`; skill `release`).
- `rusqlite` must keep the `bundled` feature; `reqwest` stays on rustls (no OpenSSL).
