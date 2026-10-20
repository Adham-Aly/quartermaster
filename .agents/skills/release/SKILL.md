---
name: release
description: Cut a Quartermaster release with cargo-dist (bump the version, tag vX.Y.Z, push the tag, watch the Release workflow, check the assets), or change what a release builds (targets, installers, runners). Use when the user asks to release, publish, ship a version, tag, or add a platform to the release.
---

# Cutting a release

Releases are built by cargo-dist (`dist` 0.33.0, pinned as `cargo-dist-version` in
`dist-workspace.toml`). Pushing a tag `vX.Y.Z` runs `.github/workflows/release.yml`, which builds
`quartermaster` for each target and publishes a GitHub Release. On pull requests the same workflow
only runs `dist plan`. Nothing goes to crates.io (`publish = false` everywhere).

Pushing a tag is a GitHub write: do it only when the user asks for this release.

## Steps

1. Bump `version` under `[workspace.package]` in the root `Cargo.toml`. Every crate inherits it,
   so that is the only edit.
2. `cargo build` (not `--locked`) to refresh `Cargo.lock`; `git diff` shows only version lines.
3. `cargo xtask ci`.
4. Commit `chore(release): vX.Y.Z` (no co-author trailer) and `git push origin main`.
5. Wait for CI on that commit to go green: `gh run list --branch main --limit 3`, `gh run watch <id>`.
6. `git tag -a vX.Y.Z -m "vX.Y.Z"` and `git push origin vX.Y.Z`. The tag must equal `v` + the
   workspace version, or dist refuses to build.
7. Watch the Release workflow: `gh run list --workflow release.yml --limit 1`, `gh run watch <id>`.
8. `gh release view vX.Y.Z` must list, for each target, `quartermaster-<triple>.tar.xz` and its
   `.sha256`, plus `sha256.sum`, `source.tar.gz` and `quartermaster-installer.sh`. The release
   body must be `.github/release-notes.md` (install command first, a warning not to click the
   assets, no download table): the `custom-release-notes` job swaps it in after dist's `announce`.
   Never write or hand-edit a `CHANGELOG.md`.
9. Check a binary by hand:

   ```sh
   d=$(mktemp -d) && gh release download vX.Y.Z --dir "$d" --pattern 'quartermaster-aarch64-apple-darwin.tar.xz*'
   cd "$d" && shasum -a 256 -c quartermaster-aarch64-apple-darwin.tar.xz.sha256 && tar xf quartermaster-aarch64-apple-darwin.tar.xz
   otool -L "$(find . -name quartermaster -type f)"   # only system libraries
   ```

   It links only `Security`, `CoreFoundation`, `libiconv` and `libSystem` (SQLite is bundled,
   TLS is rustls). There is no `--version` flag: run it with a throwaway `QUARTERMASTER_HOME`
   (skill `run-by-hand`) and read the version on the `starting` line of its log.

## Installing a release

`curl -fsSL https://github.com/Adham-Aly/quartermaster/releases/latest/download/quartermaster-installer.sh | sh`
installs into `$CARGO_HOME/bin` (`QUARTERMASTER_INSTALL_DIR` overrides it;
`QUARTERMASTER_NO_MODIFY_PATH=1` leaves the shell's PATH alone) and writes its receipt to
`~/.config/quartermaster/quartermaster-receipt.json`. The README and the release notes present
this command as the only way to install; keep them pointing at it, never at the asset links.

## When it fails

Never move, delete or re-push a published tag, and never force-push. Fix the problem on `main` and
cut the next patch version. The workflow installs Rust through rustup, which reads
`rust-toolchain.toml`.

## Changing what a release builds

All dist config is in `dist-workspace.toml`: `targets` (`aarch64-apple-darwin`,
`x86_64-apple-darwin`), `installers` (`shell` only), `precise-builds = true`, and
`github-custom-runners` (both macOS targets build on
`macos-15`, x86_64 cross-compiled; the other jobs run on `ubuntu-24.04`). Also involved:
`[package.metadata.dist] dist = true` in `crates/quartermaster/Cargo.toml` (needed because of
`publish = false`), `dist = false` in `xtask/Cargo.toml`, and `repository` and `[profile.dist]`
(inherits `release`, thin LTO) in the root `Cargo.toml`. `release.yml` is generated: never
hand-edit it; change the config and run `dist generate`. `post-announce-jobs = ["./release-notes"]`
makes it call the hand-written `.github/workflows/release-notes.yml`, which replaces the release
body with `.github/release-notes.md`; edit that file to change the notes.

- **Add a platform:** add the triple to `targets`; for Windows also add `"powershell"` to
  `installers` (the app is macOS-only today: `nix`, `libproc`, `osascript`, `pbcopy`, so it must
  build there first). Run `dist generate`, check that the runner dist picks still exists and add a
  `github-custom-runners` entry if not, then `dist plan`. Commit the config and the workflow
  together.
- **Upgrade dist:** install the new version, update `cargo-dist-version`, run `dist generate`,
  review the workflow diff.
- **Check locally:** `dist plan`, and `dist build --artifacts=local --target <triple>`.
