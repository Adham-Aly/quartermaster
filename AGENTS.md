# Quartermaster

A private, macOS-only Rust terminal app: one binary, `quartermaster`. You talk to one orchestrating
agent, the Quartermaster. It hands project work to crewmates, each in its own git worktree, and
brings the results back for you to approve and merge. Secondmates are standing orchestrators with
a charter. Claude agents run on the user's installed `claude`; every other
account runs on the app's own Rust provider engine.

**This file is zero-sum.** It stays at or under 150 lines: every line you add must be paid for by
removing another. Anything not every session needs goes in a skill, in `.agents/skills/<name>/SKILL.md`
(`.claude` links to `.agents`; `CLAUDE.md` links to this file).

**Keep this file and the skills true, so they never go stale.** Fix anything here you notice is
outdated, and when your change makes this file or a skill wrong, update it in the same change.

## Commands

| What | Command |
|---|---|
| Build everything | `cargo xtask build` (`cargo build --workspace --all-targets --locked`) |
| Format and lint | `cargo xtask check` (`cargo fmt --all --check`, then clippy with `-D warnings`) |
| Exactly what CI runs | `cargo xtask ci` (format check, clippy with `-D warnings`, build) |
| Install on this Mac | `cargo xtask install` (release build into `~/.cargo/bin/quartermaster`) |
| Run it by hand | `QUARTERMASTER_HOME=$(mktemp -d) cargo run -p quartermaster` |

CI (`.github/workflows/ci.yml`) runs `cargo xtask ci` on macOS on every push. Releases are built by
cargo-dist: pushing a `v*` tag runs `.github/workflows/release.yml` (skill `release`).
Building needs rustup, which installs the Rust pinned in `rust-toolchain.toml` by itself; Homebrew's
`rust` ignores that file. Clippy runs `pedantic` and `unsafe_code` is denied workspace-wide.

## Crate map

```text
quartermaster (the binary) ── startup, lock, logging, wiring; nothing depends on it
 ├─ qm-tui ─────────────────── the terminal UI; qm-types only, never qm-core
 ├─ qm-core ─┬─ qm-tools ───── qm-engine, qm-policy
 │           ├─ qm-engine, qm-store, qm-policy, qm-routing, qm-prompts, qm-git, qm-github, qm-auth
 │           └─ (never a concrete engine)
 ├─ qm-engine-claude ───────── qm-engine
 └─ qm-providers ───────────── qm-engine, qm-tools, qm-auth
qm-prompts ── qm-policy
every crate ── qm-types (which depends on none of ours) · xtask: std only
```

| Crate | What it holds |
|---|---|
| `quartermaster` | data directory, run lock, logging, config, store, engines, core, then the UI; `dev` commands |
| `qm-types` | IDs, domain enums, `Clock`, data-dir paths, config schema, `Refusal`, the core ⇄ UI API |
| `qm-core` | tasks, agents, turns, wake inbox, scheduler, watchdogs, settling, secondmates; `config_file` |
| `qm-store` | SQLite schema, migrations, queries, the database thread |
| `qm-engine` | the `Engine` trait and event order, process supervision, the scripted engine (dev runs) |
| `qm-engine-claude` | the Claude engine: drives `claude` over stream-json, our tools as an MCP server |
| `qm-providers` | the provider engine: wire adapters, model catalog, agent loop, file tools, helpers |
| `qm-tools` | the tool registry, argument types, descriptions and handlers |
| `qm-policy`, `qm-routing` | enforced rules and model routing, as plain functions |
| `qm-prompts` | prompts (sources in `prompts/`), briefs, charters, the context block |
| `qm-git`, `qm-github` | worktrees, kept refs, landing, guarded network git · PRs, checks, the push fence |
| `qm-auth` | credentials and logins for non-Claude accounts |

## Data directory and runtime

- All state lives in `~/.quartermaster`, or `QUARTERMASTER_HOME` when set (`DataDir::resolve`).
  Created on first run, mode 0700, umask 077: everything in it is private.
- `config.toml`: settings, never secrets; unknown keys are logged, not dropped; saved atomically.
- `auth.json`: non-Claude credentials (0600, atomic, never logged). Claude uses the user's own
  `claude` login, which the app never reads or copies.
- `state.db`: SQLite, migrated in one transaction at startup, used only through one thread.
- `run.lock`: a second instance on the same data directory says so and exits non-zero.
- `logs/quartermaster-YYYY-MM-DD.log`: redacted; level from `QM_LOG` (default `info`). The
  terminal belongs to the UI, so nothing is logged there.
- `seat/`, `secondmates/SM-n/`, `tasks/T-n/` (worktree, brief, session, jobs, evidence),
  `workspace/` (the built-in personal project), `tui.toml` (column widths).

## House rules

These override anything else. Security comes first.

1. **Never set `ANTHROPIC_API_KEY` or `CLAUDE_CONFIG_DIR`**, in code, scripts or your shell. They
   move `claude` off the user's keychain subscription login.
2. **Never print credentials**: no tokens, keys, cookies, `auth.json` contents or keychain output
   in logs, commits, screenshots, recordings or reports. A log line's redaction is a backstop, not a
   licence.
3. The user's email is for identification only; never send it anywhere.
4. Development runs use a throwaway data directory (`QUARTERMASTER_HOME=$(mktemp -d)`), never the
   real `~/.quartermaster`. Never copy `auth.json` between data directories (refresh tokens rotate).
5. Commits are conventional (`type(scope): summary`). Never add an AI or agent co-author or any
   `Co-Authored-By` trailer naming a model or agent.
6. Never hand-edit `CHANGELOG.md` or a generated file (`catalog/snapshot.json`, cargo-dist's
   `release.yml`); change the generator's input and regenerate.
7. No force-push, no `--no-verify`, no skipping hooks. Never move a pushed tag.
8. Prefer quality, simplicity, robustness and long-term maintainability over development speed.
9. Bug fixes start by reproducing the bug end to end in the real binary, the way a user sees it.
10. Be picky about the UI, down to the cell. If something looks off, even if it isn't yours, fix
    it or report it.
11. Fix any lint failure or warning you see, even if it isn't yours. No blanket `allow`s.

## Invariants the code relies on

- **The UI is a client.** `qm-tui` sees only `qm_types::api` (`Command` in; `Update`/`Snapshot`
  out, over `link()`). New API types and fields are additive: `#[non_exhaustive]`,
  `#[serde(default)]`, built through constructors, never struct literals.
- **The core is engine-agnostic.** The binary injects `Arc<dyn Engine>` per `EngineKind`; every
  engine keeps the event order documented on `qm_engine::EngineEvent`.
- **One rulebook.** Every tool call from either engine comes back to `qm-tools` handlers, which
  check `qm-policy` and reach the core through `ToolHost`. Every `ToolHost` method has a default
  body; the core host re-checks the caller's role against the registry.
- **One database thread.** Only `qm_store::db::Db::call` touches SQLite. Migrations are append-only;
  tables are `STRICT`; events are append-only; rows are retired with `*_at`, never deleted.
- **Secrets stay in `qm-auth`.** `Secret` redacts `Debug`, has no `Display`, and neither it nor
  `Credential` is `Serialize`. No error, `Refusal`, event, log line or tool result carries one.
- **Agents start from a clean environment.** Engines build the child environment from scratch;
  `qm_types::env::NEVER_PASSED_TO_AGENTS` is refused; `CLAUDE_CONFIG_DIR` is passed through only
  if the app itself has it, and never set. Claude orchestrators get only our tools; crewmates get
  an allow-list of built-ins; every `system/init` is checked (skill `claude-engine`).
- **Merging the wrong commit is the worst failure.** A merge happens only at the approved head,
  re-read from GitHub first (`qm-core` `settling/land_pr.rs`). App pushes go only through `qm-core` `delivery/push_guard.rs`,
  the app's network git only through `qm-git` `network.rs`; crewmates can't push at all (the
  fence in `qm-github` `fence.rs`); their work leaves only through `pr_open` (skill `git-safety`).
- **Crewmates have helper agents on both engines** (Claude's `Agent`, the provider engine's
  `agent`); while one runs, every crewmate tool that isn't `Read` class is refused in-band.
- **Saying no is a `Refusal`:** one plain sentence the agent can act on, naming the in-band path.
- **Time comes from a `qm_types::clock::Clock`**, never read directly in core logic.
- **The terminal's 16 colours only**, through `qm-tui` `theme.rs` and the 16-colour backend.
- Each library crate has its own `thiserror` error enum.

## Shared lists (add lines; keep order)

Workspace members and `[workspace.dependencies]` (`Cargo.toml`), `TOOLS` (`qm-tools`
`registry.rs`), `HANDLERS` (`handlers/mod.rs`), `MIGRATIONS` (`qm-store` `schema.rs`), `PROMPTS`
(`qm-prompts` `index.rs`), `WakeKind` (`qm-types` `domain.rs`), `Screen` (`qm-tui` `router.rs`),
API parts (`quartermaster` `wiring.rs`), and this crate map.

## Skills

`release` · `run-by-hand` · `debug-from-logs` · `add-tool` · `add-provider` · `model-catalog` ·
`add-migration` · `add-setting` · `core-ui-api` · `tui-conventions` · `claude-engine` ·
`git-safety` · `prompts` · `bump-toolchain`. Each `SKILL.md` says when to use it.
