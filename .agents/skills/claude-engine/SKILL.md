---
name: claude-engine
description: Change how the Claude engine (qm-engine-claude) launches and checks claude - argv flags, the built-in tool allow-list and deny list per role, the child environment and its switches, the supported claude version range, the startup and init checks, usage accounting, background jobs and helper agents. Use when a new claude version breaks or adds tools or flags, when a role's tool grant changes, or when a Claude session is refused at start.
---

# Changing the Claude engine

The engine drives the user's installed `claude` over its undocumented stream-json protocol, with
the argv the Agent SDK would build, and serves our tools as an in-process MCP server named
`quartermaster`. Each agent is one long-lived `claude` process in its own process group, started
through `qm_engine::supervisor`. Because the protocol has no spec, every session is checked
before the model may act.

## Where things are (`crates/qm-engine-claude/src/`)

| File | Holds |
|---|---|
| `argv.rs` | `build_argv`, `LaunchOptions`, `ToolSet`, `CREWMATE_BUILTINS`, `REPORTED_AS`, the `harness_tools!` list, `ORCHESTRATOR_DISALLOWED`, `CREWMATE_DISALLOWED`, `ATTRIBUTION_SETTINGS` |
| `env.rs` | the child environment from scratch: `COPIED_KEYS`, `OWNED_KEYS`, the fixed `SWITCHES`, `MIRRORED_SDK_VERSION` |
| `check.rs` | `SUPPORTED_VERSIONS`, `REQUIRED_CAPABILITIES`, `check`, `check_tools`, the zero-token `probe` |
| `detect.rs` | finding `claude` and reading `claude auth status` |
| `session/` | the protocol loop: init report, turns, interrupts, idle shutdown and resume, usage |
| `jobs.rs` | background shells and background helper agents as jobs |
| `transcript.rs` | mirroring claude's transcript into the agent's `session/` and restoring it |
| `mcp.rs`, `plugin.rs` | our MCP server; the skills-only plugin folder for crewmates |

## The tool grant per role

- **Quartermaster and secondmates** (`ToolSet::Orchestrator`): `--tools=` (no built-ins), our tools
  only, `--permission-mode=dontAsk`; `ORCHESTRATOR_DISALLOWED` also denies the harness tools,
  `Agent`/`Task`, `TaskStop` and `ToolSearch`.
- **Crewmates** (`ToolSet::Crewmate`): `--tools=` the `CREWMATE_BUILTINS` allow-list (Bash, Read,
  Edit, Write, NotebookEdit, Glob, Grep, WebFetch, WebSearch, Skill, the to-do list tools, TaskStop,
  Agent) plus ours, `bypassPermissions`; `CREWMATE_DISALLOWED` denies the harness tools and the
  ones that wait on a person (`AskUserQuestion`, plan mode) and `ToolSearch`.
- **The rule for a new `claude` tool:** grant it to crewmates only if it acts solely inside the
  agent's own session or worktree, or reads the web. Deny anything that reaches other sessions,
  schedules, notifies, publishes, triggers remotely or leaves the worktree: add it to
  `harness_tools!` so every role disallows it. A tool `claude` reports under another name than
  `--tools` takes goes in `REPORTED_AS` (`Agent` is reported as `Task`).
- Our own tools need no change here: they are allowed as `mcp__quartermaster__*` and checked
  against the session's descriptors (skill `add-tool`).

## The checks

- Every `system/init` (they arrive with the first turn, not at startup) is compared with the role's
  grant by `check_tools`: any missing tool of ours, extra built-in, or foreign MCP server stops the
  process before the model acts, with one sentence naming what was found. Keep this check strict:
  it is the backstop if `claude` renames a switch or adds a tool.
- `check` also holds the version to `SUPPORTED_VERSIONS` (min inclusive, max exclusive), the
  permission mode, and `REQUIRED_CAPABILITIES`. Supporting a new major version means reading what
  changed in its protocol first, then widening the range.

## The environment (never weaken it)

Built from scratch: only `COPIED_KEYS` come from the app's environment. `ANTHROPIC_API_KEY`,
`ANTHROPIC_AUTH_TOKEN` and the Bedrock/Vertex switches are never passed
(`qm_types::env::NEVER_PASSED_TO_AGENTS`), so an agent can't leave the subscription.
`CLAUDE_CONFIG_DIR` is passed through only if the app itself was started with it, and never set
or changed. A `SessionSpec.env` naming an owned key is refused (`EngineError::Unsupported`). The
switches: `ENABLE_TOOL_SEARCH=false` (nothing deferred), auto-memory and `CLAUDE.md` loading off,
`--setting-sources=` and `--strict-mcp-config` (none of the user's settings, agents or MCP
servers load), and `CLAUDE_CODE_HARBOR_KITE=0`, which closes the cross-session socket in
`/tmp/cc-socks` that would let any Claude session on the Mac message an agent. That switch is
undocumented: after a `claude` update, check no socket opens.

## Behaviour to keep

- **Cost totals are cumulative.** `total_cost_usd` and `modelUsage` are running totals for the
  whole process (helper agents and self-started turns included): a turn's usage is the difference
  since the previous `result` (`session/stream.rs`); a killed turn sums its finished calls. On
  resume, `claude` restores its totals from the transcript's last `cost-state` entry, so the engine
  starts its deltas from that entry (`transcript.rs`), not from zero.
- **Helper agents** (crewmates only, Claude Code's built-in agent types; `--setting-sources=` keeps
  the user's own agents out). `claude` has no foreground-only switch, so a background helper is a
  job (`JobLabel::helper`, `jobs.rs`): no idle shutdown while one runs; a finished one wakes an
  idle crewmate with an app-marked message; stop, discard and quit kill them; while any helper
  runs, every crewmate tool that isn't `Read` class is refused in-band (skill `add-tool`).
- A command `claude` moves to the background becomes a job with `JobStarted { ran_for_ms }`, how
  long it had already run, so its duration counts from when the command started.
- **Mid-turn messages.** Every user message carries a `uuid`, and `claude` reports each in a
  `command_lifecycle` message (`queued`, `started`, `completed`). A message sent during tool work is
  taken at the next tool result; one sent during the last text-only call stays queued and is
  answered in a second query after the first `result`. So a turn ends only when every injected
  message has started.
- **Self-started turns.** A `system/init` with no turn running is a turn `claude` started by
  itself (after a background task finished): it is interrupted before the model acts. Never
  interrupt a queued message's second query.
- Text is committed line by line; silent work is reported as `Activity` at most every 30 s.
- `--settings` turns off Claude Code's own commit and PR attribution.

## Verify

`cargo xtask check`, `cargo build`, then `QM_DEV=1 quartermaster dev probe claude` and one
crewmate turn on Haiku with `quartermaster dev session --engine claude --role crewmate` (skill
`run-by-hand`); the first `system/init` must pass the check. This spends a few cents of the
user's subscription; keep it to one turn.
