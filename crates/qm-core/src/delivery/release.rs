//! Release tags: a crewmate proposes one, you approve it on its own card, and the app creates and
//! pushes exactly that tag at exactly that commit, once, never over an existing one.
//!
//! - [`propose`] (`propose_release_tag`) checks the project's GitHub repository, the commit and
//!   the tag, then writes a `proposed` launch (`qm_store::releases`) and a held card of kind
//!   `release_tag`, always held whatever the project's autonomy (`qm_policy::release`). The card
//!   binds the project, GitHub's repository id and name, the tag and the commit; nothing on it can
//!   be changed (`HeldService::amend` refuses it).
//! - [`ReleaseTagPerformer`] carries out your Approve. It checks everything again (same project
//!   and repository, the commit still on the branch GitHub shows, the tag still absent), writes an
//!   annotated tag object (no local ref), then passes the launch's one gate
//!   (`releases::begin_push`) and pushes create-only, through the same destination check as every
//!   app push (`push_guard`), with the app's own credentials. A launch is never pushed twice: a
//!   retry is a new proposal.
//! - Publication is claimed only when GitHub shows the tag at the app's object. An outcome git
//!   couldn't settle (the network dropped, the app stopped mid-push) is `unconfirmed` and is read
//!   back from GitHub every [`RECHECK_EVERY`] for [`RECHECK_FOR`] after the push ([`reconcile`]),
//!   then the supervisor is woken (`release_observed`) with the verdict.
//! - A published tag's watch starts in the same transaction that records it published; its GitHub
//!   release and workflow runs are then observed separately (`super::observe`).

use super::DeliveryService;
use super::observe;
use super::push_guard::{destination, show_config, shown};
use crate::held::{HeldError, HeldOutcome, HeldPerformer, HeldService, LoadedHeld, YouLine};
use crate::inbox::Notify;
use crate::tasks::{failed, new_wake, refuse};
use qm_git::{Git, PushCredential, Repo, RepoTransportSetting, TagPushOutcome, TagPushSpec};
use qm_github::{GithubClient, RepoRef, TagRef};
use qm_policy::release::{check_not_task_branch, check_release_tag};
use qm_store::StoreError;
use qm_store::held::{HeldPayload, HeldRow, NewHeld, ReleaseTagPayload, load_held};
use qm_store::releases::{self, LaunchRow, NewLaunch, Outcome};
use qm_store::tasks as task_store;
use qm_store::wakes::insert_wake;
use qm_tools::defs::{Held, ProposeReleaseTagArgs};
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::BoxFuture;
use qm_types::api::HeldAnswer;
use qm_types::domain::WakeKind;
use qm_types::domain::{HeldKind, HeldStatus, ProjectOrigin, ReleaseLaunchStatus as S, TagKind};
use qm_types::ids::{CommitSha, HeldActionId, ReleaseLaunchId};
use rusqlite::{Connection, params};
use std::path::PathBuf;
use std::time::Duration;

/// The remote a release tag is pushed to: the project's own.
pub const REMOTE: &str = "origin";
/// `events.kind` of a release tag held for your approval.
pub const RELEASE_HELD_EVENT: &str = "task.release_tag_held";
/// `events.kind` of a release tag's outcome.
pub const RELEASE_EVENT: &str = "task.release_tag";
pub const YOU_APPROVED_RELEASE_TAG: &str = "you.approved_release_tag";
pub const YOU_REJECTED_RELEASE_TAG: &str = "you.rejected_release_tag";
/// How often an unconfirmed tag is read back from GitHub.
pub const RECHECK_EVERY: Duration = Duration::from_secs(30);
/// How long after the push an unconfirmed tag is read back before it counts as not published.
pub const RECHECK_FOR: Duration = Duration::from_secs(600);
/// The reads right after git says it pushed: GitHub usually shows a push at once.
const CONFIRM_DELAYS: [Duration; 4] = [
    Duration::ZERO,
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
/// What a published outcome says about what comes next.
const WATCHED: &str = "The app now watches its GitHub release and the workflow runs it started, \
     for up to 3 hours, and wakes you when that ends; release_status shows them meanwhile. The tag \
     alone never means a GitHub release can be downloaded.";

/// What the checks in the checkout came to.
enum Checked<T> {
    Ok(T),
    /// The world changed: the card no longer applies, with why.
    Changed(String),
}

/// What a checkout check needs, owned for the blocking task.
struct Look {
    git: Git,
    checkout: PathBuf,
    branch: String,
    /// The branch's head on GitHub, which the fetch must match.
    github_head: CommitSha,
    commit: CommitSha,
    tag: String,
}

/// Fetches the branch, and checks the fetched tip is GitHub's head, the commit is in it, and git
/// accepts the tag's name.
fn look(l: &Look) -> Result<Checked<Repo>, String> {
    let repo = Repo::open(l.git.clone(), &l.checkout).map_err(|e| e.to_string())?;
    if !repo.check_tag_name(&l.tag).map_err(|e| e.to_string())? {
        return Ok(Checked::Changed(format!(
            "git doesn't accept `{}` as a tag name",
            l.tag.escape_debug()
        )));
    }
    let tip = repo
        .fetch_remote_branch(REMOTE, &l.branch)
        .map_err(|e| format!("couldn't fetch {} from {REMOTE}: {e}", l.branch))?;
    if tip != l.github_head {
        return Ok(Checked::Changed(format!(
            "{REMOTE}'s {} fetched at {}, but GitHub shows it at {}; check where {REMOTE} fetches from",
            l.branch,
            tip.short(),
            l.github_head.short()
        )));
    }
    if !repo.has_commit(&l.commit).map_err(|e| e.to_string())?
        || !repo
            .contains_commit(&tip, &l.commit)
            .map_err(|e| e.to_string())?
    {
        return Ok(Checked::Changed(format!(
            "{} isn't on {} on GitHub (its head is {}); tag a commit that has landed there",
            l.commit.short(),
            l.branch,
            tip.short()
        )));
    }
    Ok(Checked::Ok(repo))
}

/// Whether GitHub's tag is exactly the object the app meant.
fn is_ours(found: &TagRef, kind: TagKind, expected: &str) -> bool {
    let want_kind = match kind {
        TagKind::Annotated => "tag",
        TagKind::Lightweight => "commit",
    };
    found.kind == want_kind && found.sha.eq_ignore_ascii_case(expected)
}

/// `propose_release_tag`.
#[allow(clippy::too_many_lines)]
pub(crate) async fn propose(
    svc: &DeliveryService,
    caller: &Caller,
    args: ProposeReleaseTagArgs,
) -> Result<Held, ToolFailure> {
    let task = super::callers_task(caller)?;
    let loaded = svc.load(task).await?;
    let checked = check_release_tag(
        &args.tag,
        &args.commit,
        args.branch.as_deref(),
        args.kind,
        args.message.as_deref(),
        &args.summary,
    )
    .map_err(ToolFailure::Refused)?;
    if let Some(branch) = checked.branch.clone() {
        let legacy = branch.starts_with(qm_types::ids::TaskId::LEGACY_BRANCH_PREFIX);
        let recorded = svc
            .tasks()
            .db()
            .call(move |conn| task_store::with_branch(conn, &branch))
            .await
            .map_err(|e| failed(&e))?
            .is_some();
        check_not_task_branch(legacy || recorded).map_err(ToolFailure::Refused)?;
    }
    let project = &loaded.project;
    if project.removed || project.origin == ProjectOrigin::Workspace {
        return Err(refuse(
            "release tags are only for a registered project with a GitHub repository",
        ));
    }
    let Some(repo) = loaded.repo() else {
        return Err(refuse(
            "your project has no GitHub repository on record, so the app has nowhere to push a release tag",
        ));
    };
    let github = svc.github();
    let info = github
        .repo(&repo)
        .await
        .map_err(|e| ToolFailure::Failed(e.plain()))?;
    if RepoRef::parse(&info.full_name).as_ref() != Some(&repo) {
        return Err(refuse(format!(
            "the project's repository {repo} is now {} on GitHub; ask the user to update the project before proposing a tag",
            info.full_name
        )));
    }
    if info.archived {
        return Err(refuse(format!(
            "{repo} is archived on GitHub; nothing can be tagged"
        )));
    }
    if !info.can_push {
        return Err(refuse(format!(
            "the user's GitHub login can't push to {repo}, so the app couldn't push the tag"
        )));
    }
    let branch = checked
        .branch
        .clone()
        .unwrap_or_else(|| project.default_branch.clone());
    let github_head = github
        .branch_head(&repo, &branch)
        .await
        .map_err(|e| ToolFailure::Failed(e.plain()))?
        .ok_or_else(|| refuse(format!("{repo} has no branch {branch} on GitHub")))?;
    let l = Look {
        git: svc.git(),
        checkout: project.path.clone(),
        branch: branch.clone(),
        github_head,
        commit: checked.commit.clone(),
        tag: checked.tag.clone(),
    };
    let looked = tokio::task::spawn_blocking(move || {
        look(&l).map(|c| match c {
            Checked::Ok(_) => Checked::Ok(()),
            Checked::Changed(why) => Checked::Changed(why),
        })
    })
    .await
    .map_err(|e| ToolFailure::Failed(format!("the check stopped: {e}")))?;
    match looked {
        Err(e) => return Err(ToolFailure::Failed(e)),
        Ok(Checked::Changed(why)) => return Err(refuse(why)),
        Ok(Checked::Ok(())) => {}
    }
    if let Some(found) = github
        .tag_ref(&repo, &checked.tag)
        .await
        .map_err(|e| ToolFailure::Failed(e.plain()))?
    {
        return Err(refuse(format!(
            "`{}` already exists on GitHub at {}; nothing to launch",
            checked.tag, found.sha
        )));
    }

    let at = svc.clock().now();
    let supervisor = loaded.row.supervisor;
    let asked = caller.agent;
    let project_id = project.id;
    let repo_name = info.full_name.clone();
    let repo_id = info.id;
    let tag_words = format!("{} at {}", checked.tag, checked.commit.short());
    let title = format!("{task}: push release tag {} to {repo_name}", checked.tag);
    let summary = format!("{task}'s release tag {tag_words} waits for the user's approval");
    let held = svc
        .tasks()
        .write(move |tx, _| {
            if let Some(live) = releases::live_for(tx, repo_id, &checked.tag)? {
                let same_task_waiting = live.status == S::Proposed && live.task == Some(task);
                if !same_task_waiting {
                    return Ok(Err(format!(
                        "`{}` is already being launched in {repo_name} ({}); wait for that launch to end",
                        checked.tag, live.status
                    )));
                }
                let why = "replaced by a newer proposal";
                releases::set_outcome(
                    tx,
                    live.id,
                    &[S::Proposed],
                    &Outcome {
                        status: S::Superseded,
                        remote_object: None,
                        words: why,
                        at,
                    },
                )?;
                if let Some(old) = live.held {
                    HeldService::supersede_with(tx, old, Some(task), why, at)?;
                }
            }
            let launch = releases::insert_launch(
                tx,
                &NewLaunch {
                    task,
                    project: project_id,
                    asked_by: Some(supervisor),
                    repo: repo_name.clone(),
                    repo_id,
                    tag: checked.tag.clone(),
                    commit: checked.commit.clone(),
                    branch: Some(branch.clone()),
                    kind: checked.kind,
                    message: checked.message.clone(),
                    at,
                },
            )?;
            let held = HeldService::insert(
                tx,
                &NewHeld {
                    payload: HeldPayload::ReleaseTag(ReleaseTagPayload {
                        launch,
                        task,
                        project: project_id,
                        repo: repo_name,
                        repo_id,
                        tag: checked.tag,
                        commit: checked.commit,
                        branch: Some(branch),
                        tag_kind: checked.kind,
                        message: checked.message,
                        summary: checked.summary,
                    }),
                    choices: Vec::new(),
                    title,
                    task: Some(task),
                    asked_by: Some(supervisor),
                    at,
                },
            )?;
            releases::set_held(tx, launch, held)?;
            task_store::event(
                tx,
                RELEASE_HELD_EVENT,
                task,
                Some(asked),
                &summary,
                serde_json::json!({ "held": held, "launch": launch }),
                at,
            )?;
            Ok(Ok(held))
        })
        .await
        .map_err(|e| failed(&e))?
        .map_err(refuse)?;
    Ok(Held {
        held_action: held,
        note: "Held for the user's approval. The answer and the launch status go to your supervisor; nothing is pushed until the user approves the card.".to_owned(),
    })
}

/// Carries out a release tag card (kind `release_tag`).
#[derive(Clone, Debug)]
pub struct ReleaseTagPerformer {
    delivery: DeliveryService,
}

impl ReleaseTagPerformer {
    pub fn new(delivery: DeliveryService) -> Self {
        Self { delivery }
    }
}

/// The `you` line of an approval.
fn approved(p: &ReleaseTagPayload) -> YouLine {
    YouLine {
        kind: YOU_APPROVED_RELEASE_TAG,
        summary: format!(
            "you approved pushing tag {} at {} to {}",
            p.tag,
            p.commit.short(),
            p.repo
        ),
    }
}

/// What a launch's status says, for the supervisor.
pub(crate) fn status_note(l: &LaunchRow) -> String {
    let words = l.remote_words.as_deref().unwrap_or("");
    let what = format!("Tag {} at {} in {}", l.tag, l.commit.short(), l.repo);
    match l.status {
        S::Published => format!("{what} is published: GitHub shows it. {WATCHED}"),
        S::Pushing | S::Unconfirmed => format!(
            "{what} was sent, but GitHub doesn't show it yet ({words}); the app keeps reading it back for {} minutes after the push and wakes you with the verdict. Don't propose it again meanwhile.",
            RECHECK_FOR.as_secs() / 60
        ),
        S::Collision => format!("{what} wasn't published: {words}. Nothing was overwritten."),
        S::Rejected => format!("{what} wasn't published: GitHub refused it ({words})."),
        S::NotPublished if l.pushed_at.is_some() => format!(
            "{what} isn't published: {words}. The push was sent, so check the remote before proposing it again."
        ),
        S::NotPublished => format!(
            "{what} wasn't published: {words}. Nothing was pushed; propose it again to retry."
        ),
        S::Superseded => {
            format!("{what} wasn't pushed: {words}. Propose again if it's still wanted.")
        }
        S::Declined => format!("{what} was declined; nothing was pushed."),
        S::Proposed => format!("{what} hasn't been pushed."),
    }
}

/// Records `outcome` on the launch (from one of `from`) with its task event, inside `tx`, and
/// starts the watch of a tag just published (armed by the caller once `tx` commits). True if it
/// moved.
fn record(
    tx: &Connection,
    launch: &LaunchRow,
    from: &[S],
    outcome: &Outcome<'_>,
) -> Result<bool, StoreError> {
    if !releases::set_outcome(tx, launch.id, from, outcome)? {
        return Ok(false);
    }
    if outcome.status == S::Published {
        observe::start(tx, launch.id)?;
    }
    if let Some(task) = launch.task {
        task_store::event(
            tx,
            RELEASE_EVENT,
            task,
            None,
            &format!(
                "release tag {} at {}: {} ({})",
                launch.tag,
                launch.commit.short(),
                outcome.status,
                outcome.words
            ),
            serde_json::json!({
                "launch": launch.id,
                "status": outcome.status,
                "remote_object": outcome.remote_object,
            }),
            outcome.at,
        )?;
    }
    Ok(true)
}

impl ReleaseTagPerformer {
    async fn write_outcome(
        &self,
        launch: &LaunchRow,
        from: &'static [S],
        status: S,
        remote_object: Option<String>,
        words: String,
    ) -> Result<LaunchRow, StoreError> {
        let at = self.delivery.clock().now();
        let launch = launch.clone();
        self.delivery
            .tasks()
            .db()
            .call(move |conn| {
                let tx = conn.transaction()?;
                record(
                    &tx,
                    &launch,
                    from,
                    &Outcome {
                        status,
                        remote_object: remote_object.as_deref(),
                        words: &words,
                        at,
                    },
                )?;
                let now = releases::load(&tx, launch.id)?.unwrap_or(launch);
                tx.commit()?;
                Ok(now)
            })
            .await
    }

    /// The card no longer applies: the launch is superseded with `why`, nothing was pushed.
    async fn superseded(&self, launch: &LaunchRow, why: String) -> Result<HeldOutcome, HeldError> {
        let now = self
            .write_outcome(launch, &[S::Proposed], S::Superseded, None, why)
            .await?;
        Ok(HeldOutcome {
            note: status_note(&now),
            you: None,
            superseded: true,
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn approve(
        &self,
        held: &LoadedHeld,
        p: &ReleaseTagPayload,
    ) -> Result<HeldOutcome, HeldError> {
        let svc = &self.delivery;
        let held_id = held.id;
        let launch = svc
            .read(move |conn| releases::for_held(conn, held_id))
            .await
            .map_err(|e| HeldError::Failed(e.render()))?
            .filter(|l| l.id == p.launch)
            .ok_or_else(|| HeldError::Failed("this card's launch is missing".to_owned()))?;
        if launch.status != S::Proposed {
            // Already carried out (or ended): never pushed again.
            return Ok(HeldOutcome {
                note: status_note(&launch),
                you: Some(approved(p)),
                superseded: launch.status == S::Superseded,
            });
        }
        // Everything bound on the card is checked again before anything is sent.
        let project_id = p.project;
        let project = svc
            .read(move |conn| qm_store::projects::load(conn, project_id))
            .await
            .map_err(|e| HeldError::Failed(e.render()))?;
        let Some(project) = project.filter(|pr| !pr.removed) else {
            return self
                .superseded(&launch, "the project was removed".to_owned())
                .await;
        };
        let Some(bound) = RepoRef::parse(&p.repo) else {
            return self
                .superseded(&launch, format!("{:?} isn't owner/name", p.repo))
                .await;
        };
        let repo = project
            .github_remote
            .as_deref()
            .and_then(RepoRef::from_project);
        if repo.as_ref() != Some(&bound) {
            return self
                .superseded(
                    &launch,
                    format!("the project no longer points at {}", p.repo),
                )
                .await;
        }
        let github = svc.github();
        let info = github
            .repo(&bound)
            .await
            .map_err(|e| HeldError::Failed(format!("{}; nothing was pushed", e.plain())))?;
        if info.id != p.repo_id || RepoRef::parse(&info.full_name).as_ref() != Some(&bound) {
            return self
                .superseded(
                    &launch,
                    format!(
                        "{} on GitHub is now another repository ({})",
                        p.repo, info.full_name
                    ),
                )
                .await;
        }
        if info.archived || !info.can_push {
            return self
                .superseded(&launch, format!("{} can't be pushed to now", p.repo))
                .await;
        }
        let branch = p
            .branch
            .clone()
            .unwrap_or_else(|| project.default_branch.clone());
        let Some(github_head) = github
            .branch_head(&bound, &branch)
            .await
            .map_err(|e| HeldError::Failed(format!("{}; nothing was pushed", e.plain())))?
        else {
            return self
                .superseded(&launch, format!("{branch} is gone from {}", p.repo))
                .await;
        };
        if let Some(found) = github
            .tag_ref(&bound, &p.tag)
            .await
            .map_err(|e| HeldError::Failed(format!("{}; nothing was pushed", e.plain())))?
        {
            return self
                .superseded(
                    &launch,
                    format!("`{}` already exists on GitHub at {}", p.tag, found.sha),
                )
                .await;
        }
        let l = Look {
            git: svc.git(),
            checkout: project.path.clone(),
            branch,
            github_head,
            commit: p.commit.clone(),
            tag: p.tag.clone(),
        };
        let kind = p.tag_kind;
        let message = p.message.clone().unwrap_or_else(|| p.tag.clone());
        let target = bound.clone();
        let checkout = project.path.clone();
        // In the checkout: the commit is on the branch, the tag object is written (no ref), and
        // the one URL the app may push to is found.
        let prepared = tokio::task::spawn_blocking(move || {
            let repo = match look(&l)? {
                Checked::Ok(repo) => repo,
                Checked::Changed(why) => return Ok(Checked::Changed(why)),
            };
            let object = match kind {
                TagKind::Annotated => repo
                    .write_tag_object(&l.commit, &l.tag, &message)
                    .map_err(|e| format!("couldn't write the tag object: {e}"))?,
                TagKind::Lightweight => l.commit.as_str().to_owned(),
            };
            let urls = repo.push_urls(REMOTE).map_err(|e| e.to_string())?;
            let url = destination(
                &urls,
                REMOTE,
                &checkout,
                Some(&target),
                "approve the card again",
            )?
            .to_owned();
            Ok::<_, String>(Checked::Ok((object, url)))
        })
        .await
        .map_err(|e| HeldError::Failed(format!("the check stopped: {e}")))?;
        let (object, url) = match prepared {
            Err(why) => return Err(HeldError::Failed(format!("{why}; nothing was pushed"))),
            Ok(Checked::Changed(why)) => return self.superseded(&launch, why).await,
            Ok(Checked::Ok(found)) => found,
        };
        // The one gate: only the caller that moves the launch to `pushing` sends anything.
        let at = svc.clock().now();
        let tag_object = (kind == TagKind::Annotated).then(|| object.clone());
        let launch_id = launch.id;
        let gate = svc
            .read(move |conn| {
                releases::begin_push(conn, launch_id, held_id, tag_object.as_deref(), at)
            })
            .await;
        match gate {
            Ok(true) => {}
            Ok(false) => {
                let now = svc
                    .read(move |conn| releases::load(conn, launch_id))
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(launch);
                return Ok(HeldOutcome {
                    note: status_note(&now),
                    you: Some(approved(p)),
                    superseded: now.status == S::Superseded,
                });
            }
            Err(e) => {
                return Err(HeldError::Failed(format!(
                    "{}; nothing was pushed",
                    e.render()
                )));
            }
        }
        // From here on the answer is always recorded: something may have left the Mac.
        let now = self
            .push_and_confirm(&launch, &project.path, &url, &object)
            .await;
        if matches!(now.status, S::Pushing | S::Unconfirmed) {
            reconcile(svc.clone(), now.id);
        } else if now.status == S::Published {
            observe::arm(svc, now.id, observe::READ_EVERY);
        }
        Ok(HeldOutcome {
            note: status_note(&now),
            you: Some(approved(p)),
            superseded: false,
        })
    }

    /// Pushes and reads back; always ends with the launch's status recorded.
    async fn push_and_confirm(
        &self,
        launch: &LaunchRow,
        checkout: &std::path::Path,
        url: &str,
        object: &str,
    ) -> LaunchRow {
        let git = self.delivery.git();
        let gh = (self.delivery.deps().gh)();
        let checkout = checkout.to_owned();
        let url_owned = url.to_owned();
        let tag = launch.tag.clone();
        let object_owned = object.to_owned();
        let pushed = tokio::task::spawn_blocking(move || {
            let repo = Repo::open(git, &checkout)?;
            repo.push_tag(&TagPushSpec {
                remote: REMOTE,
                url: &url_owned,
                tag: &tag,
                object: &object_owned,
                credential: PushCredential::GhHelper { gh },
            })
        })
        .await;
        let bound = RepoRef::parse(&launch.repo);
        let (status, remote, words): (S, Option<String>, String) = match pushed {
            Err(e) => (S::Unconfirmed, None, format!("the push stopped: {e}")),
            Ok(Err(e)) => (
                S::Unconfirmed,
                None,
                format!("git couldn't finish the push: {e}"),
            ),
            Ok(Ok(TagPushOutcome::Rejected { message })) => (S::Rejected, None, message),
            Ok(Ok(TagPushOutcome::Redirected { urls })) => (
                S::NotPublished,
                None,
                format!(
                    "{REMOTE}'s push URL changed while the app was about to push (it's {} now); check it with {}",
                    shown(&urls),
                    show_config(REMOTE)
                ),
            ),
            Ok(Ok(TagPushOutcome::RepoConfig { settings })) => (
                S::NotPublished,
                None,
                format!(
                    "the checkout's git config sets {}, which would change how the push travels; remove it with {}",
                    RepoTransportSetting::listed(&settings),
                    RepoTransportSetting::removal(&settings)
                ),
            ),
            // A lightweight tag names only the commit, so one already there may be someone else's:
            // never call it ours. (An annotated tag object carries the app's own tagger time.)
            Ok(Ok(TagPushOutcome::Exists { remote_now }))
                if launch.kind == TagKind::Lightweight =>
            {
                (
                    S::Collision,
                    remote_now.clone(),
                    format!(
                        "`{}` was already on GitHub (at {}) when the app pushed; it pushed nothing over it",
                        launch.tag,
                        remote_now.as_deref().unwrap_or("an unreadable object")
                    ),
                )
            }
            Ok(Ok(TagPushOutcome::Pushed | TagPushOutcome::Exists { .. })) => match &bound {
                Some(repo) => {
                    confirm(
                        self.delivery.github(),
                        repo,
                        &launch.tag,
                        launch.kind,
                        object,
                    )
                    .await
                }
                None => (
                    S::Unconfirmed,
                    None,
                    "the repository name is unreadable".to_owned(),
                ),
            },
        };
        match self
            .write_outcome(launch, &[S::Pushing], status, remote, words)
            .await
        {
            Ok(now) => now,
            Err(e) => {
                tracing::error!(launch = %launch.id, error = %e, "couldn't record a release tag's outcome");
                let mut now = launch.clone();
                now.status = S::Pushing;
                now
            }
        }
    }
}

/// Reads the tag back from GitHub after git pushed (or found it there): ours is published,
/// another object a collision, nothing (or no answer) unconfirmed.
async fn confirm(
    github: &GithubClient,
    repo: &RepoRef,
    tag: &str,
    kind: TagKind,
    expected: &str,
) -> (S, Option<String>, String) {
    let mut last = String::from("GitHub wasn't read");
    for delay in CONFIRM_DELAYS {
        tokio::time::sleep(delay).await;
        match github.tag_ref(repo, tag).await {
            Ok(Some(found)) if is_ours(&found, kind, expected) => {
                return (
                    S::Published,
                    Some(found.sha),
                    "GitHub shows the tag".to_owned(),
                );
            }
            Ok(Some(found)) => {
                return (
                    S::Collision,
                    Some(found.sha.clone()),
                    format!(
                        "`{tag}` on GitHub points at {}, not the app's tag",
                        found.sha
                    ),
                );
            }
            Ok(None) => "GitHub doesn't show the tag".clone_into(&mut last),
            Err(e) => last = format!("GitHub couldn't be read ({})", e.plain()),
        }
    }
    (S::Unconfirmed, None, last)
}

impl HeldPerformer for ReleaseTagPerformer {
    fn perform<'a>(
        &'a self,
        held: &'a LoadedHeld,
        answer: &'a HeldAnswer,
    ) -> BoxFuture<'a, Result<HeldOutcome, HeldError>> {
        Box::pin(async move {
            if !matches!(answer, HeldAnswer::Approve) {
                return Err(HeldError::Failed(
                    "only Approve pushes a release tag".to_owned(),
                ));
            }
            let HeldPayload::ReleaseTag(p) = &held.payload else {
                return Err(HeldError::Failed(
                    "this card isn't a release tag".to_owned(),
                ));
            };
            self.approve(held, p).await
        })
    }

    fn on_reject<'a>(
        &'a self,
        held: &'a LoadedHeld,
    ) -> BoxFuture<'a, Result<Option<HeldOutcome>, HeldError>> {
        Box::pin(async move {
            let HeldPayload::ReleaseTag(p) = &held.payload else {
                return Ok(None);
            };
            let launch_id = p.launch;
            let launch = self
                .delivery
                .read(move |conn| releases::load(conn, launch_id))
                .await
                .map_err(|e| HeldError::Failed(e.render()))?;
            if let Some(launch) = launch {
                self.write_outcome(
                    &launch,
                    &[S::Proposed],
                    S::Declined,
                    None,
                    "the user rejected it".to_owned(),
                )
                .await?;
            }
            Ok(Some(HeldOutcome {
                note: format!(
                    "The user rejected tag {} at {} in {}; nothing was pushed.",
                    p.tag,
                    p.commit.short(),
                    p.repo
                ),
                you: Some(YouLine {
                    kind: YOU_REJECTED_RELEASE_TAG,
                    summary: format!("you rejected pushing tag {} to {}", p.tag, p.repo),
                }),
                superseded: false,
            }))
        })
    }
}

/// Reads an unsettled launch back from GitHub every [`RECHECK_EVERY`] until [`RECHECK_FOR`] after
/// its push, then wakes its supervisor with the verdict. Runs in the background.
pub(crate) fn reconcile(svc: DeliveryService, launch: ReleaseLaunchId) {
    tokio::spawn(async move {
        loop {
            match reconcile_once(&svc, launch).await {
                Ok(true) => return,
                Ok(false) => tokio::time::sleep(RECHECK_EVERY).await,
                Err(e) => {
                    tracing::warn!(%launch, error = %e, "couldn't read back a release tag");
                    return;
                }
            }
        }
    });
}

/// One read-back. True when there's nothing more to read.
async fn reconcile_once(svc: &DeliveryService, id: ReleaseLaunchId) -> Result<bool, String> {
    let Some(launch) = svc
        .read(move |conn| releases::load(conn, id))
        .await
        .map_err(|e| e.render())?
    else {
        return Ok(true);
    };
    if !matches!(launch.status, S::Pushing | S::Unconfirmed) {
        return Ok(true);
    }
    let Some(repo) = RepoRef::parse(&launch.repo) else {
        return Ok(true);
    };
    let now = svc.clock().now();
    let pushed = launch.pushed_at.unwrap_or(launch.proposed_at);
    let grace = jiff::SignedDuration::try_from(RECHECK_FOR).unwrap_or_default();
    let over = now.duration_since(pushed) >= grace;
    let expected = match launch.kind {
        TagKind::Annotated => launch.tag_object.clone(),
        TagKind::Lightweight => Some(launch.commit.as_str().to_owned()),
    };
    let verdict = match (svc.github().tag_ref(&repo, &launch.tag).await, expected) {
        (Ok(Some(found)), Some(expected)) if is_ours(&found, launch.kind, &expected) => Some((
            S::Published,
            Some(found.sha),
            "GitHub shows the tag".to_owned(),
        )),
        (Ok(Some(found)), _) => Some((
            S::Collision,
            Some(found.sha.clone()),
            format!(
                "`{}` on GitHub points at {}, not the app's tag",
                launch.tag, found.sha
            ),
        )),
        (Ok(None), _) if over => Some((
            S::NotPublished,
            None,
            format!(
                "GitHub still didn't show the tag {} minutes after the push",
                RECHECK_FOR.as_secs() / 60
            ),
        )),
        // Still no answer: it stays unconfirmed, the supervisor hears so, and it is read again
        // at the next start.
        (Err(e), _) if over => Some((
            S::Unconfirmed,
            None,
            format!(
                "GitHub couldn't be read ({}); the app reads it again when it next starts",
                e.plain()
            ),
        )),
        (Ok(None) | Err(_), _) => None,
    };
    let Some((status, remote, words)) = verdict else {
        return Ok(false);
    };
    let published = svc
        .tasks()
        .write(move |tx, notify| {
            let moved = record(
                tx,
                &launch,
                &[S::Pushing, S::Unconfirmed],
                &Outcome {
                    status,
                    remote_object: remote.as_deref(),
                    words: &words,
                    at: now,
                },
            )?;
            if moved {
                let now_row = releases::load(tx, launch.id)?.unwrap_or(launch.clone());
                wake(tx, notify, &now_row, now)?;
            }
            Ok(moved && status == S::Published)
        })
        .await
        .map_err(|e| e.to_string())?;
    if published {
        observe::arm(svc, id, observe::READ_EVERY);
    }
    Ok(true)
}

/// The `release_observed` wake for the launch's supervisor.
fn wake(
    tx: &Connection,
    notify: &mut Notify,
    launch: &LaunchRow,
    at: jiff::Timestamp,
) -> Result<(), StoreError> {
    let (Some(target), Some(task)) = (launch.asked_by, launch.task) else {
        return Ok(());
    };
    let w = new_wake(
        target,
        WakeKind::ReleaseObserved,
        task,
        format!("release:{}:{}", launch.id, launch.status),
        &status_note(launch),
        &format!("tag {} {}", launch.tag, launch.status),
        serde_json::json!({ "launch": launch.id }),
        at,
    );
    if insert_wake(tx, &w)?.is_some() {
        notify.add(target);
    }
    Ok(())
}

/// The release cards the last run claimed (approved or rejected, the answer unrecorded): those
/// whose launch never got past `proposed` are pending again (nothing was sent); the rest are
/// returned with their launch, to have their answer recorded.
fn reclaim(conn: &Connection) -> Result<Vec<(HeldRow, LaunchRow)>, StoreError> {
    let ids = conn
        .prepare(
            "SELECT id FROM held_actions
             WHERE kind = ?1 AND status IN ('approved', 'rejected') AND answer IS NULL
             ORDER BY seq",
        )?
        .query_map(params![HeldKind::ReleaseTag], |r| {
            r.get::<_, HeldActionId>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for id in ids {
        let row = load_held(conn, id)?;
        let launch = releases::for_held(conn, id)?;
        if launch.as_ref().is_none_or(|l| l.status == S::Proposed) {
            // Nothing was sent: the card waits for the user again.
            conn.execute(
                "UPDATE held_actions SET status = 'pending', decided_at = NULL
                 WHERE id = ?1 AND status IN ('approved', 'rejected') AND answer IS NULL",
                params![id],
            )?;
            continue;
        }
        if let Some(launch) = launch {
            out.push((row, launch));
        }
    }
    Ok(out)
}

/// Startup, after settling's recovery and before any card can be answered: a release card the last
/// run claimed is given back when nothing was sent, or has its answer recorded from its launch
/// when the push had begun; every launch whose push isn't settled is read back from GitHub, and
/// every published tag's watch still running is armed again.
pub async fn recover(svc: &DeliveryService, held: &HeldService) -> Result<(), StoreError> {
    let claimed = svc.tasks().db().call(|conn| reclaim(conn)).await?;
    for (row, launch) in claimed {
        let you = match (&row.payload, row.status) {
            (HeldPayload::ReleaseTag(p), HeldStatus::Approved) => Some(approved(p)),
            _ => None,
        };
        let outcome = HeldOutcome {
            note: status_note(&launch),
            you,
            superseded: launch.status == S::Superseded,
        };
        if let Err(e) = held.finish_claimed(&row, outcome).await {
            tracing::warn!(held = %row.id, error = %e, "couldn't record a release card's answer");
        }
    }
    let unfinished = svc
        .tasks()
        .db()
        .call(|conn| releases::unfinished(conn))
        .await?;
    for launch in unfinished {
        reconcile(svc.clone(), launch.id);
    }
    observe::recover(svc).await
}
