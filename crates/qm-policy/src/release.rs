//! Release tags: a crewmate proposes one (`propose_release_tag`); only you approve it, on its own
//! card, under every autonomy; then the app creates and pushes exactly that tag at exactly that
//! commit, never over an existing one. Nothing here knows a project's tag or version scheme: any
//! name git accepts as a tag is allowed.

use qm_types::domain::{Autonomy, TagKind, TaskKind};
use qm_types::error::Refusal;
use qm_types::ids::{CommitSha, TaskId};

/// Longest tag name accepted, in bytes.
pub const MAX_TAG: usize = 128;
/// Longest annotated tag message, in characters.
pub const MAX_MESSAGE: usize = 2_000;
/// Longest summary the card shows, in characters.
pub const MAX_SUMMARY: usize = 600;

/// Whether a release tag waits for your approval: always, whatever the project's autonomy. A
/// published tag can't be taken back quietly, and Auto never stands in for it.
#[must_use]
pub const fn release_tag_held(_autonomy: Autonomy) -> bool {
    true
}

/// A proposal's arguments, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseTagArgs {
    pub tag: String,
    pub commit: CommitSha,
    pub branch: Option<String>,
    pub kind: TagKind,
    /// The annotated tag's message (the tag name when none was given); `None` for lightweight.
    pub message: Option<String>,
    pub summary: String,
}

/// Checks a proposal from the outside in: the task, then each argument. Git's own name check
/// (`check-ref-format`) runs later, in the repository.
pub fn check_release_tag(
    task_kind: TaskKind,
    tag: &str,
    commit: &str,
    branch: Option<&str>,
    kind: Option<TagKind>,
    message: Option<&str>,
    summary: &str,
) -> Result<ReleaseTagArgs, Refusal> {
    if task_kind.is_answer_only() {
        return Err(Refusal::new(
            "An answer-only task can't propose a release tag; say in your report what to tag and at which commit.",
        ));
    }
    check_tag_name(tag)?;
    let commit = check_commit(commit)?;
    let branch = branch.map(str::trim).filter(|b| !b.is_empty());
    if let Some(b) = branch {
        check_branch(b)?;
    }
    let kind = kind.unwrap_or(TagKind::Annotated);
    let message = match (kind, message.map(str::trim).filter(|m| !m.is_empty())) {
        (TagKind::Lightweight, Some(_)) => {
            return Err(Refusal::new(
                "A lightweight tag has no message: drop `message`, or ask for an annotated tag.",
            ));
        }
        (TagKind::Lightweight, None) => None,
        (_, Some(m)) => {
            if m.chars().count() > MAX_MESSAGE || m.contains('\0') {
                return Err(Refusal::new(format!(
                    "The tag message must be at most {MAX_MESSAGE} characters, with no NUL byte."
                )));
            }
            Some(m.to_owned())
        }
        (_, None) => Some(tag.to_owned()),
    };
    let summary = summary.trim();
    if summary.is_empty() || summary.chars().count() > MAX_SUMMARY {
        return Err(Refusal::new(format!(
            "Give a summary of 1 to {MAX_SUMMARY} characters: what this release is and why this commit."
        )));
    }
    Ok(ReleaseTagArgs {
        tag: tag.to_owned(),
        commit,
        branch: branch.map(str::to_owned),
        kind,
        message,
        summary: summary.to_owned(),
    })
}

/// A tag name the app is willing to ask git about: 1 to [`MAX_TAG`] bytes of printable ASCII
/// without spaces, not starting with `-`, and a bare name rather than a ref path.
pub fn check_tag_name(tag: &str) -> Result<(), Refusal> {
    let refuse = |why: &str| {
        Err(Refusal::new(format!(
            "`{}` can't be a tag name: {why}. Use the bare name, such as `v1.2.3`.",
            tag.escape_debug()
        )))
    };
    if tag.is_empty() || tag.len() > MAX_TAG {
        return refuse(&format!("it must be 1 to {MAX_TAG} characters"));
    }
    if !tag.bytes().all(|b| b.is_ascii_graphic()) {
        return refuse("only printable ASCII without spaces is allowed");
    }
    if tag.starts_with('-') {
        return refuse("it starts with `-`");
    }
    if tag.starts_with("refs/") {
        return refuse("it is a ref path");
    }
    if tag == "HEAD" || tag == "@" {
        return refuse("git reserves it");
    }
    Ok(())
}

/// A full 40-digit commit SHA, lowercased.
pub fn check_commit(commit: &str) -> Result<CommitSha, Refusal> {
    let commit = commit.trim();
    if commit.len() != 40 {
        return Err(Refusal::new(
            "Give the full 40-digit commit SHA to tag (`git rev-parse <rev>`), not a branch, tag or short SHA.",
        ));
    }
    commit
        .parse()
        .map_err(|_| Refusal::new("The commit must be 40 hex digits (`git rev-parse <rev>`)."))
}

fn check_branch(branch: &str) -> Result<(), Refusal> {
    if branch.starts_with(TaskId::BRANCH_PREFIX) {
        return Err(Refusal::new(
            "A release tag is checked against a merged branch, never a task branch: name the branch it landed on, or leave `branch` out.",
        ));
    }
    if branch.starts_with('-')
        || branch.starts_with("refs/")
        || !branch.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(Refusal::new(format!(
            "`{}` isn't a branch name: give the bare name, such as `main`.",
            branch.escape_debug()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789ABCDEF0123456789abcdef01234567";

    fn check(tag: &str, commit: &str) -> Result<ReleaseTagArgs, Refusal> {
        check_release_tag(TaskKind::Build, tag, commit, None, None, None, "Ship it.")
    }

    #[test]
    fn always_held_under_every_autonomy() {
        for autonomy in Autonomy::ALL {
            assert!(release_tag_held(*autonomy));
        }
    }

    #[test]
    fn any_tag_scheme_git_could_accept_passes() {
        for tag in ["v1.2.3", "1.0", "release/2026-10", "cli-v0.3.0-rc.1", "x"] {
            let args = check(tag, SHA).expect(tag);
            assert_eq!(args.tag, tag);
            assert_eq!(args.kind, TagKind::Annotated);
            assert_eq!(
                args.message.as_deref(),
                Some(tag),
                "message defaults to the tag"
            );
        }
        assert_eq!(
            check("v1", SHA).expect("v1").commit.as_str(),
            SHA.to_ascii_lowercase()
        );
    }

    #[test]
    fn bad_tags_and_commits_are_refused() {
        for tag in [
            "",
            "-v1",
            "refs/tags/v1",
            "v 1",
            "v1\n",
            "HEAD",
            "ü",
            &"a".repeat(129),
        ] {
            assert!(check(tag, SHA).is_err(), "{tag:?}");
        }
        for commit in ["main", "0123456", &"g".repeat(40), &"a".repeat(64)] {
            assert!(check("v1", commit).is_err(), "{commit:?}");
        }
    }

    #[test]
    fn answer_only_tasks_task_branches_and_messages_on_lightweight_tags_are_refused() {
        let refuse = |kind, branch, tag_kind, message| {
            check_release_tag(kind, "v1", SHA, branch, tag_kind, message, "s").is_err()
        };
        assert!(refuse(TaskKind::Research, None, None, None));
        assert!(refuse(TaskKind::Build, Some("qm/task-0042"), None, None));
        assert!(refuse(TaskKind::Build, Some("-main"), None, None));
        assert!(refuse(
            TaskKind::Build,
            None,
            Some(TagKind::Lightweight),
            Some("m")
        ));
        assert!(!refuse(
            TaskKind::Build,
            Some("main"),
            Some(TagKind::Lightweight),
            None
        ));
        assert!(
            check_release_tag(TaskKind::Build, "v1", SHA, None, None, None, " ").is_err(),
            "a summary is required"
        );
        let long = "m".repeat(MAX_MESSAGE + 1);
        assert!(refuse(TaskKind::Build, None, None, Some(long.as_str())));
    }
}
