//! The crewmate's brief and the crewmate's system prompt.
//!
//! **The brief** is the crewmate's first message, rendered by the app from the facts the core
//! recorded when the task started ([`BriefFacts`]) and saved as `tasks/C-0042/brief.md` exactly
//! as sent. Its sections, in order: the header lines, Goal, the user's own
//! words, the one-time instructions, Constraints, Builds on, Before your first edit, Verification,
//! Delivery and Reporting. A section with nothing to say is left out (no instructions, no
//! constraints, nothing to build on, no words of the user's); the rest change with the task's
//! kind, delivery mode and verification level.
//!
//! The whole brief sits inside one supervisor marker (`<qm:supervisor from="quartermaster"
//! task="C-0042">`). Every piece of text that didn't come from this module (the goal,
//! the user's words, instructions, constraints, the summary and answer it builds on) is
//! neutralised with [`qm_policy::markers::neutralise`] here, so nothing in it can open or close a
//! marker, whatever the caller passed.
//!
//! **The crewmate prompt** ([`crewmate_prompt`]) is the static `prompts/crewmate.md`, preceded on
//! the provider engine by `prompts/crewmate-provider.md` (which stands in for Claude Code's own
//! preset), then the instruction files (your `~/.agents/AGENTS.md` first, then the project's),
//! then, on the provider engine only, the skills list. On Claude it is appended to Claude Code's
//! `claude_code` preset, which already lists the skills from the plugin folder.
//!
//! Rendering is pure: the core passes in facts, this module returns text.

use std::fmt::Write as _;

use qm_policy::markers::neutralise;
use qm_types::domain::{DeliveryMode, PlanMode, TaskKind, VerificationLevel, VerificationSource};
use qm_types::ids::{CommitSha, PortBlock, SecondmateId, TaskId, TaskLabelId};

use crate::index;

#[cfg(test)]
#[path = "brief_tests.rs"]
mod tests;

/// Where free text (the user's words, a builds-on summary) wraps, in characters per line,
/// counting the `> "` quote prefix.
pub const WRAP: usize = 88;

/// The column the `[structured: …]` tag of a constraint starts at.
pub const CONSTRAINT_TAG_COLUMN: usize = 59;

/// The most of a builds-on answer the brief embeds, in characters; the rest is pointed to.
pub const BUILDS_ON_ANSWER_CHARS: usize = 16_000;

/// Who started the task: the name in the supervisor marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BriefSupervisor {
    /// `from="quartermaster"`.
    Quartermaster,
    /// `from="S-02"`.
    Secondmate(SecondmateId),
}

/// The task's verification level as the brief states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BriefVerification {
    /// No level: answering and review tasks, and any task of a build-in-place project.
    None,
    /// A level, where it came from, and whether Careful ships as standard because the project
    /// doesn't deliver by pull request (`qm_policy::verification::effective`).
    Level {
        level: VerificationLevel,
        source: VerificationSource,
        ships_as_standard: bool,
    },
}

/// One constraint, with its structured part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BriefConstraint {
    /// The constraint in words: "Don't change the public /auth API."
    pub text: String,
    /// Path globs the task must not change: `/auth/*`.
    pub freeze: Vec<String>,
}

/// What the task builds on: a finished plan, a reproduction, a diagnosis, the work a review checks,
/// or the failed attempt a stronger-model retry carries on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BriefBuildsOn {
    pub task: TaskId,
    pub kind: TaskKind,
    /// The fact the app checked, as the header line shows it: `reproduced: yes`, `finished plan`,
    /// `confirmed diagnosis`, `the work under review at a1b2c3d`, `the failed attempt this task
    /// retries`, or `not a finished plan or confirmed diagnosis`.
    pub fact: String,
    /// One paragraph saying what it found, wrapped at [`WRAP`]; " Full answer below." follows.
    pub summary: String,
    /// The full answer or report, embedded on the next lines; cut at
    /// [`BUILDS_ON_ANSWER_CHARS`] with a pointer to `answer_path`.
    pub body: String,
    /// Where the full text lives, for when the body is cut: `tasks/C-0072/builds-on.md`.
    pub answer_path: String,
}

/// Where a crewmate works. A branch is cut from a settled base; a folder is worked in place, its
/// files the work, with nothing to commit: a plain folder, or a repository's own folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BriefWorkspace {
    /// The task's own branch, `branch` (`tasks.branch`: `fix/upload-retry-on-slow-tmp`).
    /// `start_ref` is where it starts, as shown: `origin/main` (`tasks.base_ref`), or for a
    /// review task the builder's branch. `start_commit` is the commit it starts at
    /// (`tasks.base_commit`).
    Branch {
        branch: String,
        start_ref: String,
        start_commit: CommitSha,
    },
    /// The folder the task edits directly: a plain folder, or, when `repository`, the user's
    /// checkout in place.
    Folder { path: String, repository: bool },
}

/// Everything a brief says, recorded by the core when the task started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BriefFacts {
    pub task: TaskId,
    /// The task (`T-`) the crewmate belongs to when it starts.
    pub label: TaskLabelId,
    pub kind: TaskKind,
    pub supervisor: BriefSupervisor,
    /// The project's name.
    pub project: String,
    pub delivery: DeliveryMode,
    /// Whether the project has a remote (plain folders and local-only projects don't).
    pub has_remote: bool,
    pub verification: BriefVerification,
    /// Where the task works: its branch, or a plain folder.
    pub workspace: BriefWorkspace,
    /// The branch a pull request targets (direct pull request only): `main`, `release/2.4`.
    pub pr_target: Option<String>,
    /// The task's block of ten ports.
    pub ports: PortBlock,
    /// The supervisor's goal: its understanding of done.
    pub goal: String,
    /// The user's own words, copied by the app (empty when there are none).
    pub user_words: String,
    /// The supervisor left passages about other work out of them.
    pub user_words_trimmed: bool,
    /// One-time instructions from the supervisor, for this task only.
    pub instructions: Vec<String>,
    pub constraints: Vec<BriefConstraint>,
    /// The "tests required" constraint.
    pub tests_required: bool,
    pub builds_on: Option<BriefBuildsOn>,
    /// Failed repairs of its own regression allowed before the app holds the task
    /// (`qm_policy::repair::MAX_FAILED_REPAIRS`).
    pub repair_attempts: u8,
}

/// Renders the brief exactly as it is sent and saved.
pub fn render_brief(f: &BriefFacts) -> String {
    let mut out = String::new();
    let from = match f.supervisor {
        BriefSupervisor::Quartermaster => "quartermaster".to_owned(),
        BriefSupervisor::Secondmate(id) => id.to_string(),
    };
    let _ = writeln!(out, "<qm:supervisor from=\"{from}\" task=\"{}\">", f.task);
    header(&mut out, f);
    section_goal(&mut out, f);
    section_user_words(&mut out, f);
    section_instructions(&mut out, f);
    section_constraints(&mut out, f);
    section_builds_on(&mut out, f);
    section_planning(&mut out, f);
    section_first_edit(&mut out, f);
    section_verification(&mut out, f);
    section_delivery(&mut out, f);
    section_reporting(&mut out, f);
    out.push_str("</qm:supervisor>\n");
    out
}

fn is_review(kind: TaskKind) -> bool {
    kind == TaskKind::Review
}

fn delivery_words(delivery: DeliveryMode) -> &'static str {
    match delivery {
        DeliveryMode::DirectPr => "direct pull request",
        DeliveryMode::BranchOnly => "branch only",
        DeliveryMode::BuildInPlace => "build in place",
        DeliveryMode::Folder => "plain folder",
    }
}

/// "the commit" for a branch, "your edits" for a folder: what a check or a review is about.
fn final_state(f: &BriefFacts) -> &'static str {
    match f.workspace {
        BriefWorkspace::Branch { .. } => "your final commit",
        BriefWorkspace::Folder { .. } => "your final edits",
    }
}

fn source_words(source: VerificationSource) -> &'static str {
    match source {
        VerificationSource::User => "the user's words",
        VerificationSource::Project => "project setting",
        VerificationSource::Default => "default",
    }
}

/// The level the task runs under, if any.
fn running_level(f: &BriefFacts) -> Option<VerificationLevel> {
    match f.verification {
        BriefVerification::None => None,
        BriefVerification::Level {
            ships_as_standard: true,
            ..
        } => Some(VerificationLevel::Standard),
        BriefVerification::Level { level, .. } => Some(level),
    }
}

fn header(out: &mut String, f: &BriefFacts) {
    let _ = writeln!(out, "# Brief · {} in {} · {}", f.task, f.label, f.kind);
    let verification = match f.verification {
        BriefVerification::None => {
            let why = if is_review(f.kind) {
                "review task"
            } else if f.kind.usually_answers() {
                "usually an answer"
            } else {
                "build in place"
            };
            format!("none ({why})")
        }
        BriefVerification::Level {
            level,
            source,
            ships_as_standard,
        } => {
            if ships_as_standard {
                format!(
                    "standard ({}: {level}, which ships as standard without a pull request)",
                    source_words(source)
                )
            } else {
                format!("{level} ({})", source_words(source))
            }
        }
    };
    let delivery = match f.workspace {
        BriefWorkspace::Folder {
            repository: true, ..
        } => "in place",
        _ => delivery_words(f.delivery),
    };
    let _ = writeln!(
        out,
        "Project {} · Delivery {delivery} · Verification {verification}",
        neutralise(&f.project),
    );
    let fate = if is_review(f.kind) {
        "review only: change and commit nothing".to_owned()
    } else if f.kind.usually_answers() {
        "answer only: commit nothing".to_owned()
    } else {
        match (f.delivery, &f.pr_target) {
            (DeliveryMode::DirectPr, Some(target)) => {
                format!("the pull request targets {}", neutralise(target))
            }
            (DeliveryMode::DirectPr, None) => "the pull request targets the default branch".into(),
            (DeliveryMode::BranchOnly, _) if f.has_remote => "settling pushes this branch".into(),
            (DeliveryMode::BranchOnly, _) => "the branch stays in this repository".into(),
            (DeliveryMode::BuildInPlace, _) => "settling merges it into the user's checkout".into(),
            (DeliveryMode::Folder, _) => "the edits there are the delivery".into(),
        }
    };
    match &f.workspace {
        BriefWorkspace::Branch {
            branch,
            start_ref,
            start_commit,
        } => {
            let _ = writeln!(
                out,
                "Branch {} from {} @ {} · {fate}",
                neutralise(branch),
                neutralise(start_ref),
                start_commit.short()
            );
            out.push_str(
                "Evidence folder $QM_EVIDENCE_DIR, outside the worktree; never commit it\n",
            );
        }
        BriefWorkspace::Folder { path, repository } => {
            let git = if *repository {
                "a repository, worked in place: no branch of your own"
            } else {
                "no branch, no git"
            };
            let _ = writeln!(out, "Folder {} · {git} · {fate}", neutralise(path));
            out.push_str("Evidence folder $QM_EVIDENCE_DIR, outside the folder you work in\n");
        }
    }
    let ports = f.ports.ports();
    let _ = writeln!(
        out,
        "Ports {}–{} ($QM_PORT_BASE) for anything that listens",
        ports.start(),
        ports.end()
    );
}

fn section_goal(out: &mut String, f: &BriefFacts) {
    out.push_str("\n## Goal\n");
    push_block(out, &neutralise(f.goal.trim()));
}

fn section_user_words(out: &mut String, f: &BriefFacts) {
    let words = neutralise(f.user_words.trim());
    if words.is_empty() {
        return;
    }
    out.push_str(
        "\n## The user's own words (copied by the app; these outrank everything else here)\n",
    );
    let quoted = format!("\"{words}\"");
    let mut first = true;
    for line in quoted.lines() {
        if line.trim().is_empty() {
            out.push_str(">\n");
            continue;
        }
        for piece in wrap(line, WRAP - 3) {
            out.push_str(if first { "> " } else { ">  " });
            out.push_str(&piece);
            out.push('\n');
            first = false;
        }
    }
    if f.user_words_trimmed {
        out.push_str(TRIMMED_NOTE);
    }
}

/// Said under the user's words when the supervisor left parts of their message out.
const TRIMMED_NOTE: &str =
    "Your supervisor left out parts of this message that were about other work.\n";

fn section_instructions(out: &mut String, f: &BriefFacts) {
    let items: Vec<String> = f
        .instructions
        .iter()
        .map(|i| neutralise(i.trim()))
        .filter(|i| !i.is_empty())
        .collect();
    if items.is_empty() {
        return;
    }
    out.push_str("\n## Instructions for this task only (from your supervisor)\n");
    for item in items {
        push_bullet(out, &item);
    }
}

fn section_constraints(out: &mut String, f: &BriefFacts) {
    if f.constraints.is_empty() && !f.tests_required {
        return;
    }
    out.push_str("\n## Constraints\n");
    for c in &f.constraints {
        let text = format!("- {}", neutralise(c.text.trim()));
        if c.freeze.is_empty() {
            out.push_str(&text);
        } else {
            let globs: Vec<String> = c.freeze.iter().map(|g| neutralise(g.trim())).collect();
            push_tagged(out, &text, &format!("freeze={}", globs.join(",")));
        }
        out.push('\n');
    }
    if f.tests_required {
        push_tagged(
            out,
            "- Tests are required: cover this change with tests.",
            "tests_required",
        );
        out.push('\n');
    }
}

fn push_tagged(out: &mut String, text: &str, tag: &str) {
    out.push_str(text);
    let used = text.chars().count();
    let pad = if used + 2 <= CONSTRAINT_TAG_COLUMN {
        CONSTRAINT_TAG_COLUMN - used
    } else {
        2
    };
    out.extend(std::iter::repeat_n(' ', pad));
    let _ = write!(out, "[structured: {tag}]");
}

fn section_builds_on(out: &mut String, f: &BriefFacts) {
    let Some(b) = &f.builds_on else {
        return;
    };
    out.push_str("\n## Builds on\n");
    let _ = writeln!(
        out,
        "{} ({}) · {}",
        b.task,
        b.kind,
        neutralise(b.fact.trim())
    );
    let summary = neutralise(b.summary.trim());
    let lead = if summary.is_empty() {
        "Full answer below.".to_owned()
    } else {
        format!("{summary} Full answer below.")
    };
    for line in wrap(&lead, WRAP) {
        out.push_str(&line);
        out.push('\n');
    }
    let body = neutralise(b.body.trim());
    if body.chars().count() > BUILDS_ON_ANSWER_CHARS {
        let cut: String = body.chars().take(BUILDS_ON_ANSWER_CHARS).collect();
        push_block(out, cut.trim_end());
        let _ = writeln!(
            out,
            "… (cut here; the rest is in {})",
            neutralise(&b.answer_path)
        );
    } else if !body.is_empty() {
        push_block(out, &body);
    }
    if b.kind == TaskKind::Plan {
        out.push_str(
            "The plan itself is your copy in $QM_PLAN_DIR: plan.md, with its images in assets/.\n\
             Read it whole before you start, and follow it.\n",
        );
    }
}

/// A plan task's planning lines. They say nothing of the plan mode: the mode's own prompt and
/// skill say how, so a mode change needs no new brief.
fn section_planning(out: &mut String, f: &BriefFacts) {
    if f.kind != TaskKind::Plan {
        return;
    }
    out.push_str(
        "\n## Planning\n\
         Write $QM_PLAN_DIR/plan.md as a highly detailed, comprehensive technical implementation\n\
         handoff another agent can execute independently, with findings, paths, architecture, steps,\n\
         constraints, edge cases, tests and acceptance criteria. Put images in $QM_PLAN_DIR/assets/.\n\
         Read the supplied quartermaster-plan skill before authoring. Under normal autonomy, also\n\
         author plan.html from template.html as an XML-well-formed HTML article fragment: a simple,\n\
         clear human-facing explanation with relevant technical depth and helpful visuals, never\n\
         sacrificing detail. Keep both documents current every round. The app wraps your authored\n\
         HTML with trusted interactive runtime, not prose generated from Markdown. plan_check\n\
         composes plan.html in place and retains your source internally; for revisions replace the\n\
         whole authored fragment, never edit generated runtime. Under auto autonomy there is no\n\
         HTML template or review page; write only plan.md. Your system prompt and skill give the\n\
         full planning loop. Call plan_check and fix all problems before every report. The plan\n\
         lives only in that folder: never paste it in a report or a message.\n",
    );
}

fn section_first_edit(out: &mut String, f: &BriefFacts) {
    if f.kind.usually_answers() {
        return;
    }
    out.push_str("\n## Before your first edit\n");
    out.push_str("Call delivery_check. Close any gap you can; report the rest as blockers.\n");
}

fn section_verification(out: &mut String, f: &BriefFacts) {
    let rebase = match &f.workspace {
        BriefWorkspace::Branch { start_ref, .. } => format!(
            "Rebase onto {} and resolve conflicts yourself. Never squash your own commits.\n",
            neutralise(start_ref)
        ),
        BriefWorkspace::Folder { .. } => String::new(),
    };
    match f.verification {
        BriefVerification::None => {
            out.push_str("\n## Verification: none\n");
            out.push_str(if is_review(f.kind) {
                "Check every changed file, and the builder's Verified section and evidence against\n\
                 the user's words. Run what you need to confirm a claim; change nothing.\n"
            } else if f.kind == TaskKind::Reproduce {
                "Reproduce it with product-level evidence saved to $QM_EVIDENCE_DIR: the exact steps,\n\
                 what happened and what should have. Use throwaway data, never the user's real state.\n"
            } else if f.kind.usually_answers() {
                "Back every claim with what you read or ran: file paths and lines, commands and\n\
                 their output, links. Say plainly what you couldn't confirm.\n"
            } else {
                "Build in place has no level: nothing about CI or evidence is required.\n"
            });
        }
        BriefVerification::Level {
            level,
            ships_as_standard,
            ..
        } => {
            let running = running_level(f).unwrap_or(level);
            if ships_as_standard {
                out.push_str(
                    "\n## Verification: standard (Careful needs a pull request, so it ships as standard)\n",
                );
            } else {
                let _ = writeln!(out, "\n## Verification: {running}");
            }
            if running == VerificationLevel::Quick {
                if f.delivery == DeliveryMode::DirectPr {
                    out.push_str(
                        "CI must be green on your final commit. Nothing more is required: no evidence,\n\
                         and the report headings are optional. Never squash your own commits.\n",
                    );
                } else {
                    let _ = writeln!(
                        out,
                        "Name the local checks you ran on {} under Verified. Nothing more is\n\
                         required: no evidence, and the other headings are optional. Never squash your own\n\
                         commits.",
                        final_state(f)
                    );
                }
                return;
            }
            out.push_str(&rebase);
            out.push_str(
                "Evidence first: what the user would see, before your first edit and after your last,\n\
                 saved to $QM_EVIDENCE_DIR. Unit tests and reading code are not evidence on their own.\n\
                 Run the smallest set of existing tests that would catch a user-visible break, including\n\
                 end-to-end tests over what you changed. Read your whole diff against the user's words.\n\
                 Update any docs the change makes stale.\n",
            );
            if f.delivery != DeliveryMode::DirectPr {
                let _ = writeln!(
                    out,
                    "There's no pull request to run CI: name the local checks you ran on {} under Verified.",
                    final_state(f)
                );
            }
            if running == VerificationLevel::Reviewed {
                out.push_str(if f.delivery == DeliveryMode::Folder {
                    "Then an independent review: report done without changing anything more. Fix what\n\
                     it finds when your supervisor passes it on.\n"
                } else {
                    "Then an independent review: commit locally and report done without pushing or\n\
                     opening the pull request. Fix what it finds when your supervisor passes it on.\n"
                });
            }
        }
    }
}

fn section_delivery(out: &mut String, f: &BriefFacts) {
    out.push_str("\n## Delivery\n");
    if is_review(f.kind) {
        out.push_str(
            "You change nothing, commit nothing, push nothing and open nothing. The review is your\n\
             report.\n",
        );
        return;
    }
    if f.kind == TaskKind::Plan {
        out.push_str(
            "This task delivers a plan, not code. Don't commit, push or open anything; the plan goes\n\
             in its plan folder, and scratch work in your temporary folder.\n",
        );
        return;
    }
    if f.kind.usually_answers() {
        out.push_str(
            "This task delivers an answer, not code. Don't commit, push or open anything; put\n\
             scratch work in your temporary folder.\n",
        );
        return;
    }
    match f.delivery {
        // Quick has no failed-check rules ("CI is green. Nothing more."), so no diagnosis
        // and no repair count.
        DeliveryMode::DirectPr if running_level(f) == Some(VerificationLevel::Quick) => out.push_str(
            "Open the pull request with pr_open, in your own words, with no task IDs. Keep going\n\
             until every check on your latest commit passes; the app wakes you with each result.\n",
        ),
        DeliveryMode::DirectPr => {
            if running_level(f) == Some(VerificationLevel::Reviewed) {
                out.push_str(
                    "Once the review passes, open the pull request with pr_open, in your own words, \
                     with no task IDs.",
                );
            } else {
                out.push_str(
                    "Open the pull request with pr_open, in your own words, with no task IDs.",
                );
            }
            let _ = write!(
                out,
                " If a check fails, read the logs and declare a\n\
                 diagnosis with ci_diagnosis before repairing. You have {} repair attempts for your own\n\
                 regression. Keep going until every check on your latest commit passes; the app wakes\n\
                 you with each result.\n",
                f.repair_attempts
            );
        }
        DeliveryMode::BranchOnly if f.has_remote => out.push_str(
            "Commit your work on this branch. Don't push and don't open a pull request: settling\n\
             pushes the branch, and the user merges it. If GitHub runs checks on the pushed branch\n\
             and one fails, the app wakes you with it: fix it, commit and report again.\n",
        ),
        DeliveryMode::BranchOnly => out.push_str(
            "Commit your work on this branch. It stays in this repository, where the user merges or\n\
             deletes it.\n",
        ),
        DeliveryMode::BuildInPlace => out.push_str(
            "Commit your work on this branch. Settling merges it into the branch the user's checkout\n\
             has checked out; never touch the checkout yourself.\n",
        ),
        DeliveryMode::Folder
            if matches!(
                f.workspace,
                BriefWorkspace::Folder {
                    repository: true,
                    ..
                }
            ) =>
        {
            out.push_str(
                "Work directly in the repository's own folder, on whatever its checkout has checked out.\n\
                 Don't commit, branch or push unless the user asks: your edits are final, and settling\n\
                 marks the task done; nothing is undone when the task closes.\n",
            );
        }
        DeliveryMode::Folder => out.push_str(
            "Work directly in the folder. There's no git here: nothing to commit, push or open, and\n\
             don't make it a repository unless the user asks. Your edits are final, and settling marks\n\
             the task done; nothing is undone when the task closes.\n",
        ),
    }
}

fn section_reporting(out: &mut String, f: &BriefFacts) {
    out.push_str("\n## Reporting\n");
    if matches!(
        f.kind,
        TaskKind::Lookup | TaskKind::Research | TaskKind::Reproduce | TaskKind::Teach
    ) {
        out.push_str(
            "Before reporting done, read $QM_REPORT_DIR/SKILL.md. Copy $QM_REPORT_DIR/template.html\n\
             to $QM_REPORT_DIR/report.html and customize it into your complete static report, with\n\
             local supporting files in $QM_REPORT_DIR/assets/. Write for human clarity with technical\n\
             depth and relevant visuals; check the rendered document as the skill requires. These\n\
             artifacts are outside your working folder: never commit them. The document does not\n\
             replace the report tool's answer: still put your whole answer in answer.\n",
        );
    }
    match f.kind {
        TaskKind::Review => out.push_str(
            "Call report with review: the verdict (pass, findings or blocked); each finding with its\n\
             severity, file and line, one sentence and an action (auto_fix, ask_user or no_op); and\n\
             the risk (low, medium or high) with its reason. Put the whole review in answer.\n\
             Under normal autonomy the user reads your review before your supervisor acts on it:\n\
             before reporting done, read $QM_REPORT_DIR/SKILL.md, copy $QM_REPORT_DIR/template.html\n\
             to $QM_REPORT_DIR/report.html and customize it into your complete static report of what\n\
             you found. Never commit it. Under auto autonomy it isn't needed.\n",
        ),
        TaskKind::Reproduce => out.push_str(
            "Call report with your whole answer in answer, opening with reproduced: yes or\n\
             reproduced: no, and set reproduced to match.\n",
        ),
        TaskKind::Plan => out.push_str(
            "Call report with a short summary of the round in answer, never the documents. The\n\
             implementation handoff is plan.md; under normal autonomy the user reviews your authored\n\
             plan.html. Check before reporting, wait for replies, and update both documents each\n\
             round. Under auto autonomy, report the completed plan.md for the build.\n",
        ),
        kind if kind.usually_answers() => {
            out.push_str("Call report with your whole answer in answer.");
            if kind == TaskKind::Research {
                out.push_str("\nIf it is a confirmed diagnosis a fix can build on, set diagnosis.");
            }
            out.push('\n');
        }
        _ => {
            out.push_str("Call report with: Intent · What changed · Verified · Risk · Not done.");
            if running_level(f) == Some(VerificationLevel::Quick) {
                out.push_str(" (Optional at quick.)");
            }
            if let BriefVerification::Level {
                ships_as_standard: true,
                ..
            } = f.verification
            {
                out.push_str("\nSay under Risk that Careful shipped as standard.");
            }
            out.push('\n');
        }
    }
}

/// Pushes text as it is, line by line, ending with a newline.
fn push_block(out: &mut String, text: &str) {
    for line in text.lines() {
        out.push_str(line.trim_end());
        out.push('\n');
    }
}

fn push_bullet(out: &mut String, text: &str) {
    let mut lines = text.lines();
    if let Some(first) = lines.next() {
        let _ = writeln!(out, "- {}", first.trim_end());
    }
    for line in lines {
        let _ = writeln!(out, "  {}", line.trim_end());
    }
}

/// Wraps one line of text at `width` characters, breaking at spaces; a word longer than the
/// width gets a line of its own.
fn wrap(line: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in line.split(' ').filter(|w| !w.is_empty()) {
        let needed = if current.is_empty() {
            word.chars().count()
        } else {
            current.chars().count() + 1 + word.chars().count()
        };
        if needed > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Which engine the crewmate prompt is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptEngine {
    /// Appended to Claude Code's `claude_code` preset; skills arrive as a plugin folder.
    Claude,
    /// The whole system prompt on the provider (and scripted) engine; skills are listed.
    Provider,
}

/// Where an instruction file came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstructionSource {
    /// `~/.agents/AGENTS.md`.
    Personal,
    /// The worktree's `AGENTS.md`, or `CLAUDE.md` when there is none.
    Project,
}

/// One instruction file, read by the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionFile {
    pub source: InstructionSource,
    /// How the prompt names it: `~/.agents/AGENTS.md`, `AGENTS.md`, `CLAUDE.md`.
    pub shown_as: String,
    pub text: String,
}

/// One skill the app gathered into `tasks/C-0042/skills/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    /// Its `SKILL.md`, as an absolute path the crewmate can read.
    pub path: String,
}

/// The crewmate's system prompt: on the provider engine `crewmate-provider` first, then
/// `crewmate`, the instruction files (personal first, then the project's), and on the provider
/// engine the skills list. The instruction files and skills are your files and the project's,
/// passed on as they are.
pub fn crewmate_prompt(
    engine: PromptEngine,
    files: &[InstructionFile],
    skills: &[SkillEntry],
    plan: Option<PlanMode>,
) -> String {
    let mut out = String::new();
    if engine == PromptEngine::Provider {
        out.push_str(
            index::get("crewmate-provider")
                .expect("the crewmate-provider prompt is registered")
                .trim_end(),
        );
        out.push_str("\n\n");
    }
    out.push_str(
        index::get("crewmate")
            .expect("the crewmate prompt is registered")
            .trim_end(),
    );
    out.push('\n');
    if let Some(mode) = plan {
        let name = match mode {
            PlanMode::Interactive => "crewmate-plan-interactive",
            PlanMode::Internal => "crewmate-plan-internal",
        };
        out.push('\n');
        out.push_str(
            index::get(name)
                .expect("the plan prompts are registered")
                .trim_end(),
        );
        out.push('\n');
    }

    let mut ordered: Vec<&InstructionFile> = files
        .iter()
        .filter(|f| f.source == InstructionSource::Personal)
        .collect();
    ordered.extend(
        files
            .iter()
            .filter(|f| f.source == InstructionSource::Project),
    );
    ordered.retain(|f| !f.text.trim().is_empty());
    if !ordered.is_empty() {
        out.push_str("\n# Instruction files\n");
        for file in ordered {
            let whose = match file.source {
                InstructionSource::Personal => "The user's own instructions",
                InstructionSource::Project => "This project's instructions",
            };
            let _ = writeln!(
                out,
                "\n## {whose} ({})\n\n{}",
                neutralise(&file.shown_as),
                neutralise(file.text.trim_end())
            );
        }
    }

    if engine == PromptEngine::Provider && !skills.is_empty() {
        out.push_str(
            "\n# Skills\n\nRead a skill's SKILL.md when its description fits what you're doing.\n\n",
        );
        for skill in skills {
            let _ = writeln!(
                out,
                "- {}: {} ({})",
                neutralise(&skill.name),
                neutralise(skill.description.trim()),
                neutralise(&skill.path)
            );
        }
    }
    out
}

#[cfg(test)]
mod report_tests {
    use super::*;

    fn facts(kind: TaskKind) -> BriefFacts {
        BriefFacts {
            task: TaskId::new(42).unwrap(),
            label: TaskLabelId::new(42).unwrap(),
            kind,
            supervisor: BriefSupervisor::Quartermaster,
            project: "test".to_owned(),
            delivery: DeliveryMode::Folder,
            has_remote: false,
            verification: BriefVerification::None,
            workspace: BriefWorkspace::Folder {
                path: "/project".to_owned(),
                repository: false,
            },
            pr_target: None,
            ports: PortBlock { base: 41_000 },
            goal: "answer".to_owned(),
            user_words: String::new(),
            user_words_trimmed: false,
            instructions: vec![],
            constraints: vec![],
            tests_required: false,
            builds_on: None,
            repair_attempts: 0,
        }
    }

    #[test]
    fn a_review_writes_its_static_report_under_normal_autonomy_only() {
        let text = render_brief(&facts(TaskKind::Review));
        for required in [
            "Under normal autonomy the user reads your review before your supervisor acts on it",
            "$QM_REPORT_DIR/SKILL.md",
            "$QM_REPORT_DIR/template.html",
            "$QM_REPORT_DIR/report.html",
            "Put the whole review in answer.",
            "Under auto autonomy it isn't needed.",
        ] {
            assert!(text.contains(required), "missing {required}");
        }
    }

    #[test]
    fn exactly_four_kinds_require_static_report_artifacts() {
        for kind in [
            TaskKind::Lookup,
            TaskKind::Research,
            TaskKind::Reproduce,
            TaskKind::Teach,
        ] {
            let text = render_brief(&facts(kind));
            for required in [
                "Before reporting done",
                "$QM_REPORT_DIR/SKILL.md",
                "$QM_REPORT_DIR/template.html",
                "$QM_REPORT_DIR/report.html",
                "$QM_REPORT_DIR/assets/",
                "human clarity",
                "technical",
                "visuals",
                "whole answer in answer",
            ] {
                assert!(text.contains(required), "{kind}: missing {required}");
            }
        }
        for kind in [
            TaskKind::Build,
            TaskKind::Fix,
            TaskKind::Plan,
            TaskKind::Unclear,
        ] {
            let text = render_brief(&facts(kind));
            assert!(!text.contains("$QM_REPORT_DIR"), "{kind}");
        }
    }

    #[test]
    fn reproduction_and_research_keep_their_tool_contracts() {
        let reproduce = render_brief(&facts(TaskKind::Reproduce));
        assert!(reproduce.contains("opening with reproduced: yes or"));
        assert!(reproduce.contains("reproduced: no, and set reproduced to match"));
        let research = render_brief(&facts(TaskKind::Research));
        assert!(research.contains("set diagnosis"));
    }

    #[test]
    fn both_system_prompts_explain_artifacts_without_replacing_the_answer() {
        for engine in [PromptEngine::Claude, PromptEngine::Provider] {
            let prompt = crewmate_prompt(engine, &[], &[], None);
            assert!(prompt.contains("For a done lookup, research, reproduce or teach task"));
            assert!(prompt.contains("a done review under normal autonomy"));
            assert!(prompt.contains("$QM_REPORT_DIR/SKILL.md"));
            assert!(prompt.contains("$QM_REPORT_DIR/template.html"));
            assert!(prompt.contains("$QM_REPORT_DIR/report.html"));
            assert!(prompt.contains("whole answer"));
            assert!(prompt.contains("matching `reproduced` metadata"));
        }
    }
}
