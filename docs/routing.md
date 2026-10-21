# Model routing

Quartermaster routes crew tasks by **task kind**, then chooses an available model
from your model levels (tiers). The rules use the kind and checked task facts,
not keywords in the task description.

## Default rules

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

## Choose your model levels

Levels are pools **you control**, not fixed model names:

| Level | Purpose | Default effort |
|---|---|---|
| Premium | Above High; only used when you request it or choose a stronger-model retry | Extra high |
| High | The default for work needing thinking | Extra high |
| Medium | Work implementing finished thinking | Medium |
| Low | Lookups | Medium |

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

The app records choices for later steps as `later_steps`, by task kind. They
apply when a matching task builds on that work, directly or further down the
`builds_on` chain. The nearest matching entry wins; a newer explicit choice for
the task overrides it. A stronger-model retry ignores inherited choices.

A named model must be in a tier and able to serve the task; otherwise normal
routing takes over and records why. Naming a model does not by itself name an effort.

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
default effort, capped to the model used. Premium is never an automatic fallback:
premium requests wait for Premium, and stronger-model retries wait for their
target level. The routing record explains the choice and any fallback.
