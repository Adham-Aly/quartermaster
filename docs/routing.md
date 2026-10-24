# Model routing

Quartermaster routes crew tasks by **task kind**, then chooses an available
**model + effort combination** from your model levels (tiers). The rules use the
kind and checked task facts, not keywords in the task description. A level picks
a pool, not an effort: the selected entry supplies its own effort.

## Task kinds

| Task kind | Model level |
|---|---|
| `lookup` | Low |
| `build` building on a finished plan | Medium |
| `fix` building on a confirmed diagnosis | Medium |
| Other `build` or `fix` | High |
| `research`, `plan`, `reproduce`, `teach`, `review`, `unclear` | High |

`builds_on` links a task to earlier work. For lighter routing, the app checks
that the linked task is:

- **A finished plan:** a `plan` task that reported an answer.
- **A confirmed diagnosis:** a `reproduce` task that reported `reproduced: yes`,
  or a `research` task that reported an answer marked as a diagnosis.

Other links do not lighten routing. A plan only lightens a `build`; a diagnosis
only lightens a `fix`. If the kind cannot be decided, it routes as `unclear`.

Change any row in **Settings ▸ Routing ▸ Task kinds**: choose Premium, High,
Medium or Low, or **Default** to follow the table above. Press `r` to reset one
row, or `R` (or **Reset all to defaults**) and confirm to reset them all. Your
choices are saved in `config.toml` under `[routing.kinds]`, with every row
written once you change one; resetting all removes the section. A change applies
to the next task routed, without a restart. A model you name for a task, and
**Always use**, still come first. A value the app can't read is logged and that
row follows its default.

## Choose your model levels

Levels are pools **you control**, not fixed model names or effort settings:

| Level | Purpose |
|---|---|
| Premium | Above High; used when you request it, name a premium model, set a task kind to it, or choose a stronger-model retry |
| High | The default for work needing thinking |
| Medium | Work implementing finished thinking |
| Low | Lookups |

Levels start empty on a new installation. Configure at least one model entry in
**Settings ▸ Model levels** before messaging the Quartermaster; you do not need to
fill every level. Removing the last entry blocks chatting again. Existing saved
placements are kept, with legacy effort defaults migrated as described below.
Connecting an account never fills levels for you.

Add providers in **Settings ▸ Accounts**, then open **Settings ▸ Model levels**:

- Choose **Add model…** (`a` or `/`) to search the fixed catalogue of every model
  and each effort it supports. Search matches model name, ID, account and effort;
  a model without effort controls has one **(no effort)** entry.
- Choose a combination such as **Opus 5.5 (high)**, then its destination level.
  Levels already containing that exact combination are not offered. Adding it
  never removes it from the catalogue, so you can add it to another level later.
- Move placed combinations between levels by dragging, or select one and press
  `m`; duplicate destinations are unavailable. Press `x` to remove a placement.
- A model can be in several levels, or appear at different efforts in one level.
  Each placement is a model + effort label, with no separate effort toggle or
  add-to-tier context submenu.

## Provider quotas

**Settings ▸ Accounts** shows provider-reported quotas separately from local session
usage. Quotas are fetched in the background when an account first appears, every five
minutes, and when Settings opens (at most once per 30 seconds). Requests already in
flight are not duplicated. A failed refresh shows a plain error and keeps the previous
reading and its check time, so an old reading is not mistaken for a fresh one.

Supported sources are ChatGPT's usage windows, GitHub Copilot's chat/completions/premium
request quotas, and OpenRouter's **per-key spending budget** (not wallet credit).
ChatGPT and Copilot use private provider endpoints and may refuse access or change their
response formats. Other providers and credential types explicitly show that quota lookup
is unsupported. In particular, the app never reads Claude's subscription credentials.
An unlimited OpenRouter key budget does not mean unlimited account credit.

Lookups use the connected account's stored token and the existing refresh mechanism;
provider response bodies and credentials never appear in quota errors. Readings live only
in memory, are forgotten on disconnect, and do not influence routing or pause accounts.

## Effort belongs to the entry

The effort ladder is **Low → Medium → High → Extra high → Max**. A selected
combination keeps its effort, including when routing falls back to another level.
For example, High work falling back to a Medium entry labelled **(low)** runs at
Low effort, not High's default. Accounts that cannot run an entry's exact effort
are skipped rather than silently lowering that combination.

An effort you name for the task takes priority over an inherited `later_steps`
effort, which takes priority over the entry's effort. A named effort is adjusted
to a supported setting: the highest at or below the request, or the model's lowest
if none is that low. This explicit override replaces the combination's effort;
it does not require the account to support the original entry's effort.

**Quick** verification lowers only an inherited tier default by one effort step.
It does not lower an entry's explicit effort or an effort you named. Models
without effort controls receive no effort setting. A resumed conversation keeps
the effort it started with.

### Tier defaults and older saved levels

The `[tiers]` section of `config.toml` in your data directory
(`~/.quartermaster` by default) still defines defaults: Premium and High start at
Extra high; Medium and Low start at Medium. These are **not** the efforts assigned
to normal tier routing or new catalogue entries. They are used for model-only
pins, an explicitly selected Quartermaster model without an effort override, and
migration of legacy entries that saved no effort (`None`).

When a legacy entry's supported efforts are known, its saved `None` is migrated
to the explicit effort obtained by capping its tier's configured default to those
supported efforts. Existing explicit efforts are unchanged; identical placements
are merged, keeping the earliest placement. Once migrated, later default changes
do not alter that entry. Models without effort controls retain `None`; entries
whose capabilities are not yet known remain unresolved until a later pass. Any
unresolved legacy entry still uses tier-default rules; on a crew fallback it
keeps the originally requested level's default, adjusted to the model used.

## Ask for a model or effort

Tell the Quartermaster in your message, for example:

- “Use [model name] at high effort for this task.”
- “Plan this first; use [model name] at medium effort for the build afterwards.”
- “Use premium for this.” (only this task; say so to make later steps premium too)

The app records choices for later steps as `later_steps`, by task kind. They
apply when a matching task builds on that work, directly or further down the
`builds_on` chain. The nearest matching entry wins; a model, effort or premium
named for the task itself replaces the entry entirely. A stronger-model retry
ignores inherited choices.

A named model must be placed in a tier; a model in none is refused, with a pointer
to **Settings ▸ Model levels**. If it can't serve the task, the task waits for that
model rather than substituting another. Naming a model does not by itself name an
effort: it uses the selected entry's effort unless you name one too.

Naming a model placed in Premium asks for that specific premium model and waits
if it cannot serve. A premium request without a named model can use another
Premium model; if none can serve, it tries High, Medium, then Low, recording why.

The app retains original user requests separately from chat, so a requested model,
effort or premium choice can still be applied on a later turn, including one
started by an app event or restart. The orchestrator cites the actual user message;
reports and its own judgment do not grant permission. One-task choices stay scoped
to that task, and later user corrections take priority. Other overrides, such as
base branch and verification, still require a user-started turn.

### Deferred crewmate starts

Ask for work after another task commits, reports done or lands, or at a given time.
The Quartermaster and secondmates can register the full start configuration through
`schedule_crewmate`, retaining the brief, `builds_on` and authorized model choices.
No child task, worktree or model session is created before its trigger holds.
Pending starts survive restart and can be inspected or cancelled. A review still
requires a real task-owned commit; it reviews the exact SHA captured at launch.
A triggered task may subsequently queue for its requested model if unavailable.
The separate `schedule` tool sends only a reminder wake.

For a standing preference, **Settings ▸ Routing ▸ Always use** pins one model,
with an optional fallback. Task-specific model choices take priority; premium
requests and stronger-model retries bypass the pin. A pin names a model, not a
combination, so its effort comes from the task's routed level's default unless
you named an effort. Quick verification can lower that default, and the result
is adjusted to the pinned model's supported efforts. If neither pinned model can
serve, normal routing takes over.

## The Quartermaster and secondmates

Their model and effort are separate controls in **Settings ▸ Routing**; crew task
routing never changes those settings. Secondmates follow the same settings.

- **Auto model, Auto effort:** tries **High → Medium → Premium → Low** across
  subscriptions, using the selected entry's effort. Each account must support
  that exact combination, even when Auto moves to another level.
- **An explicit Quartermaster effort:** overrides the entry effort, including
  with Auto model, and is adjusted to the selected model's supported efforts.
- **An explicit Quartermaster model, Auto effort:** uses the model's tier default
  rather than a crew entry's effort, adjusted to the model's supported efforts.
  An explicitly selected model may use an API key as well as a subscription.

Changes apply at the next idle moment, never mid-turn.

## Availability and fallbacks

Routing skips models that cannot handle the task's images or context, accounts
at their limit, and accounts that do not support the selected combination's
effort (unless a named effort replaces it). Within a tier, it prefers your
preferred crew account, then subscriptions over API keys, allowance at risk of
going unused, and newer models in the same family. Ties retain placement order.

If no entry can serve in the chosen level, it tries **Low → Medium → High**,
**Medium → High → Low**, or **High → Medium → Low**. Each fallback uses the
selected entry's model **and effort**, subject to your named effort override.
Premium is never an automatic fallback for other levels. Premium work (asked for,
set for a task kind, or a stronger-model retry to Premium) tries
**Premium → High → Medium → Low**; retries to High or Medium wait for their
target level. If no entry can serve, the task waits. The routing record explains
the choice, skipped models and any fallback.
