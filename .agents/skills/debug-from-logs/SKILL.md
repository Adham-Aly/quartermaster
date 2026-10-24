---
name: debug-from-logs
description: Find out what a turn, a crewmate, a secondmate or a task actually did from the data directory, the day's log (QM_LOG levels, redaction), state.db (events, turns, wakes, session lines, jobs) and the agents' session transcripts. Use when a turn failed or hung, a crewmate seems stuck, a wake or a card went missing, or a user reports odd behaviour.
---

# Debugging from the data directory

Work on a copy or a throwaway data directory where you can (skill `run-by-hand`). On the user's
real `~/.quartermaster`, only read, and never copy `auth.json` or paste anything secret into a
report. Reproduce the bug end to end before fixing it.

## The log

`<data dir>/logs/quartermaster-YYYY-MM-DD.log`, one file per local day, mode 0600, appended.

- Level: `QM_LOG` is a `tracing` `EnvFilter` directive, `info` by default:
  `QM_LOG=debug quartermaster`, or per crate: `QM_LOG=info,qm_core=debug,qm_engine_claude=trace`.
- Nothing goes to the terminal (it belongs to the UI); startup failures also print one line to
  stderr. The first lines of a run are `starting` (version and data directory) and
  `database ready`; `config.toml` keys the app doesn't know are logged as warnings.
- Every line passes `redact` (`crates/quartermaster/src/logging.rs`): known token prefixes (`sk-`, `ghp_`,
  `eyJ`, …), `key=value` pairs whose key looks secret, and `Bearer`/`Basic` values become
  `[redacted]`. It is a backstop: if you add logging, never log a credential, a header or a
  request body in the first place. A new provider's token prefix belongs in `SECRET_PREFIXES`.
- The log is sparse by design: warnings and errors, session opens, limits, retries, rate limits.
  The full record of what happened is in `state.db`.

## state.db

Open it read-only, even while the app runs (WAL): `sqlite3 -readonly <data dir>/state.db`.
Times are milliseconds since the epoch, UTC: `datetime(at/1000, 'unixepoch', 'localtime')`.
Display IDs are the row IDs: `T-0042` is `tasks.id = 42`, `SM-02` is `secondmates.id = 2`.

| Table | Answers |
|---|---|
| `events` | what happened, in order: `kind` (`seat.*`, `task.*`, `pr.*`, `checks.*`, `you.*`, `model.*`), `summary` (the display line), `payload` JSON; append-only |
| `turns` | each agent's turns: `started_by` (`user`/`app_event`), `status`, `outcome_reason`, `model`, `model_calls`, tokens, `cost_usd` |
| `agents` | who is who: `role`, `engine`, `model`, `effort`, `status`, `session_ref`, `last_sign_of_life_at`, `task_id` |
| `tasks` | a task's `status`, `outcome`, `branch`, `head_commit`, `pr_number`, `retry_of`, repair and review counts |
| `wakes` | the inbox: pending (`delivered_at` null), delivered to a turn, consumed; `dedupe_key` stops repeats |
| `messages` | the conversations: `source`, `kind`, `replies_to`, `turn_id` |
| `session_lines` | what a worker's session screen shows, line by line |
| `held_actions` | approval and decision cards, with the exact stored payload |
| `jobs`, `processes` | background jobs (`log_path`, exit code) and recorded process groups |
| `routing_decisions` | why a model was chosen (`why`, `skipped`, `fallbacks`) or why work waits |

Useful starts:

```sql
SELECT id, datetime(at/1000,'unixepoch','localtime'), kind, task_id, summary FROM events ORDER BY id DESC LIMIT 40;
SELECT id, agent_id, started_by, status, outcome_reason, model_calls, output_tokens FROM turns ORDER BY id DESC LIMIT 20;
SELECT id, target_agent_id, kind, dedupe_key, delivered_turn_id, consumed_at FROM wakes WHERE consumed_at IS NULL;
```

## Transcripts and task folders

- `tasks/T-n/`: `worktree/` (the crewmate's checkout), `brief.md` (exactly what it was told),
  `jobs/` (each background job's output log), `builds-on.md` (the whole text of the task it builds
  on). The folder goes at close, so only open tasks have one.
- `artifacts/T-n/` holds `plan/` and `evidence/`, removed once no open task reads them
  (`qm-core` `retention.rs`, at start, at close and hourly); `attachments/` is swept at each start.
  For done lookup, research, reproduce and teach tasks, `report/` (`QM_REPORT_DIR`) holds the
  app-seeded `SKILL.md` and `template.html`, the agent-authored `report.html`, and local `assets/`.
- `seat/` and `secondmates/SM-n/`: `home/`; `session/` is removed at each start.
- A provider-engine agent's session is `<owner>/transcript/<session>.jsonl` (header line, then one
  line per message; replaced whole on compaction, removed on `/clear` or retire), so a restart resumes it. Claude's transcript is Claude's own, under `~/.claude/projects/`; the app keeps no copy.
- `state.db` holds the kept rows only (tasks, reports, events, wakes). Turns, messages and session
  lines are there only for live agents on the provider engine, from `agents.chat_kept_from` on (the
  current chat, with the `seat.notice` events under its messages: compaction and `/clear` trim
  it, retiring removes it); a Claude agent's are not, so read them from the day's log or from a
  running server.

## Reading a failure

- `turns.status = 'failed'` or `'interrupted'`: `outcome_reason` says why. A process that died
  mid-turn fails that turn (the engine's `FailureKind::ProcessExited`) and the next turn resumes
  the session; a turn cut short by quitting is marked interrupted at the next start.
- A Claude session refused at start names the init check's finding (wrong version, an extra or
  missing tool, a foreign MCP server): see skill `claude-engine`.
- "Stuck": compare `agents.last_sign_of_life_at` with running `jobs`; the watchdogs post
  `task.quiet`, `task.watchdog` and `task.stuck` events.
- A wake that "never arrived": check its row in `wakes` and whether a turn claimed it.
- A direct message to a worker never wakes the Quartermaster by itself: a batch of wakes made only
  of interventions starts no turn; it rides on the next one.
- A report refused while `reported` or `settling` names the in-band path; a re-report
  (`reported → reported`) carries the new head and voids cards for the old one (skill `git-safety`).
- An identical `reply` or `ask_user` in the same turn is shown once; the repeat is refused in-band
  (`CoreToolHost::show_message`). A question card is answered only by one of its options; typing a
  message while it is open doesn't answer it.
- A Quartermaster model change in Settings applies at once while the seat is idle, otherwise at
  its next idle moment. A crewmate's tokens are counted live during a turn (the task pane hides
  the row while the count is 0).
- `quartermaster dev leftovers` (skill `run-by-hand`) lists process groups left over from a crash.
