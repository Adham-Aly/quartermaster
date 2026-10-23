//! Which task numbers a repository's branches name: local task branches and remote-tracking copies.
//!
//! Each test builds a real repository in its own folder under the system temp directory, and
//! removes it at the end.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use qm_git::{Git, Repo};
use qm_types::ids::TaskId;

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

#[test]
fn local_and_remote_tracking_legacy_task_branches_name_their_numbers() {
    let scratch = Scratch::new("numbers");
    let checkout = scratch.0.join("checkout");
    fs::create_dir_all(&checkout).expect("checkout folder");
    git(&checkout, &["init", "-q", "-b", "main"]);
    fs::write(checkout.join("a.txt"), "one\n").expect("write");
    git(&checkout, &["add", "a.txt"]);
    git(&checkout, &["commit", "-q", "-m", "one"]);
    let head = git(&checkout, &["rev-parse", "HEAD"]);

    git(&checkout, &["branch", "qm/task-0042", &head]);
    git(
        &checkout,
        &["update-ref", "refs/remotes/origin/qm/task-0007", &head],
    );
    git(
        &checkout,
        &["update-ref", "refs/remotes/origin/main", &head],
    );
    git(&checkout, &["branch", "feature/other", &head]);
    // A branch named after its work names no number.
    git(
        &checkout,
        &["branch", "fix/upload-retry-on-slow-tmp", &head],
    );

    let numbers = Repo::open(Git::new(), &checkout)
        .expect("repo")
        .legacy_task_numbers()
        .expect("numbers");
    assert_eq!(
        numbers,
        vec![TaskId::new(7).expect("7"), TaskId::new(42).expect("42")]
    );
}

#[test]
fn a_name_is_taken_by_a_local_or_an_origin_branch() {
    let scratch = Scratch::new("taken");
    let checkout = scratch.0.join("checkout");
    fs::create_dir_all(&checkout).expect("checkout folder");
    git(&checkout, &["init", "-q", "-b", "main"]);
    fs::write(checkout.join("a.txt"), "one\n").expect("write");
    git(&checkout, &["add", "a.txt"]);
    git(&checkout, &["commit", "-q", "-m", "one"]);
    let head = git(&checkout, &["rev-parse", "HEAD"]);
    git(&checkout, &["branch", "fix/x", &head]);
    git(
        &checkout,
        &["update-ref", "refs/remotes/origin/fix/x-2", &head],
    );
    // Another remote's branch doesn't count: the app pushes to origin only.
    git(
        &checkout,
        &["update-ref", "refs/remotes/fork/fix/x-3", &head],
    );

    let names: Vec<String> = ["fix/x", "fix/x-2", "fix/x-3", "fix/x-4"]
        .iter()
        .map(|n| (*n).to_owned())
        .collect();
    let taken = Repo::open(Git::new(), &checkout)
        .expect("repo")
        .taken_branch_names(&names)
        .expect("taken");
    assert_eq!(
        taken.into_iter().collect::<Vec<_>>(),
        vec!["fix/x".to_owned(), "fix/x-2".to_owned()]
    );
}
