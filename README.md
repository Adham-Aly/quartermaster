# Quartermaster

```sh
curl -fsSL https://github.com/Adham-Aly/quartermaster/releases/latest/download/quartermaster-installer.sh | sh
```

> [!IMPORTANT]
> **This one command is how you install Quartermaster** (Apple silicon or Intel). Paste it into
> Terminal, then run `quartermaster`. Run it again to update. Don't download the files on the
> Releases page: in a browser they install nothing.

A macOS terminal app where you talk to one agent, the Quartermaster. It hands project work to
crewmates, each in its own git worktree, and delivers the results through GitHub once you approve.

![How Quartermaster works](assets/overview.svg)

## A task's life

![A task's life](assets/lifecycle.svg)

## Engines and accounts

![The two engines](assets/engines.svg)

## Install

Use the command at the top of this page. It picks the build for your Mac, checks its checksum,
installs `quartermaster` into `~/.cargo/bin` (or `$QUARTERMASTER_INSTALL_DIR`) and adds that folder
to your `PATH`.

**From source:** install [rustup](https://rustup.rs) (it installs the Rust release pinned in
`rust-toolchain.toml` by itself; Homebrew's `rust` ignores that file), then:

```sh
git clone https://github.com/Adham-Aly/quartermaster.git
cd quartermaster
cargo xtask install
```

## Requirements

| Needs | Notes |
|---|---|
| macOS 13 or later | Apple silicon or Intel |
| Command Line Tools | `xcode-select --install` (provides `git`) |
| Your git identity | `git config --global user.name` / `user.email`: commits are made as you |
| Claude Code, logged in | `claude auth login` with a Claude subscription; version 2.1.303 or later, below 3.0 |
| GitHub CLI, logged in | `gh auth login`: pull requests, pushes and CI (without it, projects build in place) |
| A terminal | at least 60×16 |
| Optional | other providers' accounts, added in **Settings ▸ Accounts** |

Never set `ANTHROPIC_API_KEY` or `CLAUDE_CONFIG_DIR`: they bypass the `claude` login the app uses.

## Run

```sh
quartermaster
```

The **Setup check** on first run offers to install or log in anything missing. Press `?` for keys.

![The data directory](assets/datadir.svg)

One app runs per data directory. For a throwaway one: `QUARTERMASTER_HOME=$(mktemp -d) quartermaster`.

## Code

![Crate map](assets/crates.svg)

| Task | Command |
|---|---|
| Build | `cargo xtask build` |
| Format and lint | `cargo xtask check` |
| Install this checkout | `cargo xtask install` |
