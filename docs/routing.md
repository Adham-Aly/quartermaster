# Model routing

Quartermaster routes crew tasks by **task kind**, then chooses an available model
from your model levels (tiers). The rules use the kind and checked task facts,
not keywords in the task description.

## Task kinds

| Task kind | Model level | Default effort |
|---|---|---|
| `lookup` | Low | Medium |
| `build` building on a finished plan | Medium | Medium |
| `fix` building on a confirmed diagnosis | Medium | Medium |
| Other `build` or `fix` | High | Extra high |
| `research`, `plan`, `reproduce`, `teach`, `review`, `unclear` | High | Extra high |

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

Levels are pools **you control**, not fixed model names:

| Level | Purpose | Default effort |
|---|---|---|
| Premium | Above High; used when you request it, set a task kind to it, or choose a stronger-model retry | Extra high |
| High | The default for work needing thinking | Extra high |
| Medium | Work implementing finished thinking | Medium |
| Low | Lookups | Medium |

Levels start empty on a new installation. Configure at least one model entry in
**Settings ▸ Model levels** before messaging the Quartermaster; you do not need to
fill every level. Removing the last entry blocks chatting again. Existing saved
model levels are preserved, and connecting an account never fills levels for you.

Add providers in **Settings ▸ Accounts**, then open **Settings ▸ Model levels**:

- Move model entries between levels by dragging, or select one and press `m`.
- Set an entry's effort with `e`, or choose **Tier default** to follow its level.
- A model can be in several levels, or appear at different efforts in one level.

Level-wide effort defaults live in the `[tiers]` section of `config.toml` in your
data directory (`~/.quartermaster` by default).

Effort is separate from model level: **Low → Medium → High → Extra high → Max**.
A task's named effort wins over an entry's effort, which wins over the tier default.
**Quick** verification lowers the default by one effort step, but does not lower
an effort you explicitly chose. Effort is adjusted to a supported setting (the
highest at or below the request, or the model's lowest if none is that low).
Models without effort controls receive no effort setting. A resumed conversation
keeps the effort it started with.

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
to **Settings ▸ Model levels**. If it can't serve the task, normal routing takes
over and records why. Naming a model does not by itself name an effort.

Naming a model placed in Premium asks for premium. If that model can't serve,
another Premium model runs; if none can, premium runs on High (then Medium, then
Low), and the routing record names each Premium model skipped and why.

For a standing preference, **Settings ▸ Routing ▸ Always use** pins one model,
with an optional fallback. Task-specific model choices take priority; premium
requests and stronger-model retries bypass the pin. If neither pinned model can
serve, normal routing takes over. The Quartermaster's own model and effort
(and secondmates') are separate controls on the same tab.

## Availability and fallbacks

Routing skips models that cannot handle the task's images or context, and accounts
at their limit. Within a tier, it prefers your preferred crew account, then
subscriptions over API keys, allowance at risk of going unused, and newer models
in the same family.

If no model can serve in the chosen level, it tries **Low → Medium → High**,
**Medium → High → Low**, or **High → Medium → Low**, keeping the original level's
default effort, capped to the model used. Premium work (asked for, set for a
task kind, or a retry with a premium model) tries **Premium → High → Medium → Low**. Premium is never a fallback
for other levels, and a retry with a high or medium model waits for that level. The routing record explains the choice and any fallback.
