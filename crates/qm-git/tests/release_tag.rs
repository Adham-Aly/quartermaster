//! Release tags: the object the app writes, and its create-only push to a local bare remote.
//!
//! Each test builds real repositories in its own folder under the system temp directory, and
//! removes them at the end. Nothing leaves the machine.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use qm_git::{Git, PushCredential, Repo, TagPushOutcome, TagPushSpec};
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

/// A checkout with two commits on `main`, pushed to a bare `origin`.
fn setup(name: &str) -> (Scratch, PathBuf, String, CommitSha, CommitSha) {
    let scratch = Scratch::new(name);
    let origin = scratch.0.join("origin.git");
    let checkout = scratch.0.join("checkout");
    fs::create_dir_all(&checkout).expect("checkout");
    git(&scratch.0, &["init", "--quiet", "--bare", "origin.git"]);
    git(&checkout, &["init", "--quiet", "-b", "main"]);
    git(&checkout, &["config", "user.name", "Test"]);
    git(&checkout, &["config", "user.email", "test@example.invalid"]);
    let url = origin.to_string_lossy().into_owned();
    git(&checkout, &["remote", "add", "origin", &url]);
    let mut shas = vec![];
    for n in 0..2 {
        fs::write(checkout.join("f.txt"), format!("{n}\n")).expect("write");
        git(&checkout, &["add", "f.txt"]);
        git(&checkout, &["commit", "--quiet", "-m", &format!("c{n}")]);
        shas.push(git(&checkout, &["rev-parse", "HEAD"]).parse().expect("sha"));
    }
    // Seed the remote by fetching into it: the test's own git never pushes.
    let source = checkout.to_string_lossy().into_owned();
    git(
        &origin,
        &["fetch", "--quiet", &source, "main:refs/heads/main"],
    );
    let second = shas.pop().expect("second");
    let first = shas.pop().expect("first");
    (scratch, checkout, url, first, second)
}

fn spec<'a>(url: &'a str, tag: &'a str, object: &'a str) -> TagPushSpec<'a> {
    TagPushSpec {
        remote: "origin",
        url,
        tag,
        object,
        credential: PushCredential::Default,
    }
}

fn origin_tag(scratch: &Scratch, tag: &str) -> String {
    git(
        &scratch.0.join("origin.git"),
        &["rev-parse", &format!("refs/tags/{tag}")],
    )
}

#[test]
fn an_annotated_tag_is_written_pushed_once_and_never_overwritten() {
    let (scratch, checkout, url, first, second) = setup("annotated");
    let repo = Repo::open(Git::new(), &checkout).expect("open");
    let object = repo
        .write_tag_object(&second, "v1.0.0", "Release v1.0.0")
        .expect("tag object");
    assert_eq!(
        git(&checkout, &["cat-file", "-t", &object]),
        "tag",
        "a tag object"
    );
    assert_eq!(
        git(&checkout, &["tag", "--list"]),
        "",
        "no local tag ref is made"
    );
    let pushed = repo.push_tag(&spec(&url, "v1.0.0", &object)).expect("push");
    assert_eq!(pushed, TagPushOutcome::Pushed);
    assert_eq!(origin_tag(&scratch, "v1.0.0"), object);
    assert_eq!(
        git(
            &scratch.0.join("origin.git"),
            &["rev-parse", "v1.0.0^{commit}"]
        ),
        second.as_str()
    );
    assert_eq!(
        repo.remote_tag("origin", "v1.0.0", &PushCredential::Default)
            .expect("read"),
        Some(object.clone())
    );

    // A second tag object for another commit, same name: refused, the remote unchanged.
    let other = repo
        .write_tag_object(&first, "v1.0.0", "Another")
        .expect("other object");
    let again = repo.push_tag(&spec(&url, "v1.0.0", &other)).expect("push");
    assert_eq!(
        again,
        TagPushOutcome::Exists {
            remote_now: Some(object.clone())
        }
    );
    assert_eq!(origin_tag(&scratch, "v1.0.0"), object, "never overwritten");

    // The very same object again is git's "up to date": nothing changes on the remote.
    let same = repo.push_tag(&spec(&url, "v1.0.0", &object)).expect("push");
    assert_eq!(same, TagPushOutcome::Pushed);
    assert_eq!(origin_tag(&scratch, "v1.0.0"), object);
}

#[test]
fn a_lightweight_tag_points_at_the_commit_and_an_existing_one_is_kept() {
    let (scratch, checkout, url, first, second) = setup("lightweight");
    git(
        &scratch.0.join("origin.git"),
        &["tag", "v2", first.as_str()],
    );
    let repo = Repo::open(Git::new(), &checkout).expect("open");
    let out = repo
        .push_tag(&spec(&url, "v2", second.as_str()))
        .expect("push");
    assert_eq!(
        out,
        TagPushOutcome::Exists {
            remote_now: Some(first.as_str().to_owned())
        }
    );
    assert_eq!(origin_tag(&scratch, "v2"), first.as_str());
    let fresh = repo
        .push_tag(&spec(&url, "v3", second.as_str()))
        .expect("push");
    assert_eq!(fresh, TagPushOutcome::Pushed);
    assert_eq!(origin_tag(&scratch, "v3"), second.as_str());
}

#[test]
fn a_push_to_another_url_than_the_checked_one_sends_nothing() {
    let (scratch, checkout, url, _, second) = setup("redirect");
    let repo = Repo::open(Git::new(), &checkout).expect("open");
    let out = repo
        .push_tag(&spec("/somewhere/else.git", "v4", second.as_str()))
        .expect("push");
    assert_eq!(out, TagPushOutcome::Redirected { urls: vec![url] });
    assert_eq!(
        git(&scratch.0.join("origin.git"), &["tag", "--list"]),
        "",
        "nothing pushed"
    );
}

#[test]
fn tag_names_and_commits_are_checked() {
    let (_scratch, checkout, _, _, _) = setup("names");
    let repo = Repo::open(Git::new(), &checkout).expect("open");
    assert!(repo.check_tag_name("v1.2.3").expect("check"));
    assert!(repo.check_tag_name("release/2026-10").expect("check"));
    assert!(!repo.check_tag_name("bad..name").expect("check"));
    assert!(!repo.check_tag_name("trailing.lock").expect("check"));
    assert!(!repo.check_tag_name("sp ace").expect("check"));
    let missing: CommitSha = "0123456789abcdef0123456789abcdef01234567"
        .parse()
        .expect("sha");
    assert!(!repo.has_commit(&missing).expect("has"));
    assert!(repo.write_tag_object(&missing, "v9", "m").is_err());
}

#[test]
fn remote_tags_lists_every_tag_name_once_and_sorted() {
    let (scratch, checkout, _url, first, second) = setup("remote-tags");
    let repo = Repo::open(Git::new(), &checkout).expect("open");
    assert_eq!(
        repo.remote_tags("origin").expect("list"),
        Vec::<String>::new()
    );
    let origin = scratch.0.join("origin.git");
    git(&origin, &["tag", "v2.0.0", second.as_str()]);
    git(
        &origin,
        &["tag", "-a", "-m", "one", "v1.0.0", first.as_str()],
    );
    assert_eq!(
        repo.remote_tags("origin").expect("list"),
        vec!["v1.0.0".to_owned(), "v2.0.0".to_owned()],
        "annotated tags appear once, without their peeled line"
    );
}
