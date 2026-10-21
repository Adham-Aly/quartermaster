//! The commit-name check counts only the task's own commits (`Repo::own_commits`), through a
//! real fetch from a local `origin`.

use qm_git::{Git, Repo};
use qm_policy::names::{Boundary, check_commits};
use qm_types::ids::CommitSha;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// A throwaway folder, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    /// A new, empty folder named after the test, the process and a process-wide counter, so
    /// tests running in parallel never share one.
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("qm-core-{name}-{}-{n}", std::process::id()));
        // A folder left by an earlier run whose pid was reused is stale; start clean.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("scratch folder");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs the test's own git (not the app's runner), with no hooks, signing or user config.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf-8")
        .trim()
        .to_owned()
}

fn commit(dir: &Path, file: &str, message: &str) -> CommitSha {
    std::fs::write(dir.join(file), message).expect("write");
    git(dir, &["add", file]);
    git(dir, &["commit", "--quiet", "-m", message]);
    sha(dir, "HEAD")
}

fn sha(dir: &Path, rev: &str) -> CommitSha {
    git(dir, &["rev-parse", rev]).parse().expect("a sha")
}

/// An `origin` whose `main` gained a GitHub-style merge commit after the task started, and a
/// checkout whose task branch, started at the old `main`, holds `own` and is rebased onto the new
/// one. `test` names the scratch folder. Returns the scratch folder, the checkout, and the task's
/// base.
fn rebased_after_a_merge(test: &str, own: &[&str]) -> (Scratch, PathBuf, CommitSha) {
    let scratch = Scratch::new(test);
    let origin = scratch.0.join("origin.git");
    let work = scratch.0.join("work");
    let other = scratch.0.join("other");
    git(&scratch.0, &["init", "--quiet", "--bare", "origin.git"]);
    git(&scratch.0, &["init", "--quiet", "work"]);
    let origin_url = origin.to_str().expect("utf-8 path");
    git(&work, &["remote", "add", "origin", origin_url]);
    let base = commit(&work, "a", "feat(core): start");
    git(&work, &["push", "--quiet", "origin", "main"]);

    // Another task's pull request merges with a merge commit, on origin only.
    git(&scratch.0, &["clone", "--quiet", origin_url, "other"]);
    git(&other, &["switch", "--quiet", "-c", "qm/task-0002"]);
    commit(&other, "b", "feat(tui): another task's work");
    git(&other, &["switch", "--quiet", "main"]);
    git(
        &other,
        &[
            "merge",
            "--quiet",
            "--no-ff",
            "-m",
            "Merge pull request #1 from someone/qm/task-0002",
            "qm/task-0002",
        ],
    );
    git(&other, &["push", "--quiet", "origin", "main"]);

    // The task: started at the old main, then rebased onto the new one.
    git(&work, &["switch", "--quiet", "-c", "qm/task-0003"]);
    for (i, message) in own.iter().enumerate() {
        commit(&work, &format!("own-{i}"), message);
    }
    git(&work, &["fetch", "--quiet", "origin"]);
    git(
        &work,
        &["rebase", "--quiet", "--rebase-merges", "origin/main"],
    );
    (scratch, work, base)
}

fn names(repo: &Repo, head: &CommitSha, branch: &str) -> Vec<String> {
    repo.own_commits(head, branch)
        .expect("own commits")
        .commits
        .into_iter()
        .map(|(_, m)| m.lines().next().unwrap_or_default().to_owned())
        .collect()
}

fn check(names: &[String]) -> Result<(), qm_types::error::Refusal> {
    check_commits(names.iter().map(String::as_str), &[], Boundary::PrOpen)
}

#[test]
fn a_rebase_after_a_merge_commit_counts_only_the_tasks_own_commits() {
    let (_scratch, work, base) = rebased_after_a_merge(
        "a-rebase-after-a-merge-commit-counts-only-the-tasks-own-commits",
        &["fix(core): one", "test(core): two"],
    );
    let repo = Repo::open(Git::new(), &work).expect("repo");
    let head = sha(&work, "HEAD");

    // Counted from the task's stored base, main's merge commit was refused.
    let from_base = git(
        &work,
        &["log", "--format=%s", &format!("{}..HEAD", base.as_str())],
    );
    assert!(from_base.contains("Merge pull request #1"));

    // Against origin's main, only the task's own commits count, and they pass.
    let own = repo.own_commits(&head, "main").expect("own commits");
    assert_eq!(own.against, "origin/main");
    assert_eq!(own.merge_base, sha(&work, "origin/main"));
    let own = names(&repo, &head, "main");
    assert_eq!(own, ["fix(core): one", "test(core): two"]);
    assert!(check(&own).is_ok());
}

#[test]
fn origin_is_fetched_before_the_merge_base_is_taken() {
    let (_scratch, work, _base) = rebased_after_a_merge(
        "origin-is-fetched-before-the-merge-base-is-taken",
        &["fix(core): one"],
    );
    // The checkout's origin/main is stale: back at the task's start.
    git(&work, &["update-ref", "refs/remotes/origin/main", "HEAD~2"]);
    let repo = Repo::open(Git::new(), &work).expect("repo");
    let head = sha(&work, "HEAD");
    assert_eq!(names(&repo, &head, "main"), ["fix(core): one"]);
    assert_eq!(sha(&work, "origin/main"), sha(&work, "HEAD~1"));
}

#[test]
fn a_bad_name_in_the_tasks_own_commits_is_still_refused() {
    let (_scratch, work, _base) = rebased_after_a_merge(
        "a-bad-name-in-the-tasks-own-commits-is-still-refused",
        &["fix(core): one", "Fixed stuff"],
    );
    let repo = Repo::open(Git::new(), &work).expect("repo");
    let own = names(&repo, &sha(&work, "HEAD"), "main");
    assert_eq!(own, ["fix(core): one", "Fixed stuff"]);
    let refusal = check(&own).expect_err("a bad name is refused");
    assert!(refusal.reason.contains("Fixed stuff"), "{}", refusal.reason);
}

#[test]
fn merge_commits_in_the_tasks_own_range_are_left_out() {
    let (_scratch, work, _base) = rebased_after_a_merge(
        "merge-commits-in-the-tasks-own-range-are-left-out",
        &["fix(core): one"],
    );
    git(&work, &["switch", "--quiet", "-c", "side"]);
    commit(&work, "side", "feat(core): side work");
    git(&work, &["switch", "--quiet", "qm/task-0003"]);
    git(
        &work,
        &[
            "merge",
            "--quiet",
            "--no-ff",
            "-m",
            "Merge branch 'side'",
            "side",
        ],
    );
    let repo = Repo::open(Git::new(), &work).expect("repo");
    let mut own = names(&repo, &sha(&work, "HEAD"), "main");
    own.sort();
    assert_eq!(own, ["feat(core): side work", "fix(core): one"]);
}

#[test]
fn without_the_remote_ref_the_local_default_branch_is_used() {
    let scratch = Scratch::new("without-the-remote-ref-the-local-default-branch-is-used");
    git(&scratch.0, &["init", "--quiet", "work"]);
    let work = scratch.0.join("work");
    commit(&work, "a", "feat(core): start");
    git(&work, &["switch", "--quiet", "-c", "qm/task-0001"]);
    commit(&work, "b", "fix(core): mine");
    let repo = Repo::open(Git::new(), &work).expect("repo");
    let head = sha(&work, "HEAD");
    let own = repo.own_commits(&head, "main").expect("own commits");
    assert_eq!(own.against, "main");
    assert_eq!(names(&repo, &head, "main"), ["fix(core): mine"]);
    // Neither origin/trunk nor trunk: refused, not guessed.
    let missing = repo
        .own_commits(&head, "trunk")
        .expect_err("no such branch");
    assert!(
        matches!(missing, qm_git::GitError::NoTargetBranch { .. }),
        "{missing}"
    );
}
