---
name: quartermaster-plan
description: How to write plan.md for a plan that goes straight to the build - the format, images and drawings, and the record of decisions you made alone. Use whenever you write the plan.
---

# Writing a plan that goes straight to the build

The project runs on auto: nobody reviews this plan before the work starts. Decide every open point
yourself, from the code, the brief and the project's own conventions, and record each decision
and why. Stop to ask your supervisor only when you cannot go on without a choice nobody else can
make, such as a step that would lose data, spend money or break a stated constraint.

## The steps

1. Read the code and the brief. List every decision the work depends on, and decide each one.
2. Write `plan.md`.
3. Call `plan_check`. The app checks the file. Fix anything it reports and call again.
4. Call `report` (done) with a short summary, never the plan itself. The build starts from the
   plan.

## Format

- `plan.md` is a technical handoff to the implementing agent, not a user-facing presentation.
  Include code findings and file paths, architecture, implementation steps, constraints, tests
  and acceptance criteria. Keep it precise enough for another agent to execute independently.
  Do not create `plan.html` in this mode.
- One `# Title`, then `##` sections and `###` subsections.
- A `## Decisions made without the user` section is required: each open point you decided, the
  choice, and the reason in a line. Write "None" if there were none.
- Code fences highlight by language. `svg` fences draw inline (a safe subset: shapes, paths,
  text, gradients; no scripts, links out or external images). `math` fences and `$…$` render
  maths. `mockup` fences show HTML in a sandboxed frame (`mockup height=300`). `terminal` fences
  show ANSI colours. There is no Mermaid: draw diagrams as SVG.
- Images go in the plan folder's `assets/` and are linked as `![alt](assets/name.png)` (PNG,
  JPEG, GIF, WebP or SVG). They are copied to the build.
- Links may be `http`, `https`, `mailto` or `#anchor`. Raw HTML is shown as text.
