---
name: quartermaster-plan
description: How to write plan.md for an interactive plan - the format, the question blocks, images and drawings, and how the user's answers come back. Use whenever you write or revise the plan.
---

# Writing an interactive plan

You are planning with the user, not for them. Your job is to grill them on every point you would
otherwise have to assume, one round at a time, until nothing important is left open. The app turns
your `plan.md` into a page the user reads in a browser; they answer there and paste the reply back.

## The loop

1. Read the code and the brief. List every decision the plan depends on.
2. Write `plan.md` with the plan so far and a question block for each open point. Round 1 must
   ask at least one question.
3. Call `plan_ready`. The app checks the file, renders the page and posts its card. Fix anything
   it reports and call again.
4. Stop and wait. The user's answers arrive as a `<plan-reply …>` block, sent on by the
   Quartermaster or secondmate. Never guess an answer and never start the work yourself.
5. Fold the answers into the plan, remove the answered questions, and ask the next round. Only
   when nothing is left open, and the user has answered at least one round (or told you to take
   your recommendations), write the plan with no question blocks: that is the final plan. The user
   approves it; the build starts only then.

Push back when an answer looks wrong for the code you have read, and say why in the next round.

## Format

- One `# Title`, then `##` sections and `###` subsections. Each gets an anchor, a "Suggest a
  change" and an "Ask" button, and an "updated" badge when it changes between rounds.
- A `## Assumptions` section is required: everything you still assume. Each assumption the user
  could reasonably disagree with should be a question instead.
- Put each question block in the section it concerns:

  ````markdown
  ```question
  id = "storage-engine"
  ask = "Which storage engine should the cache use?"
  why = "It decides the migration story in the Data flow section."
  multiple = false
  choices = [
    { id = "sqlite", label = "SQLite", detail = "already a dependency", recommended = true },
    { id = "sled",   label = "sled",   detail = "embedded, but unmaintained" },
  ]
  ```
  ````

  `id` is `[a-z0-9-]`, unique. Two to nine choices, at most one `recommended` unless
  `multiple = true`. An "Other" box is offered unless `other = false`. No `choices` makes an open
  question with a text box. Keep `ask` short and put the trade-offs in `why`.
- Code fences highlight by language. `svg` fences draw inline (a safe subset: shapes, paths,
  text, gradients; no scripts, links out or external images). `math` fences and `$…$` render
  maths. `mockup` fences show HTML in a sandboxed frame (`mockup height=300`). `terminal` fences
  show ANSI colours. There is no Mermaid: draw diagrams as SVG.
- Images go in the plan folder's `assets/` and are linked as `![alt](assets/name.png)` (PNG,
  JPEG, GIF, WebP or SVG). They are embedded in the page and copied to the build.
- Links may be `http`, `https`, `mailto` or `#anchor`. Raw HTML is shown as text.

## The reply

```text
## Q1 storage-engine
Choice: sqlite (SQLite)
Note:
> keep WAL off for now

## Section data-flow (Data flow)
Change:
> put the cache in front of the queue
```

`Choice`, `Other` and `Answer` answer a question; `Left open.` means the user skipped it, so ask
again or decide it in the next round and say so under Assumptions. `Change` and `Ask` under a
section are edits and questions about that section: make the change, or answer in the plan.
