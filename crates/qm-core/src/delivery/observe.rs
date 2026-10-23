//! After a release tag is published: what GitHub shows for the tag, for its GitHub release and for
//! the workflow runs the tag started, kept as three separate facts. Metadata only: every read is
//! one of `qm_github::release_meta`'s closed endpoints, and nothing is downloaded or triggered.
//!
//! - A launch's watch starts in the same transaction that records it `published`
//!   (`releases::start_observing`), and is read every [`READ_EVERY`] for [`FAST_FOR`], then every
//!   [`READ_SLOW`] (also when GitHub's rate limit runs low), for at most [`WATCH_FOR`] after the
//!   publication.
//! - It ends early when nothing more is expected: the workflows are settled (every run for the tag
//!   completed, none set up, all disabled, none started within [`RUNS_GRACE`], or unreadable for
//!   good) and the release is settled (published with every asset uploaded, or unreadable for good)
//!   or unchanged for [`QUIET_FOR`]. A tag GitHub no longer shows at the app's object ends it too.
//! - Each reading is stored on the launch, so it survives a restart, and `recover` arms every watch
//!   still running. The end wakes the launch's supervisor once (`release_observed`).
//! - "No workflows" is said only when GitHub counts none; missing runs alone never mean that. A
//!   release is "published" only when GitHub's release for the tag is, never from the tag alone.

use super::DeliveryService;
use super::checks::RATE_LOW;
use super::excerpts::external;
use crate::inbox::Notify;
use crate::secondmates::scope::may_read_task;
use crate::tasks::new_wake;
use jiff::{SignedDuration, Timestamp};
use qm_github::release_meta::{self as meta, ReleaseMeta, ReleaseRead, ReleaseReads, RunMeta};
use qm_github::{GithubError, RepoRef};
use qm_store::StoreError;
use qm_store::releases::{self, LaunchRow, ObserveState};
use qm_store::tasks as task_store;
use qm_store::wakes::insert_wake;
use qm_tools::defs::{ReleaseLaunchReport, ReleaseStatus};
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::domain::{ReleaseLaunchStatus as S, WakeKind};
use qm_types::error::Refusal;
use qm_types::ids::{ReleaseLaunchId, TaskId};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::time::Duration;

/// How long after its publication a tag is watched at most.
pub const WATCH_FOR: Duration = Duration::from_hours(3);
/// How long after the publication the watch reads every [`READ_EVERY`].
pub const FAST_FOR: Duration = Duration::from_mins(15);
pub const READ_EVERY: Duration = Duration::from_secs(30);
/// The reading interval after [`FAST_FOR`], and whenever GitHub's rate limit runs low.
pub const READ_SLOW: Duration = Duration::from_secs(120);
/// How long the release may stay unchanged before it counts as settled.
pub const QUIET_FOR: Duration = Duration::from_mins(10);
/// How long after the publication a run for the tag may take to start.
pub const RUNS_GRACE: Duration = Duration::from_mins(10);
/// How old a reading may be before `release_status` reads GitHub again.
pub const FRESH_FOR: Duration = Duration::from_secs(60);
/// The most asset names and runs a reading keeps.
const MAX_ASSETS: usize = 20;
const MAX_RUNS: usize = 10;
/// The most characters of a name from the repository (a release, asset or workflow).
const NAME_CHARS: usize = 80;
/// The most characters of a reason a read failed.
const REASON_CHARS: usize = 200;

/// The tag, read again.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TagFact {
    /// Not read again since GitHub showed it at publication.
    #[default]
    Unread,
    /// GitHub shows it at the app's object.
    Published,
    /// GitHub shows it at another object now.
    Moved {
        now: String,
    },
    /// GitHub no longer shows it.
    Gone,
    Unavailable {
        reason: String,
        transient: bool,
    },
}

/// The GitHub release for the tag.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReleaseFact {
    #[default]
    Unread,
    Unavailable {
        reason: String,
        transient: bool,
    },
    /// No release for the tag, published or draft.
    NoneYet,
    /// A draft: nothing is offered for download.
    Draft {
        name: Option<String>,
    },
    Published {
        name: Option<String>,
        prerelease: bool,
        assets: u32,
        uploaded: u32,
        /// The first [`MAX_ASSETS`] assets' names, neutralised.
        names: Vec<String>,
    },
}

/// The workflows set up in the repository.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WorkflowsFact {
    #[default]
    Unread,
    Unavailable {
        reason: String,
        transient: bool,
    },
    /// GitHub counts no workflow at all.
    NotConfigured,
    AllDisabled {
        count: u64,
    },
    Active {
        active: u64,
        total: u64,
    },
}

/// One run for the tag.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFact {
    pub name: String,
    pub event: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
}

/// The workflow runs on the tag's commit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RunsFact {
    #[default]
    Unread,
    Unavailable {
        reason: String,
        transient: bool,
    },
    Seen {
        /// Runs started by the tag (its commit and its name), newest first, at most [`MAX_RUNS`].
        for_tag: Vec<RunFact>,
        /// Runs for the tag past [`MAX_RUNS`].
        #[serde(default)]
        more: u32,
        /// Other runs on the same commit (a branch push, say), not counted as the tag's.
        #[serde(default)]
        other: u32,
    },
}

/// Why a watch ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ended {
    /// Nothing more was expected.
    Settled,
    /// GitHub no longer shows the tag at the app's object.
    TagChanged,
    /// [`WATCH_FOR`] passed.
    Deadline,
}

/// One reading of a published launch, as stored on it (`release_launches.observation`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    #[serde(default)]
    pub tag: TagFact,
    #[serde(default)]
    pub release: ReleaseFact,
    #[serde(default)]
    pub workflows: WorkflowsFact,
    #[serde(default)]
    pub runs: RunsFact,
    /// Since when the release fact has been what it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_since: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_at: Option<Timestamp>,
    /// How many readings were made.
    #[serde(default)]
    pub reads: u32,
    /// The last read that failed for a passing reason, while an earlier answer is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<Ended>,
}

impl Observation {
    /// The reading stored on `launch`, or none yet.
    pub fn of(launch: &LaunchRow) -> Self {
        launch
            .observation
            .as_ref()
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }
}

fn reason(e: &GithubError) -> (String, bool) {
    (
        external(&e.plain().replace(['\n', '\r'], " "), REASON_CHARS),
        e.is_transient(),
    )
}

fn name(raw: &str) -> String {
    external(&raw.replace(['\n', '\r'], " "), NAME_CHARS)
}

fn since(a: Timestamp, b: Timestamp) -> Duration {
    Duration::try_from(b.duration_since(a)).unwrap_or_default()
}

fn release_fact(read: Result<ReleaseRead, GithubError>) -> ReleaseFact {
    let published = |m: ReleaseMeta| {
        let uploaded = m.assets.iter().filter(|a| a.uploaded).count();
        ReleaseFact::Published {
            name: m.name.as_deref().map(name),
            prerelease: m.prerelease,
            assets: u32::try_from(m.assets.len()).unwrap_or(u32::MAX),
            uploaded: u32::try_from(uploaded).unwrap_or(u32::MAX),
            names: m
                .assets
                .iter()
                .take(MAX_ASSETS)
                .map(|a| name(&a.name))
                .collect(),
        }
    };
    match read {
        Ok(ReleaseRead::Published(m)) => published(m),
        Ok(ReleaseRead::Draft(m)) => ReleaseFact::Draft {
            name: m.name.as_deref().map(name),
        },
        Ok(_) => ReleaseFact::NoneYet,
        Err(e) => {
            let (reason, transient) = reason(&e);
            ReleaseFact::Unavailable { reason, transient }
        }
    }
}

fn workflows_fact(read: Result<meta::WorkflowsMeta, GithubError>) -> WorkflowsFact {
    match read {
        Ok(w) if w.total == 0 => WorkflowsFact::NotConfigured,
        // "All disabled" only when every workflow was read.
        Ok(w) if w.active == 0 && w.total <= u64::from(meta::WORKFLOWS_PAGE) => {
            WorkflowsFact::AllDisabled { count: w.total }
        }
        Ok(w) => WorkflowsFact::Active {
            active: w.active,
            total: w.total,
        },
        Err(e) => {
            let (reason, transient) = reason(&e);
            WorkflowsFact::Unavailable { reason, transient }
        }
    }
}

fn runs_fact(read: Result<Vec<RunMeta>, GithubError>, launch: &LaunchRow) -> RunsFact {
    let runs = match read {
        Ok(runs) => runs,
        Err(e) => {
            let (reason, transient) = reason(&e);
            return RunsFact::Unavailable { reason, transient };
        }
    };
    let on_commit = |r: &RunMeta| r.head_sha.eq_ignore_ascii_case(launch.commit.as_str());
    // A tag push's run names the tag as its head; a branch push's on the same commit doesn't.
    let (tag_runs, other): (Vec<&RunMeta>, Vec<&RunMeta>) = runs
        .iter()
        .filter(|r| on_commit(r))
        .partition(|r| r.head_branch.as_deref() == Some(launch.tag.as_str()));
    let for_tag: Vec<RunFact> = tag_runs
        .iter()
        .take(MAX_RUNS)
        .map(|r| RunFact {
            name: name(&r.name),
            event: name(&r.event),
            status: name(&r.status),
            conclusion: r.conclusion.as_deref().map(name),
        })
        .collect();
    RunsFact::Seen {
        more: u32::try_from(tag_runs.len().saturating_sub(MAX_RUNS)).unwrap_or(u32::MAX),
        other: u32::try_from(other.len()).unwrap_or(u32::MAX),
        for_tag,
    }
}

/// Whether nothing more is expected from the workflows.
fn workflows_settled(o: &Observation, published: Timestamp, now: Timestamp) -> bool {
    if let RunsFact::Seen { for_tag, .. } = &o.runs
        && !for_tag.is_empty()
    {
        return for_tag.iter().all(|r| r.status == "completed");
    }
    match &o.workflows {
        WorkflowsFact::NotConfigured
        | WorkflowsFact::AllDisabled { .. }
        | WorkflowsFact::Unavailable {
            transient: false, ..
        } => true,
        WorkflowsFact::Active { .. } => match &o.runs {
            RunsFact::Seen { .. } => since(published, now) >= RUNS_GRACE,
            RunsFact::Unavailable {
                transient: false, ..
            } => true,
            _ => false,
        },
        _ => false,
    }
}

fn release_settled(o: &Observation, now: Timestamp) -> bool {
    let quiet = o
        .release_since
        .is_some_and(|at| since(at, now) >= QUIET_FOR);
    quiet
        || matches!(
            o.release,
            ReleaseFact::Unavailable {
                transient: false,
                ..
            }
        )
        || matches!(o.release, ReleaseFact::Published { assets, uploaded, .. } if assets == uploaded)
}

/// Whether the watch is over, and why.
fn decide(o: &Observation, published: Timestamp, now: Timestamp) -> Option<Ended> {
    if matches!(o.tag, TagFact::Moved { .. } | TagFact::Gone) {
        return Some(Ended::TagChanged);
    }
    if since(published, now) >= WATCH_FOR {
        return Some(Ended::Deadline);
    }
    (workflows_settled(o, published, now) && release_settled(o, now)).then_some(Ended::Settled)
}

/// When the launch's tag counts as published (for the watch's clock).
fn published_at(launch: &LaunchRow) -> Timestamp {
    launch
        .published_at
        .or(launch.pushed_at)
        .unwrap_or(launch.proposed_at)
}

/// One reading of a published `launch`, after `prev`, at `now`; `ended` says whether the watch is
/// over. Every read is metadata from a closed endpoint.
pub async fn observe_once(
    reads: &dyn ReleaseReads,
    launch: &LaunchRow,
    prev: &Observation,
    now: Timestamp,
) -> Observation {
    let Some(repo) = RepoRef::parse(&launch.repo) else {
        let reason = "the repository name is unreadable".to_owned();
        return Observation {
            tag: TagFact::Unavailable {
                reason: reason.clone(),
                transient: false,
            },
            release: ReleaseFact::Unavailable {
                reason: reason.clone(),
                transient: false,
            },
            workflows: WorkflowsFact::Unavailable {
                reason: reason.clone(),
                transient: false,
            },
            runs: RunsFact::Unavailable {
                reason,
                transient: false,
            },
            release_since: Some(now),
            read_at: Some(now),
            reads: prev.reads.saturating_add(1),
            last_error: None,
            ended: Some(Ended::Settled),
        };
    };
    let mut errors: Vec<String> = Vec::new();
    let mut note = |what: &str, e: &GithubError| {
        if e.is_transient() {
            errors.push(format!("{what}: {}", reason(e).0));
        }
    };

    let tag_read = meta::read_tag(reads, &repo, &launch.tag).await;
    let release_read = meta::read_release(reads, &repo, &launch.tag).await;
    let tag = match tag_read {
        Ok(Some(found)) => match &launch.remote_object {
            Some(ours) if found.sha.eq_ignore_ascii_case(ours) => TagFact::Published,
            None => TagFact::Published,
            Some(_) => TagFact::Moved {
                now: name(&found.sha),
            },
        },
        // GitHub answers 404 for a repository this login can't see too, so a missing tag is
        // "gone" only when the same reading saw the repository's releases.
        Ok(None) => match &release_read {
            Ok(_) => TagFact::Gone,
            Err(e) => {
                note("the tag", e);
                let (why, transient) = reason(e);
                TagFact::Unavailable {
                    reason: format!(
                        "GitHub shows no such tag, and couldn't show the repository's releases either: {why}"
                    ),
                    transient,
                }
            }
        },
        Err(e) => {
            note("the tag", &e);
            let (reason, transient) = reason(&e);
            TagFact::Unavailable { reason, transient }
        }
    };
    if let Err(e) = &release_read {
        note("the release", e);
    }
    let release = release_fact(release_read);
    let workflows_read = meta::read_workflows(reads, &repo).await;
    if let Err(e) = &workflows_read {
        note("the workflows", e);
    }
    let workflows = workflows_fact(workflows_read);
    let runs_read = meta::read_runs(reads, &repo, &launch.commit).await;
    if let Err(e) = &runs_read {
        note("the workflow runs", e);
    }
    let runs = runs_fact(runs_read, launch);

    let (tag, release) = (kept(&prev.tag, tag), kept(&prev.release, release));
    let (workflows, runs) = (kept(&prev.workflows, workflows), kept(&prev.runs, runs));
    let release_since = if release == prev.release {
        prev.release_since.or(Some(now))
    } else {
        Some(now)
    };
    let mut o = Observation {
        tag,
        release,
        workflows,
        runs,
        release_since,
        read_at: Some(now),
        reads: prev.reads.saturating_add(1),
        last_error: (!errors.is_empty()).then(|| errors.join("; ")),
        ended: None,
    };
    o.ended = decide(&o, published_at(launch), now);
    o
}

/// A read that failed for a passing reason keeps the earlier answer, if there was one (and it
/// wasn't a failure itself).
fn kept<T: Transient + Default + PartialEq + Clone>(prev: &T, now: T) -> T {
    if now.transient() == Some(true) && prev.transient().is_none() && *prev != T::default() {
        prev.clone()
    } else {
        now
    }
}

/// Whether a fact is an unavailable one, and if so whether it passes.
trait Transient {
    fn transient(&self) -> Option<bool>;
}

macro_rules! transient {
    ($($t:ty),*) => {$(
        impl Transient for $t {
            fn transient(&self) -> Option<bool> {
                match self {
                    Self::Unavailable { transient, .. } => Some(*transient),
                    _ => None,
                }
            }
        }
    )*};
}
transient!(TagFact, ReleaseFact, WorkflowsFact, RunsFact);

// ---------------------------------------------------------------------------------------------
// Words
// ---------------------------------------------------------------------------------------------

/// The tag, in words.
pub fn tag_words(launch: &LaunchRow, o: &Observation) -> String {
    let object = launch
        .remote_object
        .as_deref()
        .map_or_else(String::new, |o| format!(" at {}", short(o)));
    match &o.tag {
        TagFact::Unread | TagFact::Published => format!("published: GitHub shows it{object}"),
        TagFact::Moved { now } => format!(
            "GitHub now shows it at {}, not the app's tag{object}",
            short(now)
        ),
        TagFact::Gone => "GitHub no longer shows it: it was deleted after publication".to_owned(),
        TagFact::Unavailable { reason, .. } => {
            format!("published{object}; reading it again failed ({reason})")
        }
    }
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// The GitHub release, in words.
pub fn release_words(o: &Observation) -> String {
    match &o.release {
        ReleaseFact::Unread => "not read yet".to_owned(),
        ReleaseFact::Unavailable { reason, .. } => format!("unavailable: {reason}"),
        ReleaseFact::NoneYet => "none on GitHub for this tag".to_owned(),
        ReleaseFact::Draft { name } => format!(
            "a draft{}, not published: nothing is offered for download",
            quoted(name.as_deref())
        ),
        ReleaseFact::Published {
            name,
            prerelease,
            assets,
            uploaded,
            names,
        } => {
            let mut out = format!(
                "published{}{}",
                quoted(name.as_deref()),
                if *prerelease { ", a pre-release" } else { "" }
            );
            if *assets == 0 {
                out.push_str(" · no files attached");
            } else {
                let _ = write!(out, " · {uploaded} of {assets} files uploaded");
                if uploaded < assets {
                    let _ = write!(out, ", {} still uploading", assets - uploaded);
                }
                let _ = write!(out, " ({})", names.join(", "));
                if usize::try_from(*assets).unwrap_or(usize::MAX) > names.len() {
                    out.push_str(", …");
                }
            }
            out
        }
    }
}

fn quoted(name: Option<&str>) -> String {
    name.map_or_else(String::new, |n| format!(" \"{n}\""))
}

/// The workflows and their runs for the tag, in words.
pub fn workflows_words(launch: &LaunchRow, o: &Observation) -> String {
    let (for_tag, more, other) = match &o.runs {
        RunsFact::Seen {
            for_tag,
            more,
            other,
        } => (for_tag.as_slice(), *more, *other),
        _ => (&[][..], 0, 0),
    };
    let others = if other > 0 {
        format!(" · {other} other run(s) on this commit, not for the tag")
    } else {
        String::new()
    };
    if !for_tag.is_empty() {
        let done = for_tag.iter().filter(|r| r.status == "completed").count();
        let list: Vec<String> = for_tag
            .iter()
            .map(|r| {
                let state = match (&r.conclusion, r.status.as_str()) {
                    (Some(c), "completed") => c.clone(),
                    _ => r.status.replace('_', " "),
                };
                format!("{} ({}): {state}", r.name, r.event)
            })
            .collect();
        let more = if more > 0 {
            format!(", and {more} more")
        } else {
            String::new()
        };
        return format!(
            "{done} of {} run(s) for the tag completed: {}{more}{others}",
            for_tag.len(),
            list.join("; ")
        );
    }
    let runs_unavailable = match &o.runs {
        RunsFact::Unavailable { reason, .. } => Some(reason.as_str()),
        _ => None,
    };
    match &o.workflows {
        WorkflowsFact::Unread => "not read yet".to_owned(),
        WorkflowsFact::Unavailable { reason, .. } => match runs_unavailable {
            Some(_) => format!("unavailable: {reason}"),
            None => format!("unavailable: {reason}; no run for the tag seen{others}"),
        },
        WorkflowsFact::NotConfigured => "none set up in this repository".to_owned(),
        WorkflowsFact::AllDisabled { count } => {
            format!("all {count} workflow(s) in this repository are disabled{others}")
        }
        WorkflowsFact::Active { active, .. } => match runs_unavailable {
            Some(reason) => format!("{active} active workflow(s); runs unavailable: {reason}"),
            None if o
                .read_at
                .is_some_and(|at| since(published_at(launch), at) >= RUNS_GRACE) =>
            {
                format!(
                    "{active} active workflow(s), but none started for this tag within {} minutes of its publication{others}",
                    RUNS_GRACE.as_secs() / 60
                )
            }
            None => format!("{active} active workflow(s); none started for this tag yet{others}"),
        },
    }
}

/// The watch itself, in words.
pub fn watch_words(launch: &LaunchRow, o: &Observation) -> String {
    let read = o.read_at.map_or_else(
        || "not read yet".to_owned(),
        |at| format!("last read {}", at.strftime("%H:%M:%S UTC")),
    );
    let failing = o
        .last_error
        .as_deref()
        .map_or_else(String::new, |e| format!("; the last read failed ({e})"));
    match (launch.observe_state, o.ended) {
        (Some(ObserveState::Done), Some(Ended::Settled)) => {
            format!("ended: nothing more was expected ({read})")
        }
        (Some(ObserveState::Done), Some(Ended::TagChanged)) => {
            format!("ended: the tag changed on GitHub ({read})")
        }
        (Some(ObserveState::Done), _) => format!(
            "ended {} hours after publication ({read}{failing})",
            WATCH_FOR.as_secs() / 3600
        ),
        (Some(ObserveState::Observing), _) => {
            let until = published_at(launch)
                .checked_add(SignedDuration::try_from(WATCH_FOR).unwrap_or_default())
                .ok()
                .map_or_else(String::new, |t| {
                    format!(" until {} at the latest", t.strftime("%H:%M UTC"))
                });
            format!("watching{until} ({read}{failing})")
        }
        (None, _) => "not watched: published before the app watched releases".to_owned(),
    }
}

/// What the supervisor hears when a watch ends.
pub fn ended_note(launch: &LaunchRow, o: &Observation) -> String {
    let why = match o.ended {
        Some(Ended::Settled) | None => "nothing more was expected".to_owned(),
        Some(Ended::TagChanged) => "the tag changed on GitHub".to_owned(),
        Some(Ended::Deadline) => format!(
            "{} hours passed since its publication",
            WATCH_FOR.as_secs() / 3600
        ),
    };
    format!(
        "The app stopped watching tag {} at {} in {} ({why}). What GitHub showed last:\n\
         - Tag: {}\n- Release: {}\n- Workflows: {}\n\
         Report these as three separate facts; a release is available for download only if the \
         release line says published with its files uploaded.",
        launch.tag,
        launch.commit.short(),
        launch.repo,
        tag_words(launch, o),
        release_words(o),
        workflows_words(launch, o)
    )
}

// ---------------------------------------------------------------------------------------------
// Running the watch
// ---------------------------------------------------------------------------------------------

fn key(id: ReleaseLaunchId) -> String {
    format!("release:{id}")
}

/// The wait before the next reading.
fn next_delay(launch: &LaunchRow, now: Timestamp, rate_left: Option<u32>) -> Duration {
    if rate_left.is_some_and(|left| left < RATE_LOW) || since(published_at(launch), now) >= FAST_FOR
    {
        READ_SLOW
    } else {
        READ_EVERY
    }
}

/// Reads the launch's tag, release and runs after `delay`, replacing any reading already armed.
pub(crate) fn arm(svc: &DeliveryService, id: ReleaseLaunchId, delay: Duration) {
    let owned = svc.clone();
    svc.scheduler().after(
        key(id),
        delay,
        Box::new(move || Box::pin(async move { watch_once(&owned, id).await })),
    );
}

/// One reading of a watched launch; arms the next one, or ends the watch and wakes its supervisor.
async fn watch_once(svc: &DeliveryService, id: ReleaseLaunchId) {
    let launch = match svc.read(move |conn| releases::load(conn, id)).await {
        Ok(Some(l)) => l,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(launch = %id, error = ?e, "the release watch couldn't read its launch");
            arm(svc, id, READ_SLOW);
            return;
        }
    };
    if launch.status != S::Published || launch.observe_state != Some(ObserveState::Observing) {
        return;
    }
    let prev = Observation::of(&launch);
    let now = svc.clock().now();
    let reading = observe_once(svc.github(), &launch, &prev, now).await;
    let to = if reading.ended.is_some() {
        ObserveState::Done
    } else {
        ObserveState::Observing
    };
    let value = serde_json::to_value(&reading).unwrap_or_default();
    let timing = launch.clone();
    let stored = svc
        .tasks()
        .write(move |tx, notify| {
            let moved =
                releases::save_observation(tx, id, ObserveState::Observing, to, &value, now)?;
            if moved && to == ObserveState::Done {
                let row = releases::load(tx, id)?.unwrap_or(launch.clone());
                wake_ended(tx, notify, &row, &reading, now)?;
            }
            Ok(moved)
        })
        .await;
    match stored {
        Ok(true) if to == ObserveState::Observing => {
            arm(
                svc,
                id,
                next_delay(&timing, now, svc.github().rate().remaining),
            );
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(launch = %id, error = %e, "couldn't store a release reading");
            arm(svc, id, READ_SLOW);
        }
    }
}

/// The one `release_observed` wake at a watch's end.
fn wake_ended(
    tx: &Connection,
    notify: &mut Notify,
    launch: &LaunchRow,
    o: &Observation,
    at: Timestamp,
) -> Result<(), StoreError> {
    let (Some(target), Some(task)) = (launch.asked_by, launch.task) else {
        return Ok(());
    };
    let w = new_wake(
        target,
        WakeKind::ReleaseObserved,
        task,
        format!("release:{}:observed", launch.id),
        &ended_note(launch, o),
        &format!("tag {} watched", launch.tag),
        serde_json::json!({ "launch": launch.id, "observed": true }),
        at,
    );
    if insert_wake(tx, &w)?.is_some() {
        notify.add(target);
    }
    Ok(())
}

/// Starts the watch of a launch just recorded `published`, inside the transaction that recorded
/// it; the caller arms it with [`arm`] once that commits.
pub(crate) fn start(tx: &Connection, id: ReleaseLaunchId) -> Result<bool, StoreError> {
    releases::start_observing(tx, id)
}

/// Startup: arms every watch still running, and starts one for a launch published less than
/// [`WATCH_FOR`] ago whose watch never started (the app stopped first).
pub(crate) async fn recover(svc: &DeliveryService) -> Result<(), StoreError> {
    let now = svc.clock().now();
    let launches = svc
        .tasks()
        .db()
        .call(|conn| releases::to_observe(conn))
        .await?;
    for launch in launches {
        let id = launch.id;
        let armed = match launch.observe_state {
            Some(ObserveState::Observing) => true,
            None if since(published_at(&launch), now) < WATCH_FOR => {
                svc.tasks()
                    .db()
                    .call(move |conn| releases::start_observing(conn, id))
                    .await?
            }
            Some(ObserveState::Done) | None => false,
        };
        if armed {
            arm(svc, id, READ_EVERY);
        }
    }
    Ok(())
}

/// A launch for `release_status`: read again first when it is watched (or was) and its reading is
/// older than [`FRESH_FOR`]. A refresh never ends a watch nor wakes anyone.
async fn refreshed(svc: &DeliveryService, launch: LaunchRow) -> LaunchRow {
    let Some(state) = launch.observe_state else {
        return launch;
    };
    let now = svc.clock().now();
    if launch.status != S::Published
        || launch
            .observed_at
            .is_some_and(|at| since(at, now) < FRESH_FOR)
    {
        return launch;
    }
    let prev = Observation::of(&launch);
    let mut reading = observe_once(svc.github(), &launch, &prev, now).await;
    reading.ended = match state {
        ObserveState::Observing => None,
        ObserveState::Done => prev.ended,
    };
    let value = serde_json::to_value(&reading).unwrap_or_default();
    let id = launch.id;
    let stored = svc
        .tasks()
        .write(move |tx, _| {
            releases::save_observation(tx, id, state, state, &value, now)?;
            releases::load(tx, id)
        })
        .await;
    match stored {
        Ok(Some(now)) => now,
        Ok(None) => launch,
        Err(e) => {
            tracing::warn!(launch = %id, error = %e, "couldn't store a release reading");
            launch
        }
    }
}

/// `release_status`: every release tag launch of `task` (or only `tag`'s), with the tag, its
/// GitHub release and its workflow runs as separate facts.
pub(crate) async fn release_status(
    svc: &DeliveryService,
    caller: &Caller,
    task: TaskId,
    tag: Option<String>,
) -> Result<ReleaseStatus, ToolFailure> {
    let reader = *caller;
    let launches = svc
        .read(move |conn| {
            if task_store::load(conn, task)?.is_none() {
                return Ok(Err(Refusal::new(format!(
                    "{task} doesn't exist or was forgotten; use find_tasks for tasks still kept"
                ))));
            }
            if let Err(r) = may_read_task(conn, &reader, task)? {
                return Ok(Err(r));
            }
            Ok(Ok(releases::for_task(conn, task)?))
        })
        .await?
        .map_err(ToolFailure::Refused)?;
    let launches: Vec<LaunchRow> = launches
        .into_iter()
        .filter(|l| tag.as_deref().is_none_or(|t| l.tag == t))
        .collect();
    if launches.is_empty() {
        let which = tag.map_or_else(String::new, |t| format!(" for tag {t}"));
        return Err(Refusal::new(format!(
            "{task} has no release tag launch{which}; a crewmate proposes one with propose_release_tag"
        ))
        .into());
    }
    let mut out = Vec::with_capacity(launches.len());
    for launch in launches {
        let launch = refreshed(svc, launch).await;
        out.push(report(&launch));
    }
    Ok(ReleaseStatus::new(out))
}

/// One launch, in words.
pub(crate) fn report(launch: &LaunchRow) -> ReleaseLaunchReport {
    let mut r = ReleaseLaunchReport::new(
        launch.tag.clone(),
        launch.commit.as_str().to_owned(),
        launch.repo.clone(),
        launch.status,
    );
    if launch.status == S::Published {
        let o = Observation::of(launch);
        r.tag_status = tag_words(launch, &o);
        if launch.observe_state.is_some() {
            r.release = Some(release_words(&o));
            r.workflows = Some(workflows_words(launch, &o));
        }
        r.watch = Some(watch_words(launch, &o));
    } else {
        r.tag_status = super::release::status_note(launch);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use qm_github::release_meta::{MetaResponse, ReleaseEndpoint};
    use qm_store::releases::{NewLaunch, Outcome};
    use qm_types::BoxFuture;
    use qm_types::domain::TagKind;
    use qm_types::ids::{AgentId, CommitSha, ProjectId};
    use std::collections::HashMap;
    use std::sync::Mutex;

    const SHA: &str = "8a82a2ff738ee593deefddbb2686f7d40c26a204";
    const OBJECT: &str = "1111111111111111111111111111111111111111";

    /// GitHub, answering by path (404 for anything not set); records every path asked.
    #[derive(Default)]
    struct Fake {
        answers: HashMap<String, Option<MetaResponse>>,
        asked: Mutex<Vec<String>>,
    }

    impl Fake {
        fn with(mut self, endpoint: &ReleaseEndpoint, status: u16, body: &str) -> Self {
            self.answers.insert(
                endpoint.path(&repo()),
                Some(MetaResponse::new(status, body)),
            );
            self
        }

        /// GitHub doesn't answer `endpoint` (the network dropped).
        fn down(mut self, endpoint: &ReleaseEndpoint) -> Self {
            self.answers.insert(endpoint.path(&repo()), None);
            self
        }
    }

    impl ReleaseReads for Fake {
        fn read<'a>(
            &'a self,
            repo: &'a RepoRef,
            endpoint: &'a ReleaseEndpoint,
        ) -> BoxFuture<'a, Result<MetaResponse, GithubError>> {
            let path = endpoint.path(repo);
            self.asked.lock().expect("lock").push(path.clone());
            let answer = match self.answers.get(&path) {
                Some(Some(r)) => Ok(r.clone()),
                Some(None) => Err(GithubError::Network("connection reset".into())),
                None => Ok(MetaResponse::new(404, r#"{"message":"Not Found"}"#)),
            };
            Box::pin(async move { answer })
        }
    }

    fn repo() -> RepoRef {
        RepoRef::parse("octo/widget").expect("repo")
    }

    fn at(s: i64) -> Timestamp {
        Timestamp::from_second(1_700_000_000 + s).expect("time")
    }

    fn minutes(m: i64) -> Timestamp {
        at(m * 60)
    }

    /// A launch of `v1.0.0` published at `at(0)`, read from a throwaway database.
    fn launch() -> LaunchRow {
        let mut conn = Connection::open_in_memory().expect("memory");
        qm_store::schema::migrate(&mut conn).expect("schema");
        conn.execute_batch("PRAGMA foreign_keys = OFF;")
            .expect("pragma");
        let id = releases::insert_launch(
            &conn,
            &NewLaunch {
                task: TaskId::new(42).expect("task"),
                project: ProjectId::new(1),
                asked_by: Some(AgentId::new(1)),
                repo: "octo/widget".into(),
                repo_id: 99,
                tag: "v1.0.0".into(),
                commit: SHA.parse::<CommitSha>().expect("sha"),
                branch: Some("main".into()),
                kind: TagKind::Annotated,
                message: None,
                at: at(-60),
            },
        )
        .expect("launch");
        conn.execute(
            "UPDATE release_launches SET status = 'pushing', pushed_at = ?2 WHERE id = ?1",
            rusqlite::params![id, at(-1).as_millisecond()],
        )
        .expect("push");
        assert!(
            releases::set_outcome(
                &conn,
                id,
                &[S::Pushing],
                &Outcome {
                    status: S::Published,
                    remote_object: Some(OBJECT),
                    words: "GitHub shows the tag",
                    at: at(0),
                },
            )
            .expect("publish")
        );
        assert!(start(&conn, id).expect("start"));
        releases::load(&conn, id).expect("load").expect("row")
    }

    fn tag_ref() -> ReleaseEndpoint {
        ReleaseEndpoint::TagRef {
            tag: "v1.0.0".into(),
        }
    }

    fn by_tag() -> ReleaseEndpoint {
        ReleaseEndpoint::ReleaseByTag {
            tag: "v1.0.0".into(),
        }
    }

    fn runs() -> ReleaseEndpoint {
        ReleaseEndpoint::Runs {
            head_sha: SHA.parse().expect("sha"),
        }
    }

    fn tag_body(sha: &str) -> String {
        format!(
            r#"{{"ref":"refs/tags/v1.0.0","object":{{"type":"tag","sha":"{sha}","url":"https://api.github.com/x"}}}}"#
        )
    }

    fn release_body(draft: bool, assets: &[(&str, &str)]) -> String {
        let assets: Vec<String> = assets
            .iter()
            .map(|(name, state)| {
                format!(
                    r#"{{"name":"{name}","size":10,"state":"{state}","browser_download_url":"https://github.com/octo/widget/releases/download/v1.0.0/{name}"}}"#
                )
            })
            .collect();
        format!(
            r#"{{"tag_name":"v1.0.0","name":"Widget 1.0","draft":{draft},"prerelease":false,"zipball_url":"https://api.github.com/z","body":"notes","assets":[{}]}}"#,
            assets.join(",")
        )
    }

    fn workflows_body(states: &[&str]) -> String {
        let list: Vec<String> = states
            .iter()
            .map(|s| format!(r#"{{"name":"Release","state":"{s}"}}"#))
            .collect();
        format!(
            r#"{{"total_count":{},"workflows":[{}]}}"#,
            states.len(),
            list.join(",")
        )
    }

    fn run(name: &str, branch: &str, status: &str, conclusion: Option<&str>) -> String {
        let conclusion = conclusion.map_or_else(|| "null".to_owned(), |c| format!("\"{c}\""));
        format!(
            r#"{{"name":"{name}","event":"push","status":"{status}","conclusion":{conclusion},"head_branch":"{branch}","head_sha":"{SHA}","logs_url":"https://api.github.com/l"}}"#
        )
    }

    fn runs_body(runs: &[String]) -> String {
        format!(
            r#"{{"total_count":{},"workflow_runs":[{}]}}"#,
            runs.len(),
            runs.join(",")
        )
    }

    fn read(fake: &Fake, prev: &Observation, now: Timestamp) -> Observation {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(observe_once(fake, &launch(), prev, now))
    }

    fn words(o: &Observation) -> (String, String, String) {
        let l = launch();
        (tag_words(&l, o), release_words(o), workflows_words(&l, o))
    }

    #[test]
    fn a_finished_release_settles_with_three_separate_facts_and_no_urls() {
        let fake = Fake::default()
            .with(&tag_ref(), 200, &tag_body(OBJECT))
            .with(
                &by_tag(),
                200,
                &release_body(
                    false,
                    &[("widget.tar.gz", "uploaded"), ("SHA256SUMS", "uploaded")],
                ),
            )
            .with(
                &ReleaseEndpoint::Workflows,
                200,
                &workflows_body(&["active"]),
            )
            .with(
                &runs(),
                200,
                &runs_body(&[
                    run("Release", "v1.0.0", "completed", Some("success")),
                    run("CI", "main", "in_progress", None),
                ]),
            );
        let o = read(&fake, &Observation::default(), minutes(5));
        assert_eq!(o.tag, TagFact::Published);
        assert!(matches!(
            o.release,
            ReleaseFact::Published {
                assets: 2,
                uploaded: 2,
                ..
            }
        ));
        assert_eq!(
            o.ended,
            Some(Ended::Settled),
            "the branch run isn't the tag's"
        );
        let (tag, release, workflows) = words(&o);
        assert!(
            tag.starts_with("published: GitHub shows it at 111111111111"),
            "{tag}"
        );
        assert_eq!(
            release,
            "published \"Widget 1.0\" · 2 of 2 files uploaded (widget.tar.gz, SHA256SUMS)"
        );
        assert_eq!(
            workflows,
            "1 of 1 run(s) for the tag completed: Release (push): success · 1 other run(s) on this commit, not for the tag"
        );
        let stored = serde_json::to_string(&o).expect("json");
        for never in ["http", "download", "zipball", "notes", "logs"] {
            assert!(!stored.contains(never), "{never} in {stored}");
        }
        let asked = fake.asked.lock().expect("lock").clone();
        assert_eq!(asked.len(), 4, "{asked:?}");
        assert!(
            asked.iter().all(|p| p.starts_with("/repos/octo/widget/")),
            "{asked:?}"
        );
    }

    #[test]
    fn a_refused_read_is_unavailable_never_absent() {
        let refused = r#"{"message":"Resource not accessible by integration"}"#;
        let fake = Fake::default()
            .with(&tag_ref(), 200, &tag_body(OBJECT))
            .with(&by_tag(), 403, refused)
            .with(&ReleaseEndpoint::Workflows, 403, refused)
            .with(&runs(), 403, refused);
        let o = read(&fake, &Observation::default(), minutes(1));
        assert!(matches!(
            o.release,
            ReleaseFact::Unavailable {
                transient: false,
                ..
            }
        ));
        assert!(matches!(
            o.workflows,
            WorkflowsFact::Unavailable {
                transient: false,
                ..
            }
        ));
        let (_, release, workflows) = words(&o);
        assert!(release.starts_with("unavailable: "), "{release}");
        assert!(release.contains("Resource not accessible"), "{release}");
        assert!(workflows.starts_with("unavailable: "), "{workflows}");
        assert!(!workflows.contains("none set up"), "{workflows}");
        // Nothing more can be learnt: the watch ends with that said, not with "no release".
        assert_eq!(o.ended, Some(Ended::Settled));
    }

    #[test]
    fn no_workflow_is_said_only_when_github_counts_none() {
        let base = || {
            Fake::default()
                .with(&tag_ref(), 200, &tag_body(OBJECT))
                .with(&ReleaseEndpoint::Releases, 200, "[]")
                .with(&runs(), 200, &runs_body(&[]))
        };
        // Workflows set up, no run yet: never "none set up", and the watch goes on.
        let fake = base().with(
            &ReleaseEndpoint::Workflows,
            200,
            &workflows_body(&["active"]),
        );
        let o = read(&fake, &Observation::default(), minutes(2));
        let (_, release, workflows) = words(&o);
        assert_eq!(release, "none on GitHub for this tag");
        assert_eq!(
            workflows,
            "1 active workflow(s); none started for this tag yet"
        );
        assert_eq!(o.ended, None);
        // Past the grace, and the release unchanged for ten minutes: it ends, still not "none set up".
        let later = read(&fake, &o, minutes(12));
        let (_, _, workflows) = words(&later);
        assert!(
            workflows.contains("none started for this tag within 10 minutes"),
            "{workflows}"
        );
        assert_eq!(later.ended, Some(Ended::Settled));

        let none = base().with(
            &ReleaseEndpoint::Workflows,
            200,
            r#"{"total_count":0,"workflows":[]}"#,
        );
        let o = read(&none, &Observation::default(), minutes(1));
        assert_eq!(o.workflows, WorkflowsFact::NotConfigured);
        assert_eq!(words(&o).2, "none set up in this repository");
        assert_eq!(o.ended, None, "the release may still appear");
        let quiet = read(&none, &o, minutes(12));
        assert_eq!(quiet.ended, Some(Ended::Settled));

        let disabled = base().with(
            &ReleaseEndpoint::Workflows,
            200,
            &workflows_body(&["disabled_manually"]),
        );
        let o = read(&disabled, &Observation::default(), minutes(1));
        assert_eq!(o.workflows, WorkflowsFact::AllDisabled { count: 1 });
        assert_eq!(
            words(&o).2,
            "all 1 workflow(s) in this repository are disabled"
        );
    }

    #[test]
    fn a_draft_or_an_upload_in_progress_is_not_available() {
        let base = || {
            Fake::default()
                .with(&tag_ref(), 200, &tag_body(OBJECT))
                .with(
                    &ReleaseEndpoint::Workflows,
                    200,
                    &workflows_body(&["active"]),
                )
                .with(
                    &runs(),
                    200,
                    &runs_body(&[run("Release", "v1.0.0", "in_progress", None)]),
                )
        };
        let draft = base().with(
            &ReleaseEndpoint::Releases,
            200,
            &format!("[{}]", release_body(true, &[])),
        );
        let o = read(&draft, &Observation::default(), minutes(3));
        let (_, release, workflows) = words(&o);
        assert_eq!(
            release,
            "a draft \"Widget 1.0\", not published: nothing is offered for download"
        );
        assert_eq!(
            workflows,
            "0 of 1 run(s) for the tag completed: Release (push): in progress"
        );
        assert_eq!(o.ended, None);

        let uploading = base().with(
            &by_tag(),
            200,
            &release_body(false, &[("a.zip", "uploaded"), ("b.zip", "open")]),
        );
        let o = read(&uploading, &Observation::default(), minutes(3));
        assert!(
            words(&o)
                .1
                .contains("1 of 2 files uploaded, 1 still uploading"),
            "{}",
            words(&o).1
        );
        assert_eq!(o.ended, None);
    }

    #[test]
    fn a_passing_failure_keeps_the_last_answer_and_says_so() {
        let good = Fake::default()
            .with(&tag_ref(), 200, &tag_body(OBJECT))
            .with(&by_tag(), 200, &release_body(false, &[("a.zip", "open")]))
            .with(
                &ReleaseEndpoint::Workflows,
                200,
                &workflows_body(&["active"]),
            )
            .with(
                &runs(),
                200,
                &runs_body(&[run("Release", "v1.0.0", "in_progress", None)]),
            );
        let first = read(&good, &Observation::default(), minutes(1));
        let flaky = Fake::default()
            .with(&tag_ref(), 200, &tag_body(OBJECT))
            .down(&by_tag())
            .with(
                &ReleaseEndpoint::Workflows,
                502,
                r#"{"message":"Bad gateway"}"#,
            )
            .down(&runs());
        let second = read(&flaky, &first, minutes(2));
        assert_eq!(second.release, first.release);
        assert_eq!(second.workflows, first.workflows);
        assert_eq!(second.runs, first.runs);
        assert_eq!(second.release_since, first.release_since);
        assert_eq!(second.reads, 2);
        let error = second.last_error.clone().expect("said");
        assert!(error.contains("the release"), "{error}");
        assert!(watch_words(&launch(), &second).contains("the last read failed"));
        // With nothing read before, a passing failure is shown as unavailable.
        let fresh = read(&flaky, &Observation::default(), minutes(2));
        assert!(matches!(
            fresh.release,
            ReleaseFact::Unavailable {
                transient: true,
                ..
            }
        ));
        assert_eq!(fresh.ended, None, "a passing failure never ends the watch");
    }

    #[test]
    fn a_changed_tag_or_the_deadline_ends_the_watch() {
        let moved = Fake::default().with(&tag_ref(), 200, &tag_body(&"2".repeat(40)));
        let o = read(&moved, &Observation::default(), minutes(1));
        assert_eq!(o.ended, Some(Ended::TagChanged));
        assert!(words(&o).0.contains("not the app's tag"));
        let gone = Fake::default().with(&ReleaseEndpoint::Releases, 200, "[]");
        let o = read(&gone, &Observation::default(), minutes(1));
        assert_eq!(o.tag, TagFact::Gone);
        assert_eq!(o.ended, Some(Ended::TagChanged));
        // Every read 404: the login may not see the repository, so the tag isn't called gone.
        let hidden = read(&Fake::default(), &Observation::default(), minutes(1));
        assert!(
            matches!(
                hidden.tag,
                TagFact::Unavailable {
                    transient: false,
                    ..
                }
            ),
            "{:?}",
            hidden.tag
        );
        assert!(
            !words(&hidden).0.contains("deleted"),
            "{}",
            words(&hidden).0
        );
        assert_eq!(hidden.ended, Some(Ended::Settled));

        let running = Fake::default()
            .with(&tag_ref(), 200, &tag_body(OBJECT))
            .with(&ReleaseEndpoint::Releases, 200, "[]")
            .with(
                &ReleaseEndpoint::Workflows,
                200,
                &workflows_body(&["active"]),
            )
            .with(
                &runs(),
                200,
                &runs_body(&[run("Release", "v1.0.0", "queued", None)]),
            );
        assert_eq!(
            read(&running, &Observation::default(), minutes(179)).ended,
            None
        );
        let o = read(&running, &Observation::default(), minutes(180));
        assert_eq!(o.ended, Some(Ended::Deadline));
        let mut done = launch();
        done.observe_state = Some(ObserveState::Done);
        let note = ended_note(&done, &o);
        assert!(note.contains("3 hours passed"), "{note}");
        assert!(
            note.contains("- Release: none on GitHub for this tag"),
            "{note}"
        );
        assert!(note.contains("three separate facts"), "{note}");
    }

    #[test]
    fn the_pace_slows_after_fifteen_minutes_or_on_a_low_rate_limit() {
        let l = launch();
        assert_eq!(next_delay(&l, minutes(1), None), READ_EVERY);
        assert_eq!(next_delay(&l, minutes(1), Some(RATE_LOW - 1)), READ_SLOW);
        assert_eq!(next_delay(&l, minutes(15), None), READ_SLOW);
    }

    #[test]
    fn a_reading_survives_the_database_round_trip() {
        let fake = Fake::default()
            .with(&tag_ref(), 200, &tag_body(OBJECT))
            .with(
                &ReleaseEndpoint::Workflows,
                200,
                &workflows_body(&["active"]),
            );
        let o = read(&fake, &Observation::default(), minutes(1));
        let mut l = launch();
        l.observation = Some(serde_json::to_value(&o).expect("json"));
        assert_eq!(Observation::of(&l), o);
        // An unreadable reading counts as none, never as an error.
        l.observation = Some(serde_json::json!({ "release": { "state": "teleported" } }));
        assert_eq!(Observation::of(&l), Observation::default());
        let r = report(&l);
        assert_eq!(r.status, S::Published);
        assert_eq!(r.release.as_deref(), Some("not read yet"));
        assert!(r.watch.expect("watch").starts_with("watching until"));
    }
}
