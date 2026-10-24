---
name: quartermaster-plan
description: Author the user's plan.html separately from the agent's technical plan.md, with questions, diagrams, and review controls. Use whenever writing or revising an interactive plan.
---

# Planning with the user

Write two distinct documents in the plan folder:
- `plan.md`: the technical handoff to the implementing agent. Include code findings, file paths,
  architecture, implementation steps, constraints, tests and acceptance criteria. Keep it current
  with the user's decisions, but do not duplicate the review UI or put question fences here.
- `plan.html`: the user-facing explanation YOU author. Start with $QM_PLAN_DIR/template.html.
  Explain the outcome, trade-offs, approach and open decisions in a layout suited to this task.
  The app preserves your markup, adding only trusted metadata and interactive controls. It does
  not generate your explanation from the technical Markdown.

## The loop

1. Read the brief and code. Identify every decision you would otherwise assume.
2. Write both documents. In round 1 ask at least one meaningful question in `plan.html`.
3. Call `plan_check`; fix all reported problems and check again.
4. Call `report` with a short summary, not the documents. The app presents the review to the user.
   Stop: do not implement or guess answers. Answers arrive in a `<plan-reply …>` block.
5. Fold answers into both documents, remove answered question placeholders, and ask the next open
   questions. Challenge an answer when the code suggests a problem and explain why.
6. Only after the user has answered a round (or asked you to take recommendations), remove all
   questions for the final review. Check and report again. Implementation waits for approval.

## HTML authoring contract

`plan_check` composes plan.html in place and retains your authored source internally. For each
revision replace plan.html with the complete new authored fragment, never edit the generated
runtime. The app reuses your retained source when refreshing status or the endpoint.

`plan.html` is an HTML **fragment** with one `<article>` root, not a full document. Do not copy
runtime output back into it. Use XML-well-formed syntax: close elements, self-close `<img />`,
`<br />`, `<hr />`, quote attributes, and escape `&` as `&amp;` and `<` as `&lt;` in text.
There are no declarations, comments, scripts, event handlers, namespaces or external resources.

- Exactly one plain-text `<h1>` title. Use plain-text `<h2 id="approach">` and `<h3 id="…">`
  headings for reviewable sections. IDs are unique lowercase `a-z`, digits and hyphens, at most
  64 bytes. Keep them stable across revisions. Runtime IDs and the `q-` prefix are reserved.
  The app adds outline links, updated badges, Suggest a change and Ask controls to each section.
- Include `<h2 id="assumptions">Assumptions</h2>` listing remaining assumptions (or None).
  Turn each important assumption the user could disagree with into a question.
- Compose prose using article, section, div, span, p, ul/ol/li, dl/dt/dd, strong/em/b/i/s/small,
  blockquote, pre/code, a, img, br/hr, table/thead/tbody/tr/th/td, figure/figcaption, details/summary.
  Attributes: class (names must start `author-`), style, title, aria-label, href on links,
  src on images, alt, width, height, colspan, rowspan. Use inline styles for your custom layout.
  Avoid fixed light/dark colors: the surrounding UI supports both themes. Use `var(--fg)`,
  `var(--bg)`, `var(--muted)`, `var(--soft)`, `var(--line)` and `var(--accent)` for custom layouts.
  Keep content in normal flow, responsive on narrow screens, and never cover review controls.
- Links use http, https, mailto or #anchor. Images use `src="assets/name.png"` (PNG/JPEG/GIF/WebP/SVG).
  The app embeds assets; it never fetches remote images. Keep files directly in `assets/`.
- Rich displays use `<pre data-render="svg">escaped SVG</pre>`, or kinds `mockup`, `math`,
  `terminal`, `rust`, `json`, `text`. SVG is cleaned; mockups use the existing sandbox; maths
  uses bundled KaTeX. No Mermaid. Escape markup inside these placeholders; do not nest elements
  or include Markdown triple-backtick fences. Everything outside placeholders is preserved.

## Questions live in HTML

Place each question after the h2/h3 it concerns. The placeholder contains escaped TOML using
exactly the same question fields as below. The app supplies typed answer, multiple-choice,
Other, recommendation and note widgets; do not hand-build inputs or buttons.

```html
<pre data-question="">id = "storage-engine"
ask = "Which storage engine should the cache use?"
why = "This determines migration complexity."
multiple = false
choices = [
  { id = "sqlite", label = "SQLite", detail = "Already a dependency", recommended = true },
  { id = "files", label = "Files", detail = "Simpler initial setup" },
]</pre>
```

Question and choice IDs are unique `[a-z0-9-]`, at most 40 bytes. Use two to nine choices, or
omit choices for a typed open answer. Other is available by default; `other = false` disables it.
At most one recommendation unless `multiple = true`. At most 60 questions per document.
Keep ask short; put reasoning in why. Never assign the user's answer yourself.

## Writing and visual clarity

Write for humans: simple, clear and easy to follow, with full relevant technical depth. Explain
proposed code, feature and behaviour changes, alternatives and risks without irrelevant detail.
Encourage understanding with diagrams, visuals and relevant images wherever they help. Keep the
technical Markdown comprehensive and synchronized, rather than hiding important changes there.

## Replies

Replies identify the task, plan and round. `Choice`, `Other`, `Answer` and `Note` belong to a
question. `Left open.` means it was skipped: ask again or explain how it is resolved under
Assumptions. Section `Change` and `Ask` entries are requested edits and discussion: apply changes
or answer in the document. Stable section/question IDs preserve that connection. Copy/send,
saved responses, theme, discussion and the approval/locked state are app-owned.
