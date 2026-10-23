//! Every push the app makes goes to the project's own repository and nowhere else, and counts
//! only once GitHub has it.
//!
//! A task's worktree shares the repository's `config` with your checkout (git keeps one per
//! repository, and per-worktree config is off), so a single config write from any worktree, a
//! `git remote set-url --push origin …` or a `url.*.insteadOf`, changes where your checkout
//! pushes, and where the app's authenticated pushes would go. This keeps the app's pushes
//! from being redirected. [`push`] is the only way the app pushes, and it:
//!
//! 1. **Checks the destination.** It reads exactly where git would push the remote
//!    ([`Repo::push_urls`]: every push URL after `pushurl`, `insteadOf` and `pushInsteadOf`)
//!    and refuses unless that is one URL, and that URL names the project's GitHub repository
//!    (compared as a repository: scheme and host case, a trailing `.git`, ssh and https forms of
//!    the same `owner/name` are equal; [`RepoRef::from_remote`]). A project with no GitHub
//!    repository on record must push to exactly one URL that isn't on GitHub, so your GitHub
//!    login is never sent to a repository the project doesn't name.
//! 2. **Pushes only there.** It hands that URL to [`Repo::push`], which reads the URLs again
//!    under the repository's write lock and pushes only if they are still exactly that one. The
//!    push travels the app's own way, not the repository config's: its own ssh command, no
//!    askpass program, only that URL's protocol, TLS verified; settings git gives no override
//!    for (a proxy or certificate authority in the repository's config) stop the push instead.
//! 3. **Confirms on GitHub.** It reads `refs/heads/<branch>` through GitHub's API and counts the
//!    push only when GitHub has the pushed commit there. A lease rejected because the branch is
//!    already at that commit (an earlier push the app didn't get to record) counts the same way,
//!    confirmed on GitHub too. Without a GitHub repository, git's own answer is all there is.
//!
//! What remains is a config write landing in the milliseconds between step 2's read and git's
//! own; step 3 then keeps a push that went elsewhere from ever being recorded as pushed.

use super::DeliveryService;
use crate::tasks::ProjectRow;
use qm_git::{GitError, PushCredential, PushOutcome, PushSpec, Repo, RepoTransportSetting};
use qm_github::{GithubClient, RepoRef, redact_url};
use qm_types::ids::CommitSha;
use std::path::Path;
use std::time::Duration;

/// How long to wait before each read of the pushed branch on GitHub: GitHub usually shows a
/// push at once, and these allow for a moment's lag before saying it didn't arrive.
const CONFIRM_DELAYS: [Duration; 4] = [
    Duration::ZERO,
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// One push the app makes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AppPush<'a> {
    pub project: &'a ProjectRow,
    /// The remote, `origin` (the held payload's).
    pub remote: &'a str,
    /// The task branch, `fix/upload-retry-on-slow-tmp`.
    pub branch: &'a str,
    pub head: &'a CommitSha,
    /// The app's own last push there; `None` means the branch must not exist yet.
    pub expected_remote: Option<&'a CommitSha>,
    /// What to do once the cause is fixed, ending a refusal: "settle again".
    pub retry: &'a str,
}

/// What an app push came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Pushed {
    /// The remote has exactly the pushed commit on the branch (on GitHub: GitHub says so): it
    /// may be recorded.
    Confirmed,
    /// Nothing may be recorded, with a sentence for whoever asked: the destination isn't the
    /// project's repository, the branch moved outside the app, or GitHub doesn't show the push.
    Refused(String),
    /// The remote refused the push, with git's words.
    Rejected(String),
    /// Git couldn't run (or the app stopped it): nothing to record.
    Failed(String),
}

/// Pushes `p.head` to `p.branch` on `p.remote` in the app's environment with its own credentials,
/// leased on the app's last push, only to the project's repository, and confirms it (the module's
/// steps 1 to 3).
pub(crate) async fn push(svc: &DeliveryService, p: &AppPush<'_>) -> Pushed {
    let repo = p
        .project
        .github_remote
        .as_deref()
        .and_then(RepoRef::from_project);
    let git = svc.git();
    let gh = (svc.deps().gh)();
    let checkout = p.project.path.clone();
    let remote = p.remote.to_owned();
    let branch = p.branch.to_owned();
    let head = p.head.clone();
    let expected = p.expected_remote.cloned();
    let retry = p.retry.to_owned();
    let target = repo.clone();
    let pushed = tokio::task::spawn_blocking(move || {
        let local = Repo::open(git, &checkout)?;
        let urls = local.push_urls(&remote)?;
        let url = match destination(&urls, &remote, &checkout, target.as_ref(), &retry) {
            Ok(url) => url.to_owned(),
            Err(refusal) => return Ok(Err(refusal)),
        };
        local
            .push(&PushSpec {
                remote: &remote,
                url: &url,
                head: &head,
                branch: &branch,
                expected_remote: expected.as_ref(),
                credential: PushCredential::GhHelper { gh },
            })
            .map(Ok)
    })
    .await;
    let outcome = match pushed {
        Err(e) => return Pushed::Failed(format!("the push of {} stopped: {e}", p.branch)),
        Ok(Err(e)) => return Pushed::Failed(git_failed(p.branch, &e)),
        Ok(Ok(Err(refusal))) => return Pushed::Refused(refusal),
        Ok(Ok(Ok(outcome))) => outcome,
    };
    match (outcome, repo) {
        (PushOutcome::Rejected { message }, _) => Pushed::Rejected(message),
        (PushOutcome::Redirected { urls }, _) => Pushed::Refused(format!(
            "{}'s push URL in {} changed while the app was about to push (it's {} now), so nothing was pushed; check it with {}, then {}",
            p.remote,
            p.project.path.display(),
            shown(&urls),
            show_config(p.remote),
            p.retry
        )),
        (PushOutcome::RepoConfig { settings }, _) => Pushed::Refused(format!(
            "the git config of {} sets {}, which would change how the app's push travels and which the app can't override (task worktrees share that config), so nothing was pushed; remove {} with {}, then {}",
            p.project.path.display(),
            RepoTransportSetting::listed(&settings),
            if settings.len() == 1 { "it" } else { "them" },
            RepoTransportSetting::removal(&settings),
            p.retry
        )),
        (PushOutcome::Pushed, Some(repo)) => confirm_pushed(svc.github(), &repo, p).await,
        (PushOutcome::LeaseRejected { .. }, Some(repo)) => {
            match svc.github().branch_head(&repo, p.branch).await {
                Ok(Some(now)) if &now == p.head => Pushed::Confirmed,
                Ok(now) => Pushed::Refused(moved_outside(p.branch, now.as_ref(), "on GitHub ")),
                Err(e) => Pushed::Refused(format!(
                    "{} was already pushed or moved outside the app, and GitHub couldn't be read to tell which ({}); nothing was recorded, so {}",
                    p.branch,
                    e.plain(),
                    p.retry
                )),
            }
        }
        (PushOutcome::Pushed, None) => Pushed::Confirmed,
        (PushOutcome::LeaseRejected { remote_now }, None) => {
            if remote_now.as_ref() == Some(p.head) {
                Pushed::Confirmed
            } else {
                Pushed::Refused(moved_outside(p.branch, remote_now.as_ref(), ""))
            }
        }
    }
}

/// Step 3 after git said it pushed: GitHub must show `p.head` on the branch.
async fn confirm_pushed(github: &GithubClient, repo: &RepoRef, p: &AppPush<'_>) -> Pushed {
    let mut last = Ok(None);
    for delay in CONFIRM_DELAYS {
        tokio::time::sleep(delay).await;
        last = github.branch_head(repo, p.branch).await;
        if matches!(&last, Ok(Some(now)) if now == p.head) {
            return Pushed::Confirmed;
        }
    }
    let short = p.head.short();
    Pushed::Refused(match last {
        Ok(now) => format!(
            "git pushed {} at {short}, but GitHub's {repo} shows {} {}, so the push didn't reach GitHub and nothing was recorded; check where {} pushes with {}, then {}",
            p.branch,
            p.branch,
            now.as_ref()
                .map_or_else(|| "missing".to_owned(), |c| format!("at {}", c.short())),
            p.remote,
            show_config(p.remote),
            p.retry
        ),
        Err(e) => format!(
            "git pushed {} at {short}, but GitHub couldn't confirm it ({}), so nothing was recorded; {}",
            p.branch,
            e.plain(),
            p.retry
        ),
    })
}

/// Step 1: the one URL the app may push to, or the refusal saying what's wrong and how to fix
/// it.
pub(crate) fn destination<'u>(
    urls: &'u [String],
    remote: &str,
    checkout: &Path,
    repo: Option<&RepoRef>,
    retry: &str,
) -> Result<&'u str, String> {
    let checkout = checkout.display();
    let target = repo.map_or_else(|| "one repository".to_owned(), |r| format!("GitHub's {r}"));
    let fix_url = repo.map_or_else(|| "<url>".to_owned(), RepoRef::https_url);
    let [url] = urls else {
        if urls.is_empty() {
            return Err(format!(
                "{remote} has no push URL in {checkout}, so there's nowhere the app may push; point it at {target} with `git remote add {remote} {fix_url}`, then {retry}"
            ));
        }
        return Err(format!(
            "{remote} in {checkout} pushes to {} places ({}) and git would push to every one, but the app pushes only to {target}; remove the extra ones (find them with {}), then {retry}",
            urls.len(),
            shown(urls),
            show_config(remote),
        ));
    };
    let on_github = RepoRef::from_remote(url);
    let resolves = format!(
        "{remote}'s push URL in {checkout} resolves to {}",
        redact_url(url)
    );
    match (repo, on_github) {
        (Some(want), Some(got)) if *want == got => Ok(url),
        (None, None) => Ok(url),
        (Some(_), _) => Err(format!(
            "{resolves}, not {target}, so nothing was pushed (task worktrees share this checkout's git config, so a `git remote set-url --push` or a `url.*.insteadOf` written in any of them changes it); fix it with `git remote set-url --push {remote} {fix_url}` or remove the pushurl with `git config --unset-all remote.{remote}.pushurl` (see where it comes from with {}), then {retry}",
            show_config(remote),
        )),
        (None, Some(got)) => Err(format!(
            "{resolves}, which is GitHub's {got}, but this project has no GitHub repository on record, so the app won't push there with your GitHub login; point {remote} back at the project's own remote (see where it comes from with {}), then {retry}",
            show_config(remote),
        )),
    }
}

/// The command that shows every setting deciding where `remote` pushes, with the file it's in.
pub(crate) fn show_config(remote: &str) -> String {
    format!("`git config --show-origin --get-regexp '^(remote\\.{remote}\\.|url\\.)'`")
}

/// URLs as a sentence shows them, without any password or token in them.
pub(crate) fn shown(urls: &[String]) -> String {
    if urls.is_empty() {
        return "none".to_owned();
    }
    urls.iter()
        .map(|u| redact_url(u))
        .collect::<Vec<_>>()
        .join(", ")
}

fn moved_outside(branch: &str, now: Option<&CommitSha>, place: &str) -> String {
    format!(
        "The remote branch {branch} changed outside the app (it's {} {place}now); nothing was pushed",
        now.map_or_else(|| "gone".to_owned(), |c| format!("at {}", c.short()))
    )
}

fn git_failed(branch: &str, e: &GitError) -> String {
    format!("git couldn't push {branch}: {e}")
}
