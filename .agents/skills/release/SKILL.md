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

1. Bump `version` under `[workspace.package]` in the root `Cargo.toml`. Every crate inherits it.
   Update `.github/release-notes.md` with changes since the last tag, keeping the install command
   first and no warning about the assets. This is the release history; never hand-edit a
   `CHANGELOG.md`.
2. `cargo build` (not `--locked`) to refresh `Cargo.lock`; its diff should contain only version lines.
3. `cargo xtask ci`.
4. Commit `chore(release): vX.Y.Z` (no co-author trailer), open the release PR through `pr_open`,
   and wait for its checks to pass and the approved PR to merge. Never push directly to main.
5. After merge, fetch main and confirm CI is green on the release merge commit.
6. The user tags that merge commit with `git tag -a vX.Y.Z <merge-sha> -m "vX.Y.Z"` and
   `git push origin vX.Y.Z`, or a crewmate calls `propose_release_tag` (annotated, at the merge
   commit, `branch` main) and the user approves its card, after which the app pushes it.
   Crewmates never push tags themselves; never bypass the push fence. The tag must equal `v` +
   the workspace version, or dist refuses to build.
7. Watch the Release workflow: `gh run list --workflow release.yml --limit 1`, `gh run watch <id>`.
   (For a tag the app pushed, `release_status` and the `release_observed` wake show the release
   and the runs as metadata; the checks below still need doing.)
8. `gh release view vX.Y.Z` must list, for each target, `quartermaster-<triple>.tar.xz` and its
   `.sha256`, plus `sha256.sum`, `source.tar.gz` and `quartermaster-installer.sh`. The release
   body must be `.github/release-notes.md` (install command first, no warning about the assets,
   no download table): the `custom-release-notes` job swaps it in after dist's `announce`.
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

The app updates itself the same way (`qm-core` `updates.rs`): it reads the newest release from
GitHub's API (`/releases/latest`, anonymously) at launch and every 6 hours, and Menu ▸ Update
runs that tag's `quartermaster-installer.sh` aimed at the running binary's folder. So every
release must keep a `vX.Y.Z` tag and the `quartermaster-installer.sh` asset, and must not be a
draft or a pre-release, or installed copies won't see it.

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
