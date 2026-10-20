//! Routing a request into a secondmate's scope.
//!
//! The app stamps the marker and a **fresh correlation ID**: a new `secondmate_requests` row whose
//! ID is the correlation, never supplied by a model. The row carries your words (copied from your
//! message on a turn you started; empty on an app-event turn), the Quartermaster's one-time
//! instructions, and a base branch and overrides only from your turn (the handler refused them on
//! any other). One `routed` wake (`routed:C-0193`) wakes the secondmate once.

use super::{SecondmateService, event_kind, failed, refuse};
use crate::inbox::{Inbox, Notify};
use qm_git::{BaseRequest, GitError, Repo};
use qm_policy::scope::check_own_secondmate;
use qm_store::events::{self, NewEvent};
use qm_store::messages;
use qm_store::projects::{self, ProjectRow};
use qm_store::secondmates::{self, NewRequest, RequestOverrides, StoredLaterStep};
use qm_store::wakes::NewWake;
use qm_tools::defs::Routed;
use qm_tools::handler::{Caller, RouteRequest, ToolFailure};
use qm_types::domain::{StartedBy, WakeKind};
use qm_types::ids::CorrelationId;
use std::fmt::Write as _;
use std::time::Duration;

/// How long checking a routed base branch against the remote may take (as for a task's base).
pub const BRANCH_CHECK_TIMEOUT: Duration = Duration::from_mins(3);

/// How much of the request the event's summary shows.
const SUMMARY_CHARS: usize = 80;

/// The routed wake's text; lines are left out when empty. Free text is neutralised when the wake is
/// rendered.
pub fn wake_text(
    scope: &str,
    user_words: &str,
    request: &str,
    instructions: &[String],
    base_branch: Option<&str>,
    correlation: CorrelationId,
) -> String {
    let mut text = format!("Request in your scope ({scope}).");
    if !user_words.trim().is_empty() {
        let _ = write!(text, " The user's words: \"{}\".", user_words.trim());
    }
    let _ = write!(text, "\nRequest: {request}");
    if !instructions.is_empty() {
        text.push_str("\nInstructions for this request only:");
        for i in instructions {
            let _ = write!(text, "\n- {i}");
        }
    }
    if let Some(b) = base_branch {
        let _ = write!(text, "\nBase branch for this request: {b}.");
    }
    let _ = write!(
        text,
        "\nReply on the parent channel with corr {correlation}."
    );
    text
}

fn cut(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let mut s: String = one_line.chars().take(max.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}

impl SecondmateService {
    /// `route_to_secondmate`.
    #[allow(clippy::too_many_lines)]
    pub async fn route(&self, caller: &Caller, req: RouteRequest) -> Result<Routed, ToolFailure> {
        let id = req.secondmate;
        let Some(turn) = caller.turn else {
            return Err(refuse(
                "routing a request needs a turn of the Quartermaster's",
            ));
        };
        let message = match turn.started_by {
            StartedBy::User { message } => Some(message),
            StartedBy::AppEvent => None,
        };
        let caller_agent = caller.agent;
        let (record, user_words, single) = self
            .db()
            .call(move |conn| {
                let Some(record) = secondmates::load(conn, id)? else {
                    return Ok(Err(refuse(format!("There's no secondmate {id}"))));
                };
                if let Err(r) = check_own_secondmate(caller_agent, id, record.parent) {
                    return Ok(Err(r.into()));
                }
                // Your words, already neutralised when stored.
                let words = match message {
                    Some(m) => messages::text(conn, m)?.unwrap_or_default(),
                    None => String::new(),
                };
                let single = match record.projects.as_slice() {
                    [(_, name)] => projects::active_by_name(conn, name)?,
                    _ => None,
                };
                Ok(Ok((record, words, single)))
            })
            .await
            .map_err(|e| failed(&e))??;

        // A typed branch is checked against the remote where the project is known.
        if let (Some(branch), Some(project)) = (&req.base_branch, &single) {
            self.check_branch(project, branch).await?;
        }

        let overrides = RequestOverrides {
            model: req.model.clone(),
            effort: req.effort,
            premium: req.premium,
            later_steps: req
                .later_steps
                .iter()
                .map(|s| StoredLaterStep {
                    kind: s.kind,
                    model: s.model.clone(),
                    effort: s.effort,
                })
                .collect(),
        };
        let at = self.clock().now();
        let scope = record.scope.clone();
        let target = record.agent;
        let request_text = req.request.clone();
        let (correlation, notify) = self
            .db()
            .call(move |conn| {
                let tx = conn.transaction()?;
                let correlation = secondmates::insert_request(
                    &tx,
                    &NewRequest {
                        secondmate: id,
                        turn: turn.turn,
                        request: &req.request,
                        user_words: &user_words,
                        instructions: &req.instructions,
                        base_branch: req.base_branch.as_deref(),
                        overrides: &overrides,
                        at,
                    },
                )?;
                events::insert(
                    &tx,
                    &NewEvent {
                        kind: event_kind::ROUTED,
                        agent: Some(caller_agent),
                        task: None,
                        turn: Some(turn.turn),
                        summary: &format!(
                            "Routed {correlation} to {id}: {}",
                            cut(&request_text, SUMMARY_CHARS)
                        ),
                        payload: serde_json::json!({
                            "secondmate": id,
                            "correlation": correlation,
                            "base_branch": req.base_branch,
                        }),
                        at,
                    },
                )?;
                let mut notify = Notify::new();
                Inbox::post(
                    &tx,
                    &NewWake {
                        target,
                        kind: WakeKind::Routed,
                        payload: serde_json::json!({
                            "text": wake_text(
                                &scope,
                                &user_words,
                                &req.request,
                                &req.instructions,
                                req.base_branch.as_deref(),
                                correlation,
                            ),
                            "short": format!("{correlation} from the Quartermaster"),
                            "correlation": correlation.to_string(),
                        }),
                        dedupe_key: format!("routed:{correlation}"),
                        task: None,
                        at,
                    },
                    &mut notify,
                )?;
                tx.commit()?;
                Ok((correlation, notify))
            })
            .await
            .map_err(|e| failed(&e))?;
        self.deps().inbox.notify(&notify);
        Ok(Routed { correlation })
    }

    /// Whether `branch` can be a base on `project`'s remote (fetched; at most
    /// [`BRANCH_CHECK_TIMEOUT`]), refused with the reason a task start would give.
    async fn check_branch(&self, project: &ProjectRow, branch: &str) -> Result<(), ToolFailure> {
        let git = self.git().clone();
        let path = project.path.clone();
        let delivery = project.delivery;
        let default_branch = project.default_branch.clone();
        let request = BaseRequest::Typed {
            branch: branch.to_owned(),
            traced_to_user: true,
        };
        let work = tokio::task::spawn_blocking(move || {
            Repo::open(git, &path)?.settle_base(&request, delivery, &default_branch)
        });
        let clock = std::sync::Arc::clone(&self.deps().clock);
        tokio::select! {
            joined = work => match joined {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(GitError::BaseRefused(r))) => Err(refuse(format!(
                    "base branch {branch} can't be used for {}: {r}",
                    project.name
                ))),
                Ok(Err(e)) => Err(failed(&e)),
                Err(e) => Err(failed(&e)),
            },
            () = clock.sleep(BRANCH_CHECK_TIMEOUT) => Err(refuse(
                "the base branch couldn't be checked: fetching origin took longer than 3 minutes",
            )),
        }
    }
}
