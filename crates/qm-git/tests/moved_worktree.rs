//! What a crewmate's move reads and removes in its old repository: whether its worktree is clean,
//! and its branch, deleted only while it has no commits of its own.
//!
//! Each test builds a real repository in its own folder under the system temp directory, and
//! removes it at the end.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use qm_git::{Git, GitError, Repo};
use qm_types::ids::CommitSha;

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
        Self(path.canonicalize().expect("canonical"))
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

/// A checkout with one commit on `main`, and a worktree on `crew` at that commit.
fn checkout_with_worktree(scratch: &Scratch) -> (PathBuf, PathBuf, CommitSha) {
    let checkout = scratch.0.join("checkout");
    fs::create_dir_all(&checkout).expect("checkout folder");
    git(&checkout, &["init", "-q", "-b", "main"]);
    fs::write(checkout.join("a.txt"), "one\n").expect("write");
    git(&checkout, &["add", "a.txt"]);
    git(&checkout, &["commit", "-q", "-m", "one"]);
    let base: CommitSha = git(&checkout, &["rev-parse", "HEAD"]).parse().expect("sha");
    let worktree = scratch.0.join("worktree");
    git(
        &checkout,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "crew",
            worktree.to_str().unwrap(),
        ],
    );
    (checkout, worktree, base)
}

#[test]
fn a_worktree_is_clean_only_with_nothing_uncommitted() {
    let scratch = Scratch::new("clean");
    let (checkout, worktree, _) = checkout_with_worktree(&scratch);
    let repo = Repo::open(Git::new(), &checkout).expect("open");
    assert!(repo.worktree_is_clean(&worktree).expect("status"));

    fs::write(worktree.join("new.txt"), "new\n").expect("write");
    assert!(
        !repo.worktree_is_clean(&worktree).expect("status"),
        "untracked"
    );
    fs::remove_file(worktree.join("new.txt")).expect("remove");
    fs::write(worktree.join("a.txt"), "changed\n").expect("write");
    assert!(
        !repo.worktree_is_clean(&worktree).expect("status"),
        "modified"
    );
    git(&worktree, &["commit", "-q", "-am", "work"]);
    assert!(
        repo.worktree_is_clean(&worktree).expect("status"),
        "committed"
    );

    let missing = scratch.0.join("gone");
    assert!(matches!(
        repo.worktree_is_clean(&missing),
        Err(GitError::NotAWorktree { .. })
    ));
    assert!(matches!(
        repo.worktree_is_clean(&worktree.join("sub")),
        Err(GitError::NotAWorktree { .. })
    ));
}

#[test]
fn a_branch_is_deleted_only_while_it_has_no_commits_of_its_own() {
    let scratch = Scratch::new("branch");
    let (checkout, worktree, base) = checkout_with_worktree(&scratch);
    let repo = Repo::open(Git::new(), &checkout).expect("open");

    // Checked out in a worktree: left alone.
    assert!(!repo.delete_branch_if_at("crew", &base).expect("delete"));
    git(
        &checkout,
        &["worktree", "remove", worktree.to_str().unwrap()],
    );

    // A commit of its own: kept.
    git(&checkout, &["branch", "other", "main"]);
    git(&checkout, &["switch", "-q", "other"]);
    git(&checkout, &["commit", "-q", "--allow-empty", "-m", "own"]);
    git(&checkout, &["switch", "-q", "main"]);
    assert!(!repo.delete_branch_if_at("other", &base).expect("delete"));
    assert!(git(&checkout, &["branch", "--list", "other"]).contains("other"));

    // At its base: deleted. Gone already, or unnamed: nothing to do.
    assert!(repo.delete_branch_if_at("crew", &base).expect("delete"));
    assert_eq!(git(&checkout, &["branch", "--list", "crew"]), "");
    assert!(!repo.delete_branch_if_at("crew", &base).expect("delete"));
    assert!(!repo.delete_branch_if_at("", &base).expect("delete"));
}
