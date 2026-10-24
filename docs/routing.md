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
provider response bodies and credentials never appear in quota errors. The display
readings live only in memory and are forgotten on disconnect. Each successful check
also stores, on the account, just the percent used of each pool and when it resets,
which routing reads (next section). A quota check never pauses an account.

## Quota-aware routing within a tier

When a tier holds several accounts and nothing explicit decides, routing serves the
account with the **most quota left**. Explicit choices still win: a named model, a pin or
your preferred crew account decides the account before quota does; a named tier, a named
effort, a premium request and inherited later steps still apply, and quota only ranks
the accounts inside the tier they select.

**What counts as quota.** Each account's reading is its **largest applicable pool**: a
prepaid credit or key budget first, then the longest window (weekly over five-hour), and
of two pools the same length the more used one. Readings come from two places:

- the engine's own usage reports, read once a minute: Claude's latest rate-limit windows
  (5-hour and 7-day) after its first turn, and the provider engine's usage headers;
- the provider quota checks above (ChatGPT windows, Copilot chat and premium requests,
  OpenRouter key budget).

A window past its reset time counts as unused. An account with **no quota left** under
any limit (a window at 100% that hasn't reset, or a spent budget) is never used until it
resets; Copilot's monthly premium requests rank but never cut an account off, since
Copilot keeps serving included models.

**Reserve marks.** In **Settings ▸ Accounts**, select an account and press `m` (or
right-click ▸ *Reserve mark…*) to set its reserve mark: a **percent used**, 1 to 99.
Once its largest pool reaches that mark, the account is *held in reserve*: other accounts
in the tier serve first. Leave the field empty for no mark. Each account's *Routing* line
shows what routing reads ("7-day window 60% used · reserve at 55% used · held in
reserve"). Marks are per account, so two keys on the same provider can differ.

**The order within a tier**, best first:

1. Under its mark (and under 90% used): the least used serves.
2. Subscriptions whose quota is unknown.
3. Nearly used up (90% or more) but under its mark.
4. In reserve: the one **least far past its mark**, ties going to the least used overall.
5. Pay-per-token API keys whose quota is unknown.

Then subscriptions before keys, newer models in the same family, and placement order.
With no marks set this is a plain comparison of quota left. For example, with Claude at
60% used (mark 55%), ChatGPT at 40% (mark 30%) and a DeepSeek key at 80% (mark 75%), all
three are in reserve; Claude and DeepSeek are 5 points past and ChatGPT 10, and Claude
wins the tie by being less used. The routing record says when quota made the choice
("Quota chose its claude account (60% used, past its 55% reserve mark) over custom (80%
used, past its 75% reserve mark)").

**Unknown quota** is never read as all left, nor as used up. Claude reports usage only
through its engine, so a Claude account shows *quota unknown* until its first turn;
API keys from Anthropic, OpenAI, Gemini and custom endpoints have no quota source and
serve after every subscription (a key with a known budget, such as OpenRouter's,
competes on quota like any other account).

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

## Ask for a model, tier or effort

**Tier (model level) and effort are separate.** “Medium-tier” always means the
Medium level where you manually ranked model entries in **Settings ▸ Model
levels**. “Medium-effort” means the medium effort version of a particular model,
regardless of which tier holds it. The same distinction applies to High and Low:
a High-tier entry can run at low effort, and a Low-tier entry at high effort.
A tier request selects that level's model pool; an effort request changes only
effort, never tier. The task kind still describes the work, not your model choice.

The top-level `start_crewmate.tier?: Tier` is a user-authorized level override,
separate from `model` and `effort`; it is not a within-tier `preference` constraint.
It bypasses the standing pin and replaces inherited later-step choices. If a model
is named too, it must be placed in that tier. An unavailable level uses normal
fallback rules and records why; the request does not invent placements or efforts.
A tier supplied with a stronger-model retry must match its computed target;
contradictory tier/retry or non-premium-tier/premium requests are refused.

Tell the Quartermaster in your message, for example:

- “Use a medium-tier model for this task.” (select from your Medium level)
- “Use [model name] at medium effort.” (keep its tier; override only effort)
- “Use [model name] at high effort for this task.”
- “Plan this first; use [model name] at medium effort for the build afterwards.”
- “Use premium for this.” (only this task; say so to make later steps premium too)

The app records choices for later steps as `later_steps`, by task kind. They
apply when a matching task builds on that work, directly or further down the
`builds_on` chain. The nearest matching entry wins; a model, tier, effort or premium
named for the task itself replaces the entry entirely. A stronger-model retry
ignores inherited choices.

A named model must be placed in a tier; a model in none is refused, with a pointer
to **Settings ▸ Model levels**. If it can't serve the task, the task waits for that
model rather than substituting another. Naming a model does not by itself name an
effort: it uses the selected entry's effort unless you name one too.

Naming a model placed in Premium asks for that specific premium model unless you select another tier where it is also placed, and waits
if it cannot serve. A premium request without a named model can use another
Premium model; if none can serve, it tries High, Medium, then Low, recording why.

The app retains original user requests separately from chat, so a requested model, tier,
effort or premium choice can still be applied on a later turn, including one
started by an app event or restart. The orchestrator cites the actual user message;
reports and its own judgment do not grant permission. Recent actual requests appear in
`<qm:user_message_sources>` (oldest first); cite their numeric ID as `choice_message`
only for a matching, unsuperseded request in its original scope. Presence alone grants
no authority. Use `find_user_requests` (`query`, `before`; twelve latest first) for older
sources; ask again only if no matching source can be recovered. One-task choices stay scoped
to that task, and later user corrections take priority. Other overrides, such as
base branch and verification, still require a user-started turn.

### Deferred crewmate starts

Ask for work after another task commits, reports done or lands, or at a given time.
The Quartermaster and secondmates can register the full start configuration through
`schedule_crewmate`, retaining the brief, `builds_on`, authorized model/tier/effort/premium
choices and structured within-tier `preference`. A secondmate passes `correlation`;
the app carries the routed request's choices and guidance without the secondmate
reasserting them. Delegated citations remain tied to the routed request's original
user source and owning conversation, not the secondmate's conversation; the app
validates that source and owner when replaying the saved start.
No child task, worktree or model session is created before its trigger holds.
Pending starts survive restart and can be inspected or cancelled. A review still
requires a real task-owned commit; it reviews the exact SHA captured at launch.
A triggered task may subsequently queue for its requested model if unavailable.
The separate `schedule` tool sends only a reminder wake.

For a standing preference, **Settings ▸ Routing ▸ Always use** pins one model,
with an optional fallback. Task-specific model choices take priority; tier, premium
requests and stronger-model retries bypass the pin. A pin names a model, not a
combination, so its effort comes from the task's routed level's default unless
you named an effort. Quick verification can lower that default, and the result
is adjusted to the pinned model's supported efforts. If neither pinned model can
serve, normal routing takes over.

## Within-tier preferences

After classifying the kind, the Quartermaster can choose the **smartest model
meeting all of the user's constraints** among actual routed tier entries, not
from the whole catalogue. `start_crewmate.preference` is a structured decision:

```text
{selection: ModelId, provider?: Provider, model?: ModelId, effort?: Effort}
```

`selection` is required: the chosen model ID. Optional `provider`, `model` and
`effort` describe the user's requirements, **all** of which must hold together.
They are not independent hints or authority to invent a new model/effort
combination. For example, a provider requirement and an effort requirement must
match the same eligible entry in the routed tier. The context's `<models>` lists
canonical IDs with provider, provider model ID and placement effort; `default/none`
means no explicit placement effort, not an extra effort value to request. The
live app still checks account availability and task capabilities.

The app validates the selection against eligible entries within the tier,
including effort. A missing, stale, out-of-tier or otherwise ineligible match
falls back to ordinary routing with a note. A preference never escapes its tier
or asks for Premium; only ordinary fallback rules can move between levels.

Preferences are authority-free guidance and may be supplied on later or app-event
turns, including routed requests and deferred starts. A `choice_message` citation
is optional for preference alone, but any cited source must be a genuine original
user message validated against its owning conversation (including checked delegation).
Preferences do **not** replace the authority of `model`, `tier` and
`effort`: these require an explicit user request, on this turn or a still-applicable
earlier message cited with `choice_message`. The app checks source ownership or
checked delegation; the orchestrator must match fields and scope. A named model can
select across tiers; a named tier selects a model level; effort alone never
changes tier. A preference cannot weaken or contradict them. Inherited
`later_steps`, premium requests, pins and stronger retries keep their routing
authority too.

Today the Quartermaster supplies this typed answer. A future external decision
model can return the same answer for the same validation; none is configured
today.

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

Changes apply at the next idle moment, never mid-turn. Quota and reserve marks do not
move the Quartermaster's own seat between accounts, since it would switch engines at
every idle moment; they rank crewmates only. An account with no quota left is still
skipped.

## Availability and fallbacks

Routing skips models that cannot handle the task's images or context, accounts
at their limit, and accounts that do not support the selected combination's
effort (unless a named effort replaces it). Within a tier, it prefers your
preferred crew account, then the account with the most quota left as described in
[Quota-aware routing within a tier](#quota-aware-routing-within-a-tier), then
subscriptions over API keys and newer models in the same family. Ties retain
placement order.

If no entry can serve in the chosen level, it tries **Low → Medium → High**,
**Medium → High → Low**, or **High → Medium → Low**. Each fallback uses the
selected entry's model **and effort**, subject to your named effort override.
Premium is never an automatic fallback for other levels. Premium work (asked for,
set for a task kind, or a stronger-model retry to Premium) tries
**Premium → High → Medium → Low**; retries to High or Medium wait for their
target level. If no entry can serve, the task waits. The routing record explains
the choice, skipped models and any fallback.
