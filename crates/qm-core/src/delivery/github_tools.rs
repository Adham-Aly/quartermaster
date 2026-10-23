//! `github_read` and `github_write`.
//!
//! A write in the crewmate's own project is done at once; outside it, or from an answer-only
//! task, it is a held action of kind `github_write` whose exact payload the app performs on your
//! Approve (`settling::performers::GithubWritePerformer`).

use super::excerpts::external;
use super::{DeliveryService, callers_task};
use crate::held::HeldService;
use crate::tasks::{failed, refuse};
use qm_github::{GithubRead, GithubWrite, RepoRef};
use qm_policy::github::{Repo as PolicyRepo, WriteDecision, write_decision};
use qm_store::held::{GithubWritePayload, HeldPayload, NewHeld};
use qm_store::tasks as task_store;
use qm_tools::defs::{GithubContent, GithubReadArgs, GithubWriteArgs, GithubWriteOutcome};
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::domain::GithubWriteAction;
use qm_types::ids::TaskId;

/// The most of what `github_read` returns, in characters.
pub const READ_MAX_CHARS: usize = 250_000;
/// `events.kind` of a GitHub write the app made.
pub const WRITE_EVENT: &str = "task.github_write";
/// `events.kind` of a GitHub write held for your approval.
pub const WRITE_HELD_EVENT: &str = "task.github_write_held";

/// "commented on owner/name#12", "opened an issue in owner/name", for events and notes.
pub fn write_words(action: GithubWriteAction, repo: &str, number: Option<u64>) -> String {
    let target = match number {
        Some(n) => format!("{repo}#{n}"),
        None => repo.to_owned(),
    };
    match action {
        GithubWriteAction::Comment => format!("comment on {target}"),
        GithubWriteAction::AddLabels => format!("labels added to {target}"),
        GithubWriteAction::RemoveLabels => format!("labels removed from {target}"),
        GithubWriteAction::OpenIssue => format!("new issue in {repo}"),
    }
}

fn target_repo(arg: Option<&str>, own: Option<&RepoRef>) -> Result<RepoRef, ToolFailure> {
    match arg {
        Some(r) => RepoRef::parse(r).ok_or_else(|| refuse(format!("{r:?} isn't owner/name"))),
        None => own.cloned().ok_or_else(|| {
            refuse("your project has no GitHub remote; name the repository as owner/name")
        }),
    }
}

/// `github_read`: the text, neutralised (it may be pasted on to another agent).
pub(crate) async fn read(
    svc: &DeliveryService,
    caller: &Caller,
    args: GithubReadArgs,
) -> Result<GithubContent, ToolFailure> {
    let task = callers_task(caller)?;
    let loaded = svc.load(task).await?;
    let repo = target_repo(args.repo.as_deref(), loaded.repo().as_ref())?;
    let text = svc
        .github()
        .read(
            &repo,
            &GithubRead {
                what: args.what,
                number: args.number,
                path: args.path,
                git_ref: args.git_ref,
            },
        )
        .await
        .map_err(|e| ToolFailure::Failed(e.plain()))?;
    Ok(GithubContent {
        content: external(&text, READ_MAX_CHARS),
    })
}

/// `github_write`: performed at once in the crewmate's own project, else held.
pub(crate) async fn write(
    svc: &DeliveryService,
    caller: &Caller,
    args: GithubWriteArgs,
) -> Result<GithubWriteOutcome, ToolFailure> {
    let task = callers_task(caller)?;
    let loaded = svc.load(task).await?;
    let own = loaded.repo();
    let repo = target_repo(args.repo.as_deref(), own.as_ref())?;
    let decision = write_decision(
        own.as_ref().map(|r| PolicyRepo {
            owner: &r.owner,
            name: &r.name,
        }),
        PolicyRepo {
            owner: &repo.owner,
            name: &repo.name,
        },
    );
    let words = write_words(args.action, &repo.to_string(), args.number);
    match decision {
        WriteDecision::Write => {
            let url = svc
                .github()
                .write(
                    &repo,
                    &GithubWrite {
                        action: args.action,
                        number: args.number,
                        title: args.title,
                        body: args.body,
                        labels: args.labels,
                    },
                )
                .await
                .map_err(|e| ToolFailure::Failed(e.plain()))?;
            record_write(svc, task, &words).await?;
            Ok(GithubWriteOutcome::Done { url })
        }
        WriteDecision::Hold(reason) => {
            let payload = GithubWritePayload {
                task,
                action: args.action,
                repo: repo.to_string(),
                number: args.number,
                title: args.title,
                body: args.body,
                labels: args.labels,
            };
            let at = svc.clock().now();
            let asked_by = caller.agent;
            let seat = svc.tasks().deps().seat_agent;
            let title = format!("{task}: {words}");
            let summary = format!("{task}'s {words} waits for the user's approval");
            let held = svc
                .tasks()
                .write(move |tx, _| {
                    let held = HeldService::insert(
                        tx,
                        &NewHeld {
                            payload: HeldPayload::GithubWrite(payload),
                            choices: Vec::new(),
                            title,
                            task: Some(task),
                            asked_by: Some(asked_by),
                            at,
                        },
                    )?;
                    HeldService::anchor(tx, held, &task.to_string(), seat, at)?;
                    task_store::event(
                        tx,
                        WRITE_HELD_EVENT,
                        task,
                        Some(asked_by),
                        &summary,
                        serde_json::json!({ "held": held }),
                        at,
                    )?;
                    Ok(held)
                })
                .await
                .map_err(|e| failed(&e))?;
            Ok(GithubWriteOutcome::Held {
                held_action: held,
                note: format!("{reason}. The user's answer comes back to you."),
            })
        }
    }
}

/// The event of a write the app made.
pub(crate) async fn record_write(
    svc: &DeliveryService,
    task: TaskId,
    words: &str,
) -> Result<(), ToolFailure> {
    let at = svc.clock().now();
    let summary = format!("{task}: {words}");
    svc.tasks()
        .write(move |tx, _| {
            task_store::event(
                tx,
                WRITE_EVENT,
                task,
                None,
                &summary,
                serde_json::json!({}),
                at,
            )
            .map(drop)
        })
        .await
        .map_err(|e| failed(&e))
}
