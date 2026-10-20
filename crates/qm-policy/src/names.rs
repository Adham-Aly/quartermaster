//! Commit, pull-request and other name checks.
//!
//! "`type(scope): summary`, lowercase imperative, at most 72 characters. No AI co-author trailer:
//! Claude's attribution is switched off, provider-engine crewmates are told, and the check rejects
//! a `Co-Authored-By` line naming a model. Checked on `pr_open`, on a 'done' report, and at
//! settling."
//!
//! Commit names are checked only on the task's own commits (`git log --no-merges
//! <merge-base>..HEAD` against the freshly fetched `origin/<default>`), so commits a rebase picked
//! up from main never count. A pull request's title is checked again right before the app
//! squash-merges it, since it becomes the commit's title, and the commit names again right before
//! landing in place.
//!
//! Readings of the rule:
//! - The scope. The rule writes `type(scope): summary`, so commit and PR names require a scope
//!   (the stricter reading). Task titles may omit it ("plan: rate limiting").
//! - The type list is the Conventional Commits set.
//! - "Imperative" can't be checked by machine; the check holds the summary to a lowercase start
//!   and no trailing period.

use qm_types::error::Refusal;
use qm_types::ids::ModelId;

/// The types a name may start with.
pub const TYPES: &[&str] = &[
    "feat", "fix", "docs", "style", "refactor", "perf", "test", "build", "ci", "chore", "revert",
];

/// At most 72 characters.
pub const MAX_SUBJECT_CHARS: usize = 72;

/// Where a name is checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boundary {
    PrOpen,
    DoneReport,
    Settling,
    /// Right before the app merges a pull request.
    Merge,
    /// Right before the app merges a branch into your checkout (build in place).
    LandingInPlace,
}

impl Boundary {
    const fn describe(self) -> &'static str {
        match self {
            Self::PrOpen => "pr_open",
            Self::DoneReport => "a done report",
            Self::Settling => "settling",
            Self::Merge => "the merge",
            Self::LandingInPlace => "landing in place",
        }
    }
}

fn check_subject(subject: &str, scope_required: bool) -> Result<(), String> {
    let n = subject.chars().count();
    if n > MAX_SUBJECT_CHARS {
        return Err(format!(
            "is {n} characters; the limit is {MAX_SUBJECT_CHARS}"
        ));
    }
    let Some((head, summary)) = subject.split_once(": ") else {
        return Err("isn't in the form type(scope): summary".to_owned());
    };
    let (ty, scope) = match head.split_once('(') {
        Some((ty, rest)) => match rest.strip_suffix(')') {
            Some(scope) => (ty, Some(scope)),
            None => return Err("has an unclosed scope; use type(scope): summary".to_owned()),
        },
        None => (head.strip_suffix('!').unwrap_or(head), None),
    };
    let ty = ty.strip_suffix('!').unwrap_or(ty);
    if !TYPES.contains(&ty) {
        return Err(format!(
            "starts with {ty:?}, which isn't a type; use one of {}",
            TYPES.join(", ")
        ));
    }
    match scope {
        Some(s)
            if s.is_empty()
                || !s.chars().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || "-_/.".contains(c)
                }) =>
        {
            return Err(format!("has the scope {s:?}; a scope is lowercase words"));
        }
        None if scope_required => {
            return Err("has no scope; use type(scope): summary".to_owned());
        }
        _ => {}
    }
    let summary_trim = summary.trim();
    if summary_trim.is_empty() || summary_trim != summary {
        return Err("has an empty or padded summary".to_owned());
    }
    if summary.chars().next().is_some_and(char::is_uppercase) {
        return Err("starts its summary with a capital; use lowercase imperative".to_owned());
    }
    if summary.ends_with('.') {
        return Err("ends with a period".to_owned());
    }
    Ok(())
}

/// Words that name a model or an AI vendor in a `Co-Authored-By` trailer.
const MODEL_WORDS: &[&str] = &[
    "claude",
    "anthropic",
    "fable",
    "opus",
    "sonnet",
    "haiku",
    "gpt",
    "openai",
    "chatgpt",
    "codex",
    "copilot",
    "gemini",
    "grok",
    "xai",
    "kimi",
    "moonshot",
    "llm",
    "ai assistant",
];

/// Whether a trailer value names a model: a known word, or one of the pool's model IDs.
fn names_model(value: &str, models: &[ModelId]) -> bool {
    let v = value.to_lowercase();
    MODEL_WORDS.iter().any(|w| v.contains(w))
        || models
            .iter()
            .any(|m| v.contains(&m.as_str().to_lowercase()))
}

/// A commit message: its subject line, and no `Co-Authored-By` line naming a model. `models` are
/// the pool's model IDs, also refused in a trailer.
pub fn check_commit(message: &str, models: &[ModelId], at: Boundary) -> Result<(), Refusal> {
    let subject = message.lines().next().unwrap_or("");
    check_subject(subject, true).map_err(|why| {
        Refusal::new(format!(
            "the commit name {subject:?} {why} (checked at {})",
            at.describe()
        ))
    })?;
    for line in message.lines().skip(1) {
        if let Some((key, value)) = line.split_once(':')
            && key.trim().eq_ignore_ascii_case("co-authored-by")
            && names_model(value, models)
        {
            return Err(Refusal::new(format!(
                "commit {subject:?} has a Co-Authored-By line naming a model; remove it (checked at {})",
                at.describe()
            )));
        }
    }
    Ok(())
}

/// Every commit on the branch, as at settling or on a done report.
pub fn check_commits<'a>(
    messages: impl IntoIterator<Item = &'a str>,
    models: &[ModelId],
    at: Boundary,
) -> Result<(), Refusal> {
    messages
        .into_iter()
        .try_for_each(|m| check_commit(m, models, at))
}

/// A pull request's title.
pub fn check_pr_title(title: &str, at: Boundary) -> Result<(), Refusal> {
    check_subject(title, true).map_err(|why| {
        Refusal::new(format!(
            "the pull request title {title:?} {why} (checked at {})",
            at.describe()
        ))
    })
}

/// A task title, shown in the context block, the brief and the TUI: the same form, with the
/// scope optional.
pub fn check_task_title(title: &str) -> Result<(), Refusal> {
    check_subject(title, false)
        .map_err(|why| Refusal::new(format!("the task title {title:?} {why}")))
}

/// Git refuses a ref component ending in `.lock` (exactly, case-sensitive like git itself).
fn is_lock(component: &str) -> bool {
    component
        .rsplit_once('.')
        .is_some_and(|(_, suffix)| suffix == "lock")
}

/// A branch name a user typed (a base branch or a secondmate's standing branch), held to git's
/// own ref-name rules (`git check-ref-format --branch`).
pub fn check_branch_name(name: &str) -> Result<(), Refusal> {
    let bad = name.is_empty()
        || name.starts_with('-')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || is_lock(name)
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
        || name == "@"
        || name.split('/').any(|c| c.starts_with('.') || is_lock(c))
        || name
            .chars()
            .any(|c| c.is_control() || c == ' ' || "~^:?*[\\".contains(c));
    if bad {
        Err(Refusal::new(format!("{name:?} isn't a valid branch name")))
    } else {
        Ok(())
    }
}

/// A name for a secondmate or a project as shown in the TUI and the context block: one line, not
/// empty, at most 40 characters, with no marker prefix.
///
/// This is the minimal rule that keeps the context block's one-line rows intact.
pub fn check_display_name(name: &str) -> Result<(), Refusal> {
    let n = name.chars().count();
    if name.trim().is_empty() || name.trim() != name {
        return Err(Refusal::new("a name can't be empty or padded"));
    }
    if n > 40 {
        return Err(Refusal::new(format!(
            "the name {name:?} is {n} characters; the limit is 40"
        )));
    }
    if name.chars().any(char::is_control) || crate::markers::contains_marker(name) {
        return Err(Refusal::new(format!(
            "the name {name:?} has a line break or a reserved <qm: marker"
        )));
    }
    Ok(())
}
