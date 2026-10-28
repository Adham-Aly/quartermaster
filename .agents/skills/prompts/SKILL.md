---
name: prompts
description: Edit what the models are told - the system prompts in prompts/ (Quartermaster, crewmate, provider crewmate, secondmate), the prompts index, crewmate briefs, secondmate charters, the context block, tool descriptions and the provider helper prompt - within their size budgets and the marker rules. Use when changing agent behaviour through wording, adding a prompt, or changing what a brief, charter or context block contains.
---

# Prompts and what agents are told

Wording guides judgment; it never enforces a rule. Anything that must hold is checked in code
(`qm-policy`, the tool handlers, the core). A prompt only tells the model the rule exists and what
to do when it's refused.

## Where each text lives

| Text | File | Rendered by |
|---|---|---|
| Quartermaster system prompt | `prompts/quartermaster.md` | embedded via `PROMPTS` |
| Crewmate prompt (both engines; appended to Claude Code's preset on Claude) | `prompts/crewmate.md` | `qm_prompts::brief` |
| Provider engine's stand-in for Claude Code's preset | `prompts/crewmate-provider.md` | embedded |
| Secondmate prompt | `prompts/secondmate.md` | embedded; its charter follows |
| Crewmate brief (first message, saved as `tasks/C-n/brief.md`) | `crates/qm-prompts/src/brief.rs` | `BriefFacts` |
| App-authored static report skill | `crates/qm-core/src/reports/SKILL.md` | seeded into `$QM_REPORT_DIR/SKILL.md` |
| Secondmate charter | `crates/qm-prompts/src/charter.rs` | |
| Context block at the head of every orchestrator turn | `crates/qm-prompts/src/context_block.rs` | `ContextFacts` |
| Summary when a session is replaced | `crates/qm-prompts/src/session_summary.rs` | |
| Tool descriptions | `crates/qm-tools/descriptions/<key>.md` | skill `add-tool` |
| Provider helper agent's prompt | `crates/qm-providers/src/helpers/prompt.md` | |

A new prompt file goes in `prompts/` with one line in `PROMPTS`
(`crates/qm-prompts/src/index.rs`), looked up with `qm_prompts::index::get(name)`. Everything is
embedded at build time, so a change ships with a rebuild.

## Budgets

Orchestrators pay for their prompt on every turn, so keep them small. Tokens are estimated at
about four characters each:

- the Quartermaster's prompt: at most about 3,000 tokens;
- its prompt plus all its tool descriptions: at most about 7,500 tokens;
- one tool description: at most 1,900 characters.

Cut words before adding them; one exact sentence beats a paragraph. Check both orchestrator roles.

For deferred starts, distinguish `schedule_crewmate` (no child before its trigger), `link_tasks`
(existing tasks), and `schedule` (reminder wake). Keep normal start fields in one schema, nested
under `start`. Lasting model/effort/premium choices cite `choice_message`, an actual user-message
numeric ID. The core verifies user source and ownership/delegation, not semantic field intent:
wording must still require a matching, unsuperseded user request within its original scope.

Orchestrators author optimal, self-contained briefs, routed requests and follow-ups for each
recipient, rather than relying on automatic verbatim relay. Preserve every applicable user
constraint, scope, permission, prohibition, path and attachment, including earlier context and
corrections; split tasks without losing shared conditions. Quote only when exact wording helps
(e.g. structured plan answers); never require wholesale copying or invent authorization.

Retained `words_message` and `words_cut` fields are internal source provenance for authority, not
the recipient's task text. The former selects an earlier user-message source; the latter may
remove only unrelated whole sentences, lines or clauses (`qm_policy::user_words`), never the
request or its conditions. They neither replace authored instructions nor grant permission.
Keep `choice_message` separate: a curated paraphrase is not authority for model/effort/premium
choices, and citations must still match an unsuperseded original request and its scope.

## Markers

The model tells three sources apart: its supervisor and the app (both marked, `<qm:…>` tags) and
the user (unmarked, authoritative). Only the app may produce a marker:

- Every piece of text that didn't come from the renderer (user words, goals, instructions,
  summaries, a charter's scope words) passes through `qm_policy::markers::neutralise`, which turns
  `<qm:` and `</qm:` into `‹qm:` (any case), before it reaches an agent.
- Renderers are pure functions of their facts structs; the core gathers the facts. Keep them pure
  so what was sent can be saved exactly (briefs are written atomically, 0600).
- The context block's "since your last turn" sections come from events whose `kind` has a section
  prefix (`model.*` → `<model_changes>`), with the display line in `summary`.

## Style

Plain, direct, second person ("you"). Say what to do and what a refusal means. Name the in-band
path (the tool to call) whenever the model might otherwise look for another way out.

## Verify

`cargo xtask check`, `cargo build`, then one real turn of the affected role in a throwaway run on
the cheapest model (skill `run-by-hand`), reading the brief or transcript it actually got (skill
`debug-from-logs`).
