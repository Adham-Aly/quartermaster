//! Commit, pull-request and other name checks, and the branch names tasks are given.
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

/// The most words in a task's name.
pub const MAX_TASK_NAME_WORDS: usize = 4;
/// The most characters in a task's name.
pub const MAX_TASK_NAME_CHARS: usize = 32;

/// A task's name, as `start_crewmate` gives it: two or three plain words for the crew list
/// ("Token refresh race"), on one line.
pub fn check_task_name(name: &str) -> Result<(), Refusal> {
    let words = name.split_whitespace().count();
    let chars = name.chars().count();
    if words == 0 {
        return Err(Refusal::new(
            "a task's name can't be empty; leave name out to have one made from the title",
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(Refusal::new("a task's name is one line of plain words"));
    }
    if words > MAX_TASK_NAME_WORDS || chars > MAX_TASK_NAME_CHARS {
        return Err(Refusal::new(format!(
            "the task name {name:?} is too long; use two or three words (at most \
             {MAX_TASK_NAME_WORDS} words and {MAX_TASK_NAME_CHARS} characters)"
        )));
    }
    Ok(())
}

/// A name for a task that wasn't given one, made from its title: the summary's first three words,
/// capitalised ("feat(ui): floating focus-timer pill in the corner" is "Floating focus-timer
/// pill"), cut to [`MAX_TASK_NAME_CHARS`] at a word.
pub fn task_name_from_title(title: &str) -> String {
    let summary = title.split_once(": ").map_or(title, |(_, s)| s);
    let mut name = String::new();
    for word in summary.split_whitespace().take(3) {
        let next = name.chars().count() + usize::from(!name.is_empty()) + word.chars().count();
        if next > MAX_TASK_NAME_CHARS && !name.is_empty() {
            break;
        }
        if !name.is_empty() {
            name.push(' ');
        }
        name.push_str(word);
    }
    let name: String = name.chars().take(MAX_TASK_NAME_CHARS).collect();
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "Untitled".to_owned(),
    }
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

/// The most characters a task branch has after its `type/`, suffix included.
pub const BRANCH_NAME_CHARS: usize = 48;

/// The highest suffix a task branch is given (`fix/x-99`) before the start is refused.
pub const MAX_BRANCH_SUFFIX: u32 = 99;

/// Lowercase ASCII letters and digits, every run of anything else one `-`, none at either end.
fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// `slug` cut to at most `cap` characters: at the last `-` at or before the cap, or at the cap
/// when one word is longer.
fn cut(slug: &str, cap: usize) -> &str {
    if slug.len() <= cap {
        return slug;
    }
    if slug.as_bytes()[cap] == b'-' {
        return &slug[..cap];
    }
    match slug[..cap].rfind('-') {
        Some(at) if at > 0 => &slug[..at],
        _ => &slug[..cap],
    }
}

/// A task title's type and the rest of its branch name, unsuffixed and uncut:
/// `fix(upload): retry on slow tmp` gives `("fix", "upload-retry-on-slow-tmp")`.
fn branch_parts(title: &str) -> (String, String) {
    let (head, summary) = title.split_once(':').unwrap_or(("", title));
    let (ty, scope) = match head.split_once('(') {
        Some((ty, rest)) => (ty, rest.trim_end_matches(')')),
        None => (head, ""),
    };
    let ty = Some(slug(ty))
        .filter(|t| !t.is_empty() && !t.contains('-'))
        .unwrap_or_else(|| "chore".to_owned());
    let rest = slug(&format!("{scope} {summary}"));
    (ty, rest)
}

/// The branch a task titled `title` is given when its name is free, `type/scope-summary`
/// (`fix(upload): retry on slow tmp` gives `fix/upload-retry-on-slow-tmp`), at most
/// [`BRANCH_NAME_CHARS`] after the `type/`. A title with nothing usable gives `type/change`.
pub fn branch_for_title(title: &str) -> String {
    branch_with_suffix(title, 1)
}

/// The names a task titled `title` may get, in order: [`branch_for_title`], then the same name
/// with `-2` … `-99`, each cut short enough that the suffix fits the cap. All distinct.
pub fn branch_candidates(title: &str) -> Vec<String> {
    (1..=MAX_BRANCH_SUFFIX)
        .map(|n| branch_with_suffix(title, n))
        .collect()
}

fn branch_with_suffix(title: &str, n: u32) -> String {
    let (ty, rest) = branch_parts(title);
    let suffix = if n > 1 {
        format!("-{n}")
    } else {
        String::new()
    };
    let rest = cut(&rest, BRANCH_NAME_CHARS - suffix.len());
    let rest = if rest.is_empty() { "change" } else { rest };
    let name = format!("{ty}/{rest}{suffix}");
    if check_branch_name(&name).is_ok() {
        name
    } else {
        format!("{ty}/change{suffix}")
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
