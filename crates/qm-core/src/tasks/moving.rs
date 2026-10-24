//! Moving a crewmate's workplace while it runs (`set_task_project`).
//!
//! An orchestrator moves one of its crewmates to another project, another folder, or between a
//! folder and a worktree. The crewmate keeps its name (`C-`), its task (`T-`), its supervisor and
//! its conversation; where it works, its project, branch, base, delivery and verification level
//! follow the new place, chosen as `start_crewmate` chooses them.
//!
//! ```text
//! tool call ──▶ check, settle the new base ──▶ record the move (tasks.pending_move), tell the crewmate
//!                                                   │
//!            runner, between turns (no job running) ◀┘ close the session ──▶ leave the old place
//!            ──▶ make the new one ──▶ record it (one transaction) ──▶ new brief ──▶ the session
//!            reopens in the new folder, resuming the same conversation
//! ```
//!
//! The row's own columns keep describing where the crewmate works until the move is applied
//! ([`apply`]); a task that hasn't been prepared yet (queued) moves at once, in the store alone.
//! A worktree is left only when it is clean: otherwise the crewmate is asked to commit or discard
//! its changes, and the move is tried again after its turn. The old branch stays in its old
//! repository when it has commits of its own; one without goes. When the new place can't be
//! made, the old worktree is put back and the crewmate and its supervisor are told.

use super::base::{Starter, base_request};
use super::lifecycle::{TaskEvent, next_for};
use super::start::{
    caller_authority, check_writer_slot_excluding, find_place, free_branch_names, no_free_branch,
    recorded_base, settle, standing_branch, worktree_failure,
};
use super::waits::{check_direct, load_task};
use super::{ProjectRow, TaskService, failed, load_project, new_wake, refuse};
use crate::briefs::write_brief;
use crate::inbox::Inbox;
use crate::places;
use crate::verification::wake_crewmate;
use qm_git::{GitError, Repo, RetireSpec};
use qm_policy::verification::choose as choose_level;
use qm_store::tasks::{self as task_store, PendingMove, TaskRow};
use qm_store::{agents, projects};
use qm_tools::defs::{CrewmateMoved, MoveTiming, SetTaskProjectArgs};
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::domain::{DeliveryMode, Role, TaskKind, TaskStatus, VerificationSource, WakeKind};
use qm_types::error::Refusal;
use qm_types::ids::{PortBlock, SecondmateId, TaskId};
use rusqlite::OptionalExtension;
use std::fmt::Write as _;
use std::path::Path;

/// The event recorded when a supervisor asks for a move.
pub const MOVE_ASKED_EVENT: &str = "crewmate.move_asked";
/// The event recorded when a crewmate's workplace moved.
pub const MOVED_EVENT: &str = "crewmate.moved";
/// The event recorded when a move couldn't be made; the crewmate stays where it was.
pub const MOVE_FAILED_EVENT: &str = "crewmate.move_failed";

/// The dedupe keys of a move's wakes: `{key}:{task}:{asked_at}`.
const ASKED_KEY: &str = "move_asked";
const WAITS_KEY: &str = "move_waits";

/// `set_task_project`.
#[allow(clippy::too_many_lines)]
pub(crate) async fn set_task_project(
    svc: &TaskService,
    caller: &Caller,
    args: SetTaskProjectArgs,
) -> Result<CrewmateMoved, ToolFailure> {
    let task = args.task;
    let row = load_task(svc, task).await?;
    check_direct(svc, caller, &row).await?;
    check_movable(&row).map_err(ToolFailure::Refused)?;
    let waiting = svc
        .db()
        .call(move |conn| task_store::pending_move(conn, task))
        .await
        .map_err(|e| failed(&e))?;
    if !row.in_place {
        check_undelivered(svc, &row).await?;
    }

    // The new place, and the project it gets.
    let place = find_place(svc, args.project, args.folder, args.create_folder).await?;
    let (home, now) = (svc.deps().home.clone(), svc.clock().now());
    let preview_place = place.clone();
    let (project, current_home) = svc
        .db()
        .call(move |conn| {
            let project = places::preview(conn, &preview_place, &home, now)?;
            let current = agents::crewmate_of(conn, task)?.map(|a| a.home);
            Ok((project, current))
        })
        .await
        .map_err(|e| failed(&e))?;
    let in_place = args.in_place || project.delivery == DeliveryMode::Folder;
    if in_place && args.base_branch.is_some() {
        return Err(refuse(
            "a task working in place has no branches, so a base branch can't be named",
        ));
    }
    let same_project = project.id == row.project && project.path == place.root;
    if same_project && in_place == row.in_place {
        let here = if in_place {
            current_home.as_deref() == Some(place.folder.as_path())
        } else {
            true
        };
        if here {
            // Naming where it works calls off a move that waits.
            return match waiting {
                Some(waiting) => cancel(svc, &row, &waiting, caller.agent).await,
                None => Err(refuse(format!(
                    "{task} already works there; name another project or folder"
                ))),
            };
        }
    }
    let delivery = if in_place {
        DeliveryMode::Folder
    } else {
        project.delivery
    };

    // The new base, settled now (it may fetch), and the delivery the work needs.
    let base = if in_place {
        None
    } else {
        let request = base_for(svc, caller, args.base_branch.as_deref())
            .await?
            .map_err(ToolFailure::Refused)?;
        Some(settle(svc, &project, request).await?)
    };
    if row.kind.can_change_code() && delivery == DeliveryMode::DirectPr {
        let readiness = svc.deps().probe.probe(&project, &project.path).await;
        if readiness.has_gaps() {
            return Err(refuse(format!(
                "{task} can't move there: {}",
                readiness.gaps.join("; ")
            )));
        }
    }
    let branch_names = if in_place {
        None
    } else {
        free_branch_names(svc, &project, &row.title).await
    };
    // Your words for this task keep their level; anything else follows the new project.
    let user_level = row
        .verification
        .filter(|(_, source)| *source == VerificationSource::User)
        .map(|(level, _)| level);
    let level = choose_level(delivery, user_level, Some(project.verification));

    // The record, in one transaction.
    let home = svc.deps().home.clone();
    let at = svc.clock().now();
    let worktree = svc.deps().data_dir.worktree(task);
    let supervisor = caller.agent;
    let queued = row.status == TaskStatus::Queued;
    let recorded = svc
        .write(move |tx, notify| {
            let Some(now_row) = task_store::load(tx, task)? else {
                return Ok(Err(Refusal::new(format!("{task} doesn't exist"))));
            };
            if let Err(r) = check_movable(&now_row) {
                return Ok(Err(r));
            }
            if in_place
                && let Err(r) = check_writer_slot_excluding(tx, task, &place.folder, now_row.kind)?
            {
                return Ok(Err(r));
            }
            // A move that waits is replaced by this one.
            let replaced = task_store::pending_move(tx, task)?;
            if let Some(before) = &replaced {
                task_store::set_pending_move(tx, task, None)?;
                forget_news(tx, task, before.asked_at)?;
            }
            let project = places::register(tx, &place, &home, at)?;
            let branch = match &branch_names {
                None => None,
                Some(names) => match task_store::first_free_branch(tx, names)? {
                    Some(name) => Some(name),
                    None => return Ok(Err(no_free_branch(&now_row.title))),
                },
            };
            let pending = PendingMove {
                project: project.id,
                in_place,
                delivery,
                home: if in_place {
                    place.folder.clone()
                } else {
                    worktree.clone()
                },
                branch,
                base_branch: base
                    .as_ref()
                    .and_then(qm_git::SettledBase::typed_branch)
                    .map(str::to_owned),
                base_ref: base.as_ref().map(|b| b.base_ref.clone()),
                base_commit: base.as_ref().map(|b| b.commit.clone()),
                verification: level.map(|l| (l.level, l.source)),
                asked_at: at.as_millisecond(),
            };
            task_store::set_pending_move(tx, task, Some(&pending))?;
            if let Some(before) = replaced {
                leave_destination(tx, &now_row, &before, at)?;
            }
            let from = projects::load(tx, now_row.project)?.map_or_else(String::new, |p| p.name);
            task_store::event(
                tx,
                MOVE_ASKED_EVENT,
                task,
                Some(supervisor),
                &format!("{task} is moving from {from} to {}", project.name),
                serde_json::json!({
                    "from": now_row.project,
                    "to": project.id,
                    "home": pending.home,
                    "in_place": in_place,
                    "branch": pending.branch,
                }),
                at,
            )?;
            // A queued crewmate hasn't started: its brief will say where it works.
            if !queued {
                let text = format!(
                    "Your supervisor is moving you to {}: you'll work in {}{}. The move happens \
                     between your turns, once none of your background jobs runs; your \
                     conversation carries on, and you'll be told when it's done.{}",
                    project.name,
                    pending.home.display(),
                    branch_words(&pending),
                    if now_row.in_place {
                        String::new()
                    } else {
                        " Commit or discard your uncommitted changes first: a worktree is left \
                         only when it's clean."
                            .to_owned()
                    },
                );
                wake_crewmate(
                    tx,
                    notify,
                    task,
                    WakeKind::WorkplaceChanged,
                    format!("{ASKED_KEY}:{task}:{}", pending.asked_at),
                    &text,
                    &format!("moving to {}", project.name),
                    at,
                )?;
            }
            Ok(Ok((project, pending)))
        })
        .await
        .map_err(|e| failed(&e))?;
    let (project, pending) = recorded.map_err(ToolFailure::Refused)?;
    svc.changed();
    // No runner to apply it between turns: apply it now.
    let when = if svc.has_runner(task) {
        MoveTiming::BetweenTurns
    } else {
        match apply(svc, task, None).await {
            Applied::Moved => MoveTiming::Moved,
            Applied::Nothing | Applied::Waiting => MoveTiming::BetweenTurns,
            Applied::Failed(reason) => {
                return Err(refuse(format!("{task} couldn't move: {reason}")));
            }
        }
    };
    tracing::info!(%task, project = %project.name, ?when, "crewmate move asked");
    Ok(CrewmateMoved {
        crewmate: task,
        project: project.name,
        folder: pending.home.display().to_string(),
        in_place: pending.in_place,
        branch: pending.branch,
        base: pending.base_ref,
        when,
    })
}

/// Calls off `task`'s waiting move: it stays where it works, and it is told unless it never heard.
async fn cancel(
    svc: &TaskService,
    row: &TaskRow,
    waiting: &PendingMove,
    supervisor: qm_types::ids::AgentId,
) -> Result<CrewmateMoved, ToolFailure> {
    let task = row.id;
    let at = svc.clock().now();
    let asked_at = waiting.asked_at;
    let home = waiting.home.clone();
    let here = svc
        .write(move |tx, notify| {
            let Some(now) = task_store::pending_move(tx, task)? else {
                return Ok(None);
            };
            if now.asked_at != asked_at {
                return Ok(None);
            }
            task_store::set_pending_move(tx, task, None)?;
            if let Some(row) = task_store::load(tx, task)? {
                leave_destination(tx, &row, &now, at)?;
            }
            let told = !forget_news(tx, task, asked_at)?;
            task_store::event(
                tx,
                MOVE_FAILED_EVENT,
                task,
                Some(supervisor),
                &format!("{task}'s move to {} was called off", home.display()),
                serde_json::json!({ "home": home, "reason": "called off" }),
                at,
            )?;
            let crewmate = agents::crewmate_of(tx, task)?;
            if told {
                wake_crewmate(
                    tx,
                    notify,
                    task,
                    WakeKind::WorkplaceChanged,
                    format!("move_called_off:{task}:{asked_at}"),
                    &format!(
                        "Your supervisor called off your move to {}: you stay where you work.",
                        home.display()
                    ),
                    "move called off",
                    at,
                )?;
            }
            Ok(crewmate.map(|a| a.home))
        })
        .await
        .map_err(|e| failed(&e))?;
    let Some(here) = here else {
        return Err(refuse(format!(
            "{task}'s move was made or changed meanwhile; look at it again"
        )));
    };
    svc.changed();
    let name = svc
        .db()
        .call({
            let project = row.project;
            move |conn| Ok(projects::load(conn, project)?.map(|p| p.name))
        })
        .await
        .map_err(|e| failed(&e))?
        .unwrap_or_default();
    Ok(CrewmateMoved {
        crewmate: task,
        project: name,
        folder: here.display().to_string(),
        in_place: row.in_place,
        branch: (!row.branch.is_empty()).then(|| row.branch.clone()),
        base: row.base_ref.clone(),
        when: MoveTiming::CalledOff,
    })
}

/// Drops the news of `task`'s move asked at `asked_at` that its crewmate hasn't read yet. Returns
/// whether every piece was unread (the crewmate never heard of the move).
fn forget_news(
    conn: &rusqlite::Connection,
    task: TaskId,
    asked_at: i64,
) -> Result<bool, qm_store::StoreError> {
    let Some(agent) = agents::crewmate_of(conn, task)? else {
        return Ok(true);
    };
    let asked = format!("{ASKED_KEY}:{task}:{asked_at}");
    let unread = qm_store::wakes::withdraw_pending(conn, agent.id, &asked)?;
    let waits = format!("{WAITS_KEY}:{task}:{asked_at}");
    qm_store::wakes::withdraw_pending(conn, agent.id, &waits)?;
    Ok(unread)
}

/// A move that won't be made no longer keeps its destination listed: the project goes when no
/// other work is there (a new project is listed from the moment a move to it is asked for).
fn leave_destination(
    conn: &rusqlite::Connection,
    row: &TaskRow,
    to: &PendingMove,
    at: jiff::Timestamp,
) -> Result<(), qm_store::StoreError> {
    if to.project != row.project {
        places::retire_if_idle(conn, to.project, at)?;
    }
    Ok(())
}

/// Whether `row` can move now.
fn check_movable(row: &TaskRow) -> Result<(), Refusal> {
    let task = row.id;
    if row.kind == TaskKind::Review {
        return Err(Refusal::new(format!(
            "{task} is a review, which works at the reviewed head; start a new review there instead"
        )));
    }
    match row.status {
        TaskStatus::Queued
        | TaskStatus::Running
        | TaskStatus::Waiting
        | TaskStatus::Held
        | TaskStatus::Stuck
        | TaskStatus::Reported => Ok(()),
        TaskStatus::Preparing => Err(Refusal::new(format!(
            "{task} is still being prepared; move it once it runs"
        ))),
        TaskStatus::InReview | TaskStatus::Settling => Err(Refusal::new(format!(
            "{task}'s work is {}; move it once that's settled",
            row.status
        ))),
        TaskStatus::Landed
        | TaskStatus::Closed
        | TaskStatus::Failed
        | TaskStatus::Stopped
        | TaskStatus::Discarded => Err(Refusal::new(format!(
            "{task} is {}; start a new crewmate there instead",
            row.status
        ))),
    }
}

/// A worktree whose branch was pushed, or has a pull request, stays with its delivery.
async fn check_undelivered(svc: &TaskService, row: &TaskRow) -> Result<(), ToolFailure> {
    let task = row.id;
    let delivery = svc
        .db()
        .call(move |conn| qm_store::delivery::load(conn, task))
        .await
        .map_err(|e| failed(&e))?;
    let pr = row.pr_number.or_else(|| {
        delivery
            .as_ref()
            .and_then(|d| d.pr_number)
            .and_then(|n| i64::try_from(n).ok())
    });
    if let Some(pr) = pr {
        return Err(refuse(format!(
            "{task} has pull request #{pr} open from its branch; settle or stop that work, or \
             start a new crewmate there"
        )));
    }
    if delivery.is_some_and(|d| d.pushed_head.is_some()) {
        return Err(refuse(format!(
            "{task}'s branch {} was pushed; settle or stop that work, or start a new crewmate \
             there",
            row.branch
        )));
    }
    Ok(())
}

/// The base request for the caller's move, as `start_crewmate` makes it: your typed branch, or a
/// secondmate's standing branch, else the default.
async fn base_for(
    svc: &TaskService,
    caller: &Caller,
    typed: Option<&str>,
) -> Result<Result<qm_git::BaseRequest, Refusal>, ToolFailure> {
    let standing = match (caller.role, caller.secondmate) {
        (Role::Secondmate, Some(sm)) => svc
            .db()
            .call(move |conn| standing_branch(conn, sm))
            .await
            .map_err(|e| failed(&e))?,
        _ => None,
    };
    let starter = if caller.role == Role::Secondmate {
        Starter::Secondmate {
            request: None,
            standing: standing.as_deref(),
            typed,
        }
    } else {
        Starter::Quartermaster {
            authority: caller_authority(caller),
            typed,
        }
    };
    Ok(base_request(starter, None))
}

/// ", on branch x from origin/main", or nothing in place.
fn branch_words(to: &PendingMove) -> String {
    match (&to.branch, &to.base_ref) {
        (Some(branch), Some(base)) => format!(", on a new branch {branch} from {base}"),
        (Some(branch), None) => format!(", on a new branch {branch}"),
        _ => String::new(),
    }
}

/// What [`apply`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Applied {
    /// No move waits.
    Nothing,
    /// The crewmate now works in its new place.
    Moved,
    /// The move waits: its worktree has uncommitted changes (it was asked to deal with them).
    Waiting,
    /// The move couldn't be made and was dropped; the crewmate stays where it was (both told).
    Failed(String),
}

/// Applies `task`'s waiting move. Call only while no session of the crewmate's is open and none
/// of its jobs runs: its folder changes under it. `cancel` stops the project's setup command
/// (the runner's; `None` without a runner).
#[allow(clippy::too_many_lines)]
pub(crate) async fn apply(
    svc: &TaskService,
    task: TaskId,
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Applied {
    let read = svc
        .db()
        .call(move |conn| {
            let row = task_store::load(conn, task)?;
            let pending = task_store::pending_move(conn, task)?;
            let agent = agents::crewmate_of(conn, task)?;
            let (old, new) = match (&row, &pending) {
                (Some(r), Some(p)) => (
                    load_project(conn, r.project)?,
                    load_project(conn, p.project)?,
                ),
                _ => (None, None),
            };
            Ok((row, pending, agent, old, new))
        })
        .await;
    let (row, pending, agent, old, new) = match read {
        Ok((Some(row), Some(pending), Some(agent), Some(old), Some(new))) => {
            (row, pending, agent, old, new)
        }
        Ok((Some(_), Some(_), ..)) => {
            return drop_move(svc, task, "its project or crewmate is gone").await;
        }
        Ok(_) => return Applied::Nothing,
        Err(e) => {
            tracing::warn!(%task, error = %e, "a crewmate's move couldn't be read");
            return Applied::Nothing;
        }
    };
    // Being prepared: its runner moves it before its first turn.
    if row.status == TaskStatus::Preparing {
        return Applied::Nothing;
    }
    // What made the move possible may have changed since it was asked for: its work went to
    // review, or its branch was pushed.
    let still = match check_movable(&row) {
        Err(r) => Err(r.reason),
        Ok(()) if row.in_place => Ok(()),
        Ok(()) => check_undelivered(svc, &row).await.map_err(|e| match e {
            ToolFailure::Refused(r) => r.reason,
            ToolFailure::Failed(why) => why,
        }),
    };
    if let Err(reason) = still {
        return drop_move(svc, task, &reason).await;
    }
    // Never prepared: nothing on disk yet, and preparing makes the new place.
    if row.status == TaskStatus::Queued {
        return match record(svc, &row, agent.id, &pending, None).await {
            Ok(()) => Applied::Moved,
            Err(e) => drop_move(svc, task, &e).await,
        };
    }
    let paths = svc.paths_of(&row);
    let git = svc.deps().git.clone();

    // 1. Leave the old place. A worktree goes only when it is clean; its branch stays for now.
    if !row.in_place {
        let (g, checkout, wt) = (git.clone(), old.path.clone(), paths.worktree.clone());
        let clean = tokio::task::spawn_blocking(move || {
            Repo::open(g, &checkout).and_then(|repo| repo.worktree_is_clean(&wt))
        })
        .await;
        match clean {
            Ok(Ok(true) | Err(GitError::NotAWorktree { .. })) => {}
            Ok(Ok(false)) => {
                ask_to_commit(svc, task, &paths.worktree, &row.branch, &pending).await;
                return Applied::Waiting;
            }
            Ok(Err(e)) => {
                return drop_move(svc, task, &format!("its worktree couldn't be read: {e}")).await;
            }
            Err(e) => return drop_move(svc, task, &e.to_string()).await,
        }
        let (g, checkout, branch, p) = (
            git.clone(),
            old.path.clone(),
            row.branch.clone(),
            paths.clone(),
        );
        let retired = tokio::task::spawn_blocking(move || {
            Repo::open(g, &checkout)
                .and_then(|repo| repo.retire(&RetireSpec::new(&branch, &p).keep_branch(true)))
        })
        .await;
        match retired {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                return drop_move(svc, task, &format!("its worktree couldn't be removed: {e}"))
                    .await;
            }
            Err(e) => return drop_move(svc, task, &e.to_string()).await,
        }
    }

    // 2. Make the new one: a worktree on the new branch, or the folder, which must be there.
    let made = arrive(svc, task, &pending, &new, &paths).await;
    if let Err(reason) = made {
        if !row.in_place {
            let (g, checkout, branch, p) = (
                git.clone(),
                old.path.clone(),
                row.branch.clone(),
                paths.clone(),
            );
            let back = tokio::task::spawn_blocking(move || {
                Repo::open(g, &checkout).and_then(|repo| repo.reattach_worktree(&branch, &p))
            })
            .await;
            if !matches!(back, Ok(Ok(_))) {
                tracing::error!(%task, ?back, "a crewmate's old worktree couldn't be put back after a failed move");
            }
        }
        return drop_move(svc, task, &reason).await;
    }

    // 3. The project's setup command, in the new worktree.
    let mut notes = Vec::new();
    if !pending.in_place
        && let Some(command) = new.setup_command.clone().filter(|c| !c.trim().is_empty())
    {
        let ports = PortBlock {
            base: row.port_base.unwrap_or_default(),
        };
        let cancel = cancel.unwrap_or_else(|| tokio::sync::watch::channel(false).1);
        match super::setup::run(svc, task, &paths, &pending.home, &command, ports, cancel).await {
            Ok(_) => {}
            Err(e) => notes.push(format!(
                "{e}. Run the project's setup command yourself before you rely on the worktree: {command}"
            )),
        }
    }

    // 4. The old branch: kept when it has commits of its own, else deleted.
    if !row.in_place && !row.branch.is_empty() {
        let kept = match row.base_commit.clone() {
            Some(base) => {
                let (g, checkout, branch) = (git.clone(), old.path.clone(), row.branch.clone());
                let deleted = tokio::task::spawn_blocking(move || {
                    Repo::open(g, &checkout)
                        .and_then(|repo| repo.delete_branch_if_at(&branch, &base))
                })
                .await;
                !matches!(deleted, Ok(Ok(true)))
            }
            None => true,
        };
        if kept {
            notes.push(format!(
                "Your old branch {} stays in {} with its commits.",
                row.branch,
                old.path.display()
            ));
        }
    }

    // 5. Record it, write the new brief, tell the crewmate.
    if let Err(e) = record(svc, &row, agent.id, &pending, Some(notes)).await {
        tracing::error!(%task, error = %e, "a crewmate's move was made but couldn't be recorded");
        return drop_move(svc, task, &e).await;
    }
    if let Err(e) = write_brief(svc.db(), &svc.deps().data_dir, task).await {
        tracing::warn!(%task, error = %e, "a moved crewmate's brief couldn't be written");
    }
    svc.changed();
    tracing::info!(%task, home = %pending.home.display(), "crewmate moved");
    Applied::Moved
}

/// Makes the new place: a worktree on the move's branch from its base, or checks the folder.
async fn arrive(
    svc: &TaskService,
    task: TaskId,
    to: &PendingMove,
    project: &ProjectRow,
    paths: &qm_types::data_dir::TaskPaths,
) -> Result<(), String> {
    if to.in_place {
        let dir = to.home.clone();
        let there = tokio::task::spawn_blocking(move || dir.is_dir())
            .await
            .map_err(|e| e.to_string())?;
        return if there {
            Ok(())
        } else {
            Err(format!("{} isn't there", to.home.display()))
        };
    }
    let (Some(branch), Some(settled)) = (
        to.branch.clone(),
        recorded_base(
            project,
            to.base_branch.as_deref(),
            to.base_ref.as_deref(),
            to.base_commit.as_ref(),
        ),
    ) else {
        return Err("the move has no branch or base recorded".to_owned());
    };
    let (git, checkout, files, p) = (
        svc.deps().git.clone(),
        project.path.clone(),
        project.files_to_copy.clone(),
        paths.clone(),
    );
    tokio::task::spawn_blocking(move || {
        Repo::open(git, &checkout)
            .and_then(|repo| repo.create_worktree(&branch, &p, &settled, &files))
    })
    .await
    .map_err(|e| e.to_string())?
    .map(drop)
    .map_err(|e| worktree_failure(task, &e))
}

/// The move, recorded in one transaction: the row and the crewmate's home, the status a moved
/// crewmate gets back to work in, the secondmate's projects, the old project's retirement, the
/// event and the crewmate's wake. `notes` are the wake's last lines; `None` for a queued task,
/// which isn't told (its brief says where it works).
async fn record(
    svc: &TaskService,
    row: &TaskRow,
    agent: qm_types::ids::AgentId,
    to: &PendingMove,
    notes: Option<Vec<String>>,
) -> Result<(), String> {
    let task = row.id;
    let (old, from_home) = (row.project, row.in_place);
    let to = to.clone();
    let at = svc.clock().now();
    svc.write(move |tx, notify| {
        let Some(now) = task_store::load(tx, task)? else {
            return Ok(Err(format!("{task} doesn't exist")));
        };
        if task_store::pending_move(tx, task)?.as_ref() != Some(&to) {
            return Ok(Err("the move changed while it was being made".to_owned()));
        }
        if to.in_place
            && let Err(r) = check_writer_slot_excluding(tx, task, &to.home, now.kind)?
        {
            return Ok(Err(r.to_string()));
        }
        task_store::apply_move(tx, task, &to)?;
        agents::set_home(tx, agent, &to.home)?;
        // Back to work in the new place: a report on the old one is done with.
        match now.status {
            TaskStatus::Reported => {
                task_store::set_status(tx, task, now.status, TaskStatus::Running, at)?;
            }
            TaskStatus::Stuck => {
                if let Ok(next) = next_for(task, now.status, TaskEvent::Steered) {
                    task_store::set_status(tx, task, now.status, next, at)?;
                    crate::decisions::hold::supersede_stuck(tx, task, at)?;
                }
            }
            _ => {}
        }
        let from = projects::load(tx, old)?.map_or_else(String::new, |p| p.name);
        let name = projects::load(tx, to.project)?.map_or_else(String::new, |p| p.name);
        task_store::event(
            tx,
            MOVED_EVENT,
            task,
            None,
            &format!("{task} moved from {from} to {name}"),
            serde_json::json!({
                "from": old,
                "to": to.project,
                "from_in_place": from_home,
                "home": to.home,
                "in_place": to.in_place,
                "branch": to.branch,
            }),
            at,
        )?;
        let sm: Option<SecondmateId> = tx
            .query_row(
                "SELECT secondmate_id FROM agents WHERE task_id = ?1 AND role = 'crewmate'",
                rusqlite::params![task],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if let Some(sm) = sm {
            qm_store::secondmates::sync_projects(tx, sm)?;
        }
        if old != to.project {
            places::retire_if_idle(tx, old, at)?;
        }
        if let Some(notes) = notes {
            // The crewmate hears it once: news of the move it hasn't read yet goes.
            forget_news(tx, task, to.asked_at)?;
            let mut text = format!(
                "You've moved to {name}: you now work in {}{}. Your conversation carries on; \
                 your brief is updated, and everything else about your task stays.",
                to.home.display(),
                branch_words(&to)
            );
            if to.in_place {
                text.push_str(" You work in the folder itself: there's no branch.");
            }
            for note in notes {
                let _ = write!(text, " {note}");
            }
            wake_crewmate(
                tx,
                notify,
                task,
                WakeKind::WorkplaceChanged,
                format!("moved:{task}:{}", to.asked_at),
                &text,
                &format!("moved to {name}"),
                at,
            )?;
        }
        Ok(Ok(()))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Drops a move that couldn't be made: the crewmate stays where it was, and it and its
/// supervisor are told why.
async fn drop_move(svc: &TaskService, task: TaskId, reason: &str) -> Applied {
    let at = svc.clock().now();
    let why = reason.to_owned();
    let written = svc
        .write(move |tx, notify| {
            let Some(row) = task_store::load(tx, task)? else {
                return Ok(());
            };
            let Some(to) = task_store::pending_move(tx, task)? else {
                return Ok(());
            };
            task_store::set_pending_move(tx, task, None)?;
            leave_destination(tx, &row, &to, at)?;
            task_store::event(
                tx,
                MOVE_FAILED_EVENT,
                task,
                None,
                &format!("{task} couldn't move: {why}"),
                serde_json::json!({ "home": to.home, "reason": why }),
                at,
            )?;
            let here = agents::crewmate_of(tx, task)?
                .map_or_else(String::new, |a| a.home.display().to_string());
            let key = format!("move_failed:{task}:{}", to.asked_at);
            wake_crewmate(
                tx,
                notify,
                task,
                WakeKind::WorkplaceChanged,
                key.clone(),
                &format!(
                    "Your move to {} couldn't be made ({why}). You still work in {here}, as before.",
                    to.home.display()
                ),
                "move failed",
                at,
            )?;
            Inbox::post(
                tx,
                &new_wake(
                    row.supervisor,
                    WakeKind::WorkplaceChanged,
                    task,
                    key,
                    &format!(
                        "{task} couldn't move to {}: {why}. It still works in {here}.",
                        to.home.display()
                    ),
                    &format!("{task} didn't move"),
                    serde_json::json!({}),
                    at,
                ),
                notify,
            )?;
            Ok(())
        })
        .await;
    if let Err(e) = written {
        tracing::error!(%task, error = %e, "a failed move couldn't be recorded");
    }
    svc.changed();
    Applied::Failed(reason.to_owned())
}

/// Asks the crewmate, once per move, to commit or discard its changes so it can move.
async fn ask_to_commit(
    svc: &TaskService,
    task: TaskId,
    worktree: &Path,
    branch: &str,
    to: &PendingMove,
) {
    let at = svc.clock().now();
    let text = format!(
        "Your move to {} waits: your worktree {} has uncommitted changes. Commit them on {branch} \
         (they stay on that branch in its repository) or discard them, then end your turn; the \
         move happens after it.",
        to.home.display(),
        worktree.display()
    );
    let key = format!("{WAITS_KEY}:{task}:{}", to.asked_at);
    let written = svc
        .write(move |tx, notify| {
            wake_crewmate(
                tx,
                notify,
                task,
                WakeKind::WorkplaceChanged,
                key,
                &text,
                "move waits for a clean worktree",
                at,
            )
            .map(drop)
        })
        .await;
    if let Err(e) = written {
        tracing::warn!(%task, error = %e, "a waiting move's wake couldn't be posted");
    }
}

/// Whether a move waits for `task`.
pub(crate) async fn is_waiting(svc: &TaskService, task: TaskId) -> bool {
    svc.db()
        .call(move |conn| task_store::pending_move(conn, task))
        .await
        .is_ok_and(|m| m.is_some())
}
