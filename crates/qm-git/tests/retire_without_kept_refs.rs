//! Retiring a task writes no kept ref; startup deletes the ones earlier versions wrote.
//!
//! Each test builds a real repository in its own folder under the system temp directory, and
//! removes it at the end.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use qm_git::{BranchRemoval, Git, ReconcileProject, Repo, RetireSpec, WorktreeRemoval, reconcile};
use qm_types::data_dir::DataDir;
use qm_types::ids::{ProjectId, TaskId};

/// A scratch folder, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path =
            std::env::temp_dir().join(format!("qm-git-{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).expect("scratch folder");
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs `git` in `dir` with a fixed identity, and returns its trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A checkout with one commit on `main`, and a second commit on top of it. Returns the second.
fn checkout_with_two_commits(checkout: &Path) -> String {
    fs::create_dir_all(checkout).expect("checkout folder");
    git(checkout, &["init", "-q", "-b", "main"]);
    fs::write(checkout.join("a.txt"), "one\n").expect("write");
    git(checkout, &["add", "a.txt"]);
    git(checkout, &["commit", "-q", "-m", "one"]);
    fs::write(checkout.join("a.txt"), "two\n").expect("write");
    git(checkout, &["commit", "-q", "-am", "two"]);
    git(checkout, &["rev-parse", "HEAD"])
}

#[test]
fn startup_deletes_legacy_kept_refs_and_leaves_their_commits_alone() {
    let scratch = Scratch::new("legacy");
    let checkout = scratch.0.join("checkout");
    let tasks_dir = scratch.0.join("data").join("tasks");
    let second = checkout_with_two_commits(&checkout);
    git(
        &checkout,
        &["update-ref", "refs/quartermaster/kept/T-0042", &second],
    );
    git(
        &checkout,
        &["update-ref", "refs/quartermaster/other/keep", &second],
    );

    let project = ReconcileProject {
        project: ProjectId::new(1),
        checkout: checkout.clone(),
        tasks_with_worktrees: BTreeSet::new(),
    };
    let report = reconcile(&Git::new(), &tasks_dir, &[project]).expect("reconcile");

    assert_eq!(report.removed_kept_refs.len(), 1);
    assert_eq!(
        report.removed_kept_refs[0].git_ref,
        "refs/quartermaster/kept/T-0042"
    );
    assert_eq!(
        git(&checkout, &["for-each-ref", "refs/quartermaster/kept"]),
        ""
    );
    // Only the kept namespace is touched; the commit itself stays until git collects it.
    assert_eq!(
        git(&checkout, &["cat-file", "-t", &second]),
        "commit",
        "the commit must still exist"
    );
    assert!(git(&checkout, &["for-each-ref", "refs/quartermaster/other"]).contains("keep"));
    assert!(!report.needs_attention());
}

#[test]
fn retiring_removes_the_worktree_and_branch_and_writes_no_kept_ref() {
    let scratch = Scratch::new("retire");
    let checkout = scratch.0.join("checkout");
    let data = DataDir::new(scratch.0.join("data"));
    checkout_with_two_commits(&checkout);
    let task = TaskId::new(42).expect("a task number");
    let paths = data.task(task);

    git(
        &checkout,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            &task.branch(),
            paths.worktree.to_str().expect("utf-8 path"),
            "main",
        ],
    );
    // An unmerged commit, and an uncommitted change: both go with the task.
    fs::write(paths.worktree.join("a.txt"), "unmerged\n").expect("write");
    git(&paths.worktree, &["commit", "-q", "-am", "unmerged work"]);
    fs::write(paths.worktree.join("a.txt"), "uncommitted\n").expect("write");

    let retirement = Repo::open(Git::new(), &checkout)
        .expect("repo")
        .retire(&RetireSpec::new(task, &paths))
        .expect("retire");

    assert!(matches!(
        retirement.worktree,
        WorktreeRemoval::Removed { .. }
    ));
    assert!(matches!(retirement.branch, BranchRemoval::Deleted { .. }));
    assert!(!paths.worktree.exists(), "the worktree folder is gone");
    assert_eq!(
        git(&checkout, &["for-each-ref", "refs/heads/qm/task-0042"]),
        "",
        "the branch is gone"
    );
    assert_eq!(
        git(&checkout, &["for-each-ref", "refs/quartermaster/kept"]),
        "",
        "no kept ref is written"
    );
}
