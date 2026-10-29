//! The Actions inbox: every request waiting on you, rebuilt for each snapshot from the
//! authoritative pending states. Nothing here is stored, so an item can never outlive its request.
//!
//! Each source is read directly, never scraped from `snapshot.conversation`, so a request older
//! than the conversation's window is still listed. This is the only place cards reach the UI:
//! the chat carries none.

use crate::api::{ApiPart, Handled, MODEL_SETUP_NOTICE, notice_card, question_card};
use crate::held::HeldService;
use crate::plans::{Stage, api as plan_api};
use crate::tasks::TaskService;
use qm_plans::PAGE_FILE;
use qm_store::db::Db;
use qm_store::{agents, events, messages, secondmates, tasks as task_store};
use qm_types::BoxFuture;
use qm_types::api::{
    ActionItem, ActionKey, ActionKind, ActionOpen, AgentRef, Card, CardAction, CardButton,
    CardKind, CardLine, Command, Snapshot, Styled, Tone,
};
use qm_types::clock::Clock;
use qm_types::data_dir::DataDir;
use qm_types::domain::{PlanMode, Role, TaskKind, TaskStatus};
use qm_types::ids::{AgentId, TaskId};
use rusqlite::{Connection, OptionalExtension};
use std::sync::Arc;

/// The inbox's part. It handles no command: every button maps to one the other parts take.
#[derive(Clone, Debug)]
pub struct ActionsPart {
    held: HeldService,
    db: Db,
    clock: Arc<dyn Clock>,
    data_dir: DataDir,
    /// The Quartermaster's agent row.
    seat_agent: AgentId,
    /// For the plans waiting on you.
    tasks: TaskService,
}

impl ActionsPart {
    pub fn new(
        held: HeldService,
        (db, clock, data_dir): (Db, Arc<dyn Clock>, DataDir),
        seat_agent: AgentId,
        tasks: TaskService,
    ) -> Self {
        Self {
            held,
            db,
            clock,
            data_dir,
            seat_agent,
            tasks,
        }
    }

    async fn build(&self, snapshot: &Snapshot) -> Vec<ActionItem> {
        let now = self.clock.now();
        let config = configuration(snapshot, now);
        let mut items = Vec::new();

        match self.held.pending_cards().await {
            Ok(rows) => {
                let seat = self.seat_agent;
                let asked: Vec<(Option<AgentId>, Option<TaskId>)> =
                    rows.iter().map(|(r, _)| (r.asked_by, r.task)).collect();
                let refs = self
                    .db
                    .call(move |conn| {
                        asked
                            .into_iter()
                            .map(|(a, t)| match a {
                                Some(a) => agent_ref(conn, seat, a, t),
                                None => Ok(t.map(|task| AgentRef::Crewmate { task })),
                            })
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .await
                    .unwrap_or_default();
                for (i, (row, card)) in rows.into_iter().enumerate() {
                    let kind = match card.kind {
                        CardKind::Decision => ActionKind::Decision,
                        _ => ActionKind::Approval,
                    };
                    let mut item = ActionItem::new(
                        ActionKey::Held { id: row.id },
                        kind,
                        card.title.clone().unwrap_or_else(|| card.heading.clone()),
                        row.created_at,
                        card,
                        ActionOpen::Conversation,
                    );
                    if let Some(Some(from)) = refs.get(i) {
                        item = item.with_from(*from);
                    }
                    if let Some(task) = row.task {
                        item = item.with_task(task);
                    }
                    items.push(item);
                }
            }
            Err(e) => tracing::error!(error = %e, "couldn't read the held actions for the inbox"),
        }

        items.extend(self.plans().await);
        items.extend(self.reviews().await);
        items.extend(self.reports().await);
        items.extend(self.questions_and_notices().await);

        // Read retained records, not the live crew snapshot: unread reports and held actions
        // can outlive their worker and still need a friendly source label.
        let sources: Vec<_> = items.iter().map(|item| (item.from, item.task)).collect();
        match self
            .db
            .call(move |conn| {
                sources
                    .into_iter()
                    .map(|(from, task)| source_label(conn, from, task))
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
        {
            Ok(labels) => {
                for (item, label) in items.iter_mut().zip(labels) {
                    item.from_label = label;
                }
            }
            Err(e) => tracing::error!(error = %e, "couldn't read inbox source names"),
        }
        order(config, items)
    }

    /// Plans open for you: an interactive plan whose round waits on your reply or approval.
    async fn plans(&self) -> Vec<ActionItem> {
        let plans: Vec<_> = plan_api::live(&self.data_dir)
            .into_iter()
            .filter(|(_, s, _)| {
                matches!(s.stage, Stage::WithUser | Stage::Final)
                    && s.locked.is_none()
                    && !s.pending_reply
            })
            .collect();
        let mut out = Vec::new();
        for (task, state, dir) in plans {
            let Ok(Some((row, project))) = self
                .db
                .call(move |conn| {
                    let Some(row) = task_store::load(conn, task)? else {
                        return Ok(None);
                    };
                    let project = crate::tasks::load_project(conn, row.project)?
                        .map(|p| p.name)
                        .unwrap_or_default();
                    Ok(Some((row, project)))
                })
                .await
            else {
                continue;
            };
            if row.kind != TaskKind::Plan || row.closed_at.is_some() {
                continue;
            }
            if !matches!(
                self.tasks.live_plan_mode(&row).await,
                Ok(Some(PlanMode::Interactive))
            ) {
                continue;
            }
            let card = plan_api::card(task, &row.title, &project, &state, &dir.join(PAGE_FILE));
            let title = format!("{} · {}", card.heading, row.title);
            let at = row.reported_at.or(row.started_at).unwrap_or(row.created_at);
            let item = ActionItem::new(
                ActionKey::Plan { task },
                ActionKind::Plan,
                title,
                at,
                card,
                ActionOpen::Plan { task },
            )
            .with_task(task)
            .with_from(AgentRef::Crewmate { task });
            out.push(item);
        }
        out
    }

    /// Reviews whose report waits on your go-ahead (normal autonomy).
    async fn reviews(&self) -> Vec<ActionItem> {
        let read = self
            .db
            .call(|conn| {
                let mut out = Vec::new();
                for row in task_store::with_status(conn, TaskStatus::Waiting)? {
                    if !crate::tasks::review::awaits_go_ahead(&row) {
                        continue;
                    }
                    let project = crate::tasks::load_project(conn, row.project)?
                        .map(|p| p.name)
                        .unwrap_or_default();
                    let report = qm_store::reports::latest_clear(conn, row.id)?;
                    let who = review_supervisor(conn, row.supervisor)?;
                    out.push((row, project, report, who));
                }
                Ok(out)
            })
            .await;
        let rows = match read {
            Ok(rows) => rows,
            Err(e) => {
                tracing::error!(error = %e, "couldn't read the reviews waiting on you");
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for (row, project, report, who) in rows {
            let task = row.id;
            let card =
                crate::reports::api::review_card(task, &row.title, &project, report.as_ref(), &who);
            let title = format!("{} · {}", card.heading, row.title);
            let at = report
                .as_ref()
                .map(|r| r.at)
                .or(row.started_at)
                .unwrap_or(row.created_at);
            out.push(
                ActionItem::new(
                    ActionKey::Review { task },
                    ActionKind::Review,
                    title,
                    at,
                    card,
                    ActionOpen::Session {
                        agent: AgentRef::Crewmate { task },
                    },
                )
                .with_task(task)
                .with_from(AgentRef::Crewmate { task }),
            );
        }
        out
    }

    /// Finished answer tasks' reports you haven't dismissed, while their page is there.
    async fn reports(&self) -> Vec<ActionItem> {
        let kinds = crate::reports::api::card_kinds();
        let read = self
            .db
            .call(move |conn| {
                let mut out = Vec::new();
                for row in task_store::unread_reports(conn, &kinds)? {
                    if !crate::reports::api::has_report_card(&row) {
                        continue;
                    }
                    let project = crate::tasks::load_project(conn, row.project)?
                        .map(|p| p.name)
                        .unwrap_or_default();
                    let report = qm_store::reports::latest_clear(conn, row.id)?;
                    out.push((row, project, report));
                }
                Ok(out)
            })
            .await;
        let rows = match read {
            Ok(rows) => rows,
            Err(e) => {
                tracing::error!(error = %e, "couldn't read the reports waiting on you");
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for (row, project, report) in rows {
            if crate::reports::api::report_page(&self.tasks.paths_of(&row)).is_err() {
                continue;
            }
            let task = row.id;
            let card = crate::reports::api::report_card(
                task,
                row.kind,
                &row.title,
                &project,
                report.as_ref(),
            );
            let title = format!("{} · {}", card.heading, row.title);
            let at = row.reported_at.unwrap_or(row.created_at);
            // Go to its session while the task is open; a closed one has nobody left to talk to.
            let open = if row.status == TaskStatus::Closed {
                ActionOpen::Conversation
            } else {
                ActionOpen::Session {
                    agent: AgentRef::Crewmate { task },
                }
            };
            out.push(
                ActionItem::new(
                    ActionKey::Report { task },
                    ActionKind::Report,
                    title,
                    at,
                    card,
                    open,
                )
                .with_task(task)
                .with_from(AgentRef::Crewmate { task }),
            );
        }
        out
    }

    /// Questions from the Quartermaster and live secondmates, and the seat's unanswered notices.
    async fn questions_and_notices(&self) -> Vec<ActionItem> {
        let seat = self.seat_agent;
        let read = self.db.call(move |conn| open_requests(conn, seat)).await;
        let (questions, notices, mates) = match read {
            Ok(read) => read,
            Err(e) => {
                tracing::error!(error = %e, "couldn't read the questions for the inbox");
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for row in questions {
            let (from, open) = match mates.iter().find(|(a, _)| *a == row.agent) {
                Some((_, id)) => {
                    let agent = AgentRef::Secondmate { id: *id };
                    (agent, ActionOpen::Session { agent })
                }
                None => (AgentRef::Quartermaster, ActionOpen::Conversation),
            };
            let mut card = if row.options.is_empty() {
                let mut card = Card::new(CardKind::Question, "Question");
                card.buttons = vec![CardButton::new(
                    "Reply…",
                    'r',
                    CardAction::OpenChat { to: from },
                )];
                card
            } else {
                question_card(&row.options, None)
            };
            // The overlay shows no chat line above the card, so the card carries the question.
            card.lines.insert(
                0,
                CardLine::new(2, vec![Styled::new(row.text.clone(), Tone::Text)]),
            );
            let mut item = ActionItem::new(
                ActionKey::Question { message: row.id },
                ActionKind::Question,
                first_line(&row.text),
                row.at,
                card,
                open,
            )
            .with_from(from);
            if from == AgentRef::Quartermaster {
                item = item.with_anchor(row.id);
            }
            out.push(item);
        }
        for (row, state) in notices {
            let card = notice_card(&state, &row.text);
            let title = format!("{}: {}", card.heading, first_line(&row.text));
            out.push(
                ActionItem::new(
                    ActionKey::Notice { message: row.id },
                    ActionKind::Notice,
                    title,
                    row.at,
                    card,
                    ActionOpen::Conversation,
                )
                .with_from(AgentRef::Quartermaster)
                .with_anchor(row.id),
            );
        }
        out
    }
}

/// The app's own prompts: model levels, setup findings and accounts to reconnect, in that order.
/// The open questions, the open notices with their message, and each active secondmate's agent.
type Requests = (
    Vec<messages::MessageRow>,
    Vec<(messages::MessageRow, events::NoticeState)>,
    Vec<(AgentId, qm_types::ids::SecondmateId)>,
);

/// The questions and notices still waiting on you, from the agents that can still act on your
/// answer: the current seat (`seat`) and every active secondmate. A question from a retired
/// secondmate, or a question or notice under an earlier seat's agent, has nobody left to read the
/// reply, so it is not listed; the conversation it lived in is gone with it. Notices only ever
/// come from the seat's own turns.
fn open_requests(conn: &Connection, seat: AgentId) -> Result<Requests, qm_store::StoreError> {
    let mates = secondmates::active(conn)?;
    let mut agents = vec![seat];
    agents.extend(mates.iter().map(|m| m.agent));
    let questions = messages::open_questions(conn, &agents)?;
    let mut notices = Vec::new();
    for (message, state) in events::pending_notices(conn)? {
        if let Some(row) = messages::get(conn, message)?
            && row.agent == seat
        {
            notices.push((row, state));
        }
    }
    let mates = mates.iter().map(|m| (m.agent, m.id)).collect();
    Ok((questions, notices, mates))
}

fn configuration(snapshot: &Snapshot, now: jiff::Timestamp) -> Vec<ActionItem> {
    let mut out = Vec::new();
    let setup_card = |heading: &str, text: &str, button: CardButton| {
        let mut card = Card::new(CardKind::Notice, heading);
        card.lines = vec![CardLine::new(2, vec![Styled::new(text, Tone::Text)])];
        card.buttons = vec![button];
        card
    };
    let settings = || CardButton::new("Open Settings", 's', CardAction::OpenSettings);
    if snapshot.model_setup_notice.is_some() {
        out.push(ActionItem::new(
            ActionKey::ModelLevels,
            ActionKind::Setup,
            "Choose model levels",
            now,
            setup_card("Choose model levels", MODEL_SETUP_NOTICE, settings()),
            ActionOpen::Settings,
        ));
    }
    for issue in &snapshot.setup_issues {
        out.push(ActionItem::new(
            ActionKey::Setup {
                issue: issue.clone(),
            },
            ActionKind::Setup,
            first_line(issue),
            now,
            setup_card(
                "Setup",
                issue,
                CardButton::new("Open Setup", 's', CardAction::OpenSetup),
            ),
            ActionOpen::Setup,
        ));
    }
    for account in &snapshot.settings.accounts {
        let Some(problem) = &account.problem else {
            continue;
        };
        let heading = format!("Reconnect {}", account.label);
        out.push(ActionItem::new(
            ActionKey::Account { id: account.id },
            ActionKind::Setup,
            heading.clone(),
            now,
            setup_card(&heading, problem, settings()),
            ActionOpen::Settings,
        ));
    }
    out
}

/// The inbox's order: the app's configuration prompts first, then everything else newest first.
/// Never cut: every active request stays reachable, however many there are.
fn order(mut config: Vec<ActionItem>, mut items: Vec<ActionItem>) -> Vec<ActionItem> {
    // Each source is read oldest first, so reversed, a stable sort keeps requests made in the
    // same millisecond newest first too.
    items.reverse();
    items.sort_by_key(|item| std::cmp::Reverse(item.at));
    config.append(&mut items);
    config
}

/// Who an agent is, as the UI addresses it.
fn agent_ref(
    conn: &Connection,
    seat: AgentId,
    agent: AgentId,
    task: Option<TaskId>,
) -> Result<Option<AgentRef>, qm_store::StoreError> {
    if agent == seat {
        return Ok(Some(AgentRef::Quartermaster));
    }
    Ok(match agents::role_of(conn, agent)? {
        Some((Role::Quartermaster, _)) => Some(AgentRef::Quartermaster),
        Some((Role::Secondmate, Some(id))) => Some(AgentRef::Secondmate { id }),
        Some((Role::Crewmate, _)) => agents::load(conn, agent)?
            .map(|a| AgentRef::Crewmate { task: a.task })
            .or(task.map(|task| AgentRef::Crewmate { task })),
        _ => None,
    })
}

/// The review card's supervisor is presentation, not the marker/protocol supervisor label.
fn review_supervisor(conn: &Connection, agent: AgentId) -> Result<String, qm_store::StoreError> {
    match agents::role_of(conn, agent)? {
        Some((Role::Secondmate, id)) => Ok(id
            .map(|id| source_label(conn, Some(AgentRef::Secondmate { id }), None))
            .transpose()?
            .flatten()
            .unwrap_or_else(|| "Secondmate".to_owned())),
        _ => Ok("the Quartermaster".to_owned()),
    }
}

/// A retained source's friendly name, including after it has left the crew tree.
fn source_label(
    conn: &Connection,
    from: Option<AgentRef>,
    task: Option<TaskId>,
) -> Result<Option<String>, qm_store::StoreError> {
    if let Some(AgentRef::Secondmate { id }) = from {
        // `secondmates::load` excludes retired records; a held action may still need its name.
        return Ok(conn
            .query_row("SELECT name FROM secondmates WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?);
    }
    let Some(task) = task else {
        return Ok(None);
    };
    let Some(row) = task_store::load(conn, task)? else {
        return Ok(None);
    };
    Ok(Some(
        qm_store::task_labels::name(conn, row.label)?.unwrap_or(row.title),
    ))
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_owned()
}

impl ApiPart for ActionsPart {
    fn handle<'a>(&'a self, _: &'a Command) -> Option<BoxFuture<'a, Handled>> {
        None
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            snapshot.actions = self.build(snapshot).await;
        })
    }
}
