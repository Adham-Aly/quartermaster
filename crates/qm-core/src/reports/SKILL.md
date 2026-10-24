# Static reports

For a **done lookup, research, reproduce or teach** task, and a **done review under normal
autonomy**, deliver a complete document in this folder before calling `report`. This is an
app-authored skill, not project instructions.

## Files and workflow

1. Read this skill and the seeded `template.html` in `$QM_REPORT_DIR`.
2. Copy `template.html` to `report.html`, then customize the copy for the task. Keep the template
   as the baseline; replace its sample content and remove unused sections in your report.
3. Put supporting images, diagrams and other local resources in `assets/`, referenced with
   relative paths such as `assets/result.png`. Keep the report and its assets together. These
   artifacts are outside the worktree: never commit them. Do not change this skill.
4. Open and visually check the finished `report.html` before reporting done.

## Write for a person

Lead with the answer or conclusion, then explain the evidence and reasoning. Give a reader who
wasn't there enough context to understand the result without reading a transcript. Use clear
headings, concise prose, labeled tables and relevant visuals, not decoration. Retain technical
depth: exact commands, file paths and line references, versions, assumptions, limitations and
uncertainty wherever they matter. Distinguish observation from inference. Cite sources with
ordinary links and useful labels; don't invent evidence or imply checks you didn't run.

- **Lookup:** give the direct answer and the sources that establish it.
- **Research:** explain findings, alternatives, tradeoffs and a justified recommendation; note
  gaps and any confirmed diagnosis a later fix can build on.
- **Reproduce:** state `reproduced: yes` or `reproduced: no`, the isolated environment, exact
  steps, expected versus actual results, and evidence. Never use the user's real state.
- **Teach:** build a coherent explanation with examples, diagrams and practical details suited
  to the reader; make the important concepts understandable without sacrificing correctness.
- **Review:** lead with the verdict and the risk with its reason, then each finding with its
  severity, file and line, what is wrong and why it matters, and the action it asks for. Say what
  you reviewed (the commits or diff) and how. The user reads it before anyone acts on the findings,
  so write it for them; don't decide what happens next.

## A document, not an app

The report must be a static, offline-readable HTML document. No forms, inputs, submissions,
interactive widgets, navigation that hides essential content, or other interaction except a
light/dark theme toggle, which you must retain from the template. All essential content must be
visible without JavaScript. Only the theme toggle may use script; preserve the template's offline
Content Security Policy and do not add remote scripts, libraries, stylesheets,
fonts, images, iframes, analytics, network fetches or other remote dependencies. Use local assets
(or inline SVG/CSS) for visuals. External source links are fine: they are citations a reader can
choose to visit, not resources the document needs to load. Do not embed secrets or private state.

## Visual checks

Render the finished file in a browser and inspect it at a normal desktop width and a narrow
viewport. Check the complete document, not just its first screen: readable typography, clear
hierarchy, contrast, spacing, wrapping code and tables, no clipping or horizontal overflow,
working local images, legible diagram labels, and meaningful image alt text. Check both themes.
Confirm it still makes sense offline with no missing remote resources.
Fix placeholder content, broken paths and rendering defects. If a check cannot be performed,
say exactly what remains unchecked; never claim a visual check from source inspection alone.

## The tool contract still applies

The HTML artifact does not replace the `report` tool's answer. Put the **whole answer** in
`answer`, not merely a summary or a path to `report.html`. For reproduce, open the answer with
`reproduced: yes` or `reproduced: no` and set the tool's `reproduced` field to match. Keep any
required research diagnosis metadata too, and a review's verdict, findings and risk in `review`. Blocked or needs-decision reports do not require a
finished static document; explain what stops you through the normal reporting contract.
