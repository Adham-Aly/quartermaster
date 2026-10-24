//! Parse-stable patches and matching stats, using only throwaway local repositories.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use qm_git::{DiffStat, Git, Repo};
use qm_types::ids::CommitSha;

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

fn git(dir: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"GIT_") {
            command.env_remove(key);
        }
    }
    let out = command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A task renames and edits one file, and edits another outside its subdirectory. The base
/// advances independently and the checkout is then dirtied: neither belongs in the approved diff.
fn setup(name: &str) -> (Scratch, Repo, CommitSha, CommitSha) {
    let scratch = Scratch::new(name);
    let checkout = &scratch.0;
    git(checkout, &["init", "-q", "-b", "main", "--template="]);
    for (key, value) in [
        ("core.attributesFile", "/dev/null"),
        ("core.autocrlf", "false"),
        ("diff.context", "3"),
        ("diff.renames", "true"),
    ] {
        git(checkout, &["config", "--local", key, value]);
    }
    fs::create_dir(checkout.join("nested")).expect("nested folder");
    fs::write(
        checkout.join("nested/old.txt"),
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n",
    )
    .expect("original file");
    fs::write(checkout.join("root.txt"), "old root\n").expect("root file");
    git(checkout, &["add", "."]);
    git(checkout, &["commit", "-q", "-m", "initial"]);
    git(checkout, &["switch", "-q", "-c", "task"]);
    fs::rename(
        checkout.join("nested/old.txt"),
        checkout.join("nested/new.txt"),
    )
    .expect("rename");
    fs::write(
        checkout.join("nested/new.txt"),
        "one\ntwo\nthree\nupdated\nfive\nsix\nseven\neight\n",
    )
    .expect("edited rename");
    fs::write(checkout.join("root.txt"), "new root\n").expect("edit root file");
    git(checkout, &["add", "-A"]);
    git(checkout, &["commit", "-q", "-m", "task changes"]);
    let head = git(checkout, &["rev-parse", "HEAD"]).parse().expect("head");

    git(checkout, &["switch", "-q", "main"]);
    fs::write(checkout.join("base-only.txt"), "base only\n").expect("base-only file");
    git(checkout, &["add", "."]);
    git(checkout, &["commit", "-q", "-m", "base advances"]);
    let base = git(checkout, &["rev-parse", "HEAD"]).parse().expect("base");
    fs::write(checkout.join("root.txt"), "not approved\n").expect("dirty checkout");
    let repo = Repo::open(Git::new(), &checkout.join("nested")).expect("repo");
    (scratch, repo, base, head)
}

fn assert_patch(patch: &str) {
    let files: Vec<_> = patch
        .lines()
        .filter(|line| line.starts_with("diff --git "))
        .collect();
    assert_eq!(
        files,
        [
            "diff --git a/nested/old.txt b/nested/new.txt",
            "diff --git a/root.txt b/root.txt",
        ]
    );
    assert!(patch.contains("rename from nested/old.txt\nrename to nested/new.txt\n"));
    assert!(patch.contains("--- a/nested/old.txt\n+++ b/nested/new.txt\n"));
    assert!(patch.contains("--- a/root.txt\n+++ b/root.txt\n"));
    assert!(patch.contains("\n three\n-four\n+updated\n five\n"));
    assert!(patch.contains("\n-old root\n+new root\n"));
    assert!(!patch.contains("base-only.txt"));
    assert!(!patch.contains("not approved"));
}

fn assert_stat(repo: &Repo, base: &CommitSha, head: &CommitSha) {
    assert_eq!(
        repo.diff_stat(base, head).expect("stats"),
        DiffStat {
            files: 2,
            additions: 2,
            deletions: 2,
        }
    );
}

#[test]
fn rename_stats_match_the_patch_and_keep_the_approved_range() {
    let (_scratch, repo, base, head) = setup("diff-rename");
    let (patch, truncated) = repo.diff_text(&base, &head, usize::MAX).expect("patch");
    assert!(!truncated);
    assert_patch(&patch);
    assert_stat(&repo, &base, &head);

    let (cut, truncated) = repo.diff_text(&base, &head, 7).expect("cut patch");
    assert!(truncated);
    assert_eq!(
        cut,
        patch.lines().take(7).collect::<Vec<_>>().join("\n") + "\n"
    );
    let (whole, truncated) = repo
        .diff_text(&base, &head, patch.lines().count())
        .expect("exact line limit");
    assert!(!truncated);
    assert_eq!(whole, patch);
}

#[test]
fn submodule_commits_remain_numbered_patch_lines_despite_log_config() {
    let (scratch, repo, base, head) = setup("diff-submodule");
    let path = &scratch.0;
    git(
        path,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{base},vendor/lib"),
        ],
    );
    git(path, &["commit", "-q", "-m", "add gitlink"]);
    let before: CommitSha = git(path, &["rev-parse", "HEAD"]).parse().unwrap();
    git(
        path,
        &[
            "update-index",
            "--cacheinfo",
            &format!("160000,{head},vendor/lib"),
        ],
    );
    git(path, &["commit", "-q", "-m", "update gitlink"]);
    let after: CommitSha = git(path, &["rev-parse", "HEAD"]).parse().unwrap();
    git(path, &["config", "--local", "diff.submodule", "log"]);
    let (patch, truncated) = repo.diff_text(&before, &after, usize::MAX).unwrap();
    assert!(!truncated);
    assert!(patch.contains("@@ -1 +1 @@"));
    assert!(patch.contains(&format!(
        "-Subproject commit {base}\n+Subproject commit {head}"
    )));
    assert_eq!(
        repo.diff_stat(&before, &after).unwrap(),
        DiffStat {
            files: 1,
            additions: 1,
            deletions: 1,
        }
    );
}

#[test]
fn patch_format_and_rename_stats_ignore_repository_diff_settings() {
    let (scratch, repo, base, head) = setup("diff-config");
    for (key, value) in [
        ("diff.renames", "false"),
        ("diff.mnemonicPrefix", "true"),
        ("diff.relative", "true"),
        ("diff.outputIndicatorOld", "<"),
        ("diff.outputIndicatorNew", ">"),
        ("diff.outputIndicatorContext", "!"),
    ] {
        git(&scratch.0, &["config", "--local", key, value]);
    }
    for no_prefix in ["false", "true"] {
        git(
            &scratch.0,
            &["config", "--local", "diff.noprefix", no_prefix],
        );
        let (patch, truncated) = repo.diff_text(&base, &head, usize::MAX).expect("patch");
        assert!(!truncated);
        assert_patch(&patch);
        assert_stat(&repo, &base, &head);
    }
}
