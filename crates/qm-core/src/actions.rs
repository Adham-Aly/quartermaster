//! The Actions inbox: every request waiting on you, rebuilt for each snapshot from the
//! authoritative pending states. Nothing here is stored, so an item can never outlive its request.
//!
//! Each source is read directly, never scraped from `snapshot.conversation`, so a request older
//! than the conversation's window is still listed.

use crate::api::{ApiPart, Handled, MODEL_SETUP_NOTICE, notice_card, question_card};
use crate::held::HeldService;
use crate::plans::{Stage, api as plan_api};
use crate::tasks::TaskService;
use qm_plans::PAGE_FILE;
use qm_store::{agents, events, messages, secondmates, tasks as task_store};
use qm_types::BoxFuture;
use qm_types::api::{
    ActionItem, ActionKey, ActionKind, ActionOpen, AgentRef, Card, CardAction, CardButton,
    CardKind, CardLine, Command, Snapshot, Styled, Tone,
};
use qm_types::data_dir::PLANS;
use qm_types::domain::{PlanMode, Role, TaskKind};
use qm_types::ids::{AgentId, TaskId};
use rusqlite::Connection;

/// At most this many items in one snapshot, so a runaway source can't flood it.
pub const MAX_ITEMS: usize = 200;

/// The inbox's part. It handles no command: every button maps to one the other parts take.
#[derive(Clone, Debug)]
pub struct ActionsPart {
    held: HeldService,
    tasks: TaskService,
}

impl ActionsPart {
    pub fn new(held: HeldService, tasks: TaskService) -> Self {
        Self { held, tasks }
    }

    async fn build(&self, snapshot: &Snapshot) -> Vec<ActionItem> {
        let deps = self.tasks.deps();
        let now = deps.clock.now();
        let mut config = configuration(snapshot, now);
        let mut items = Vec::new();

        match self.held.pending_cards().await {
            Ok(rows) => {
                let seat = deps.seat_agent;
                let asked: Vec<(Option<AgentId>, Option<TaskId>)> =
                    rows.iter().map(|(r, _, _)| (r.asked_by, r.task)).collect();
                let refs = deps
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
                for (i, (row, card, anchor)) in rows.into_iter().enumerate() {
                    let kind = match card.kind {
                        CardKind::Decision => ActionKind::Decision,
                        _ => ActionKind::Approval,
                    };
                    let mut item = ActionItem::new(
                        ActionKey::Held { id: row.id },
                        kind,
                        row.title.clone(),
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
                    if let Some(anchor) = anchor {
                        item = item.with_anchor(anchor);
                    }
                    items.push(item);
                }
            }
            Err(e) => tracing::error!(error = %e, "couldn't read the held actions for the inbox"),
        }

        items.extend(self.plans().await);
        items.extend(self.questions_and_notices().await);

        items.sort_by(|a, b| b.at.cmp(&a.at));
        config.append(&mut items);
        if config.len() > MAX_ITEMS {
            tracing::warn!(items = config.len(), "the actions inbox was cut to its cap");
            config.truncate(MAX_ITEMS);
        }
        config
    }

    /// Plans open for you: an interactive plan whose round waits on your reply or approval.
    async fn plans(&self) -> Vec<ActionItem> {
        let deps = self.tasks.deps();
        let plans: Vec<_> = plan_api::live(&deps.data_dir.scratch().join(PLANS))
            .into_iter()
            .filter(|(_, s, _)| {
                matches!(s.stage, Stage::WithUser | Stage::Final)
                    && s.locked.is_none()
                    && !s.pending_reply
            })
            .collect();
        let mut out = Vec::new();
        for (task, state, dir) in plans {
            let row = match deps
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
            {
                Ok(Some(row)) => row,
                _ => continue,
            };
            let (row, project) = row;
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
            let mut item = ActionItem::new(
                ActionKey::Plan { task },
                ActionKind::Plan,
                title,
                at,
                card,
                ActionOpen::Plan { task },
            )
            .with_task(task)
            .with_from(AgentRef::Crewmate { task });
            if let Some(anchor) = state.card {
                item = item.with_anchor(anchor);
            }
            out.push(item);
        }
        out
    }

    /// Questions from the Quartermaster and live secondmates, and the seat's unanswered notices.
    async fn questions_and_notices(&self) -> Vec<ActionItem> {
        let deps = self.tasks.deps();
        let seat = deps.seat_agent;
        let read = deps
            .db
            .call(move |conn| {
                let mates = secondmates::active(conn)?;
                let mut agents = vec![seat];
                agents.extend(mates.iter().map(|m| m.agent));
                let questions = messages::open_questions(conn, &agents)?;
                let mut notices = Vec::new();
                for (message, state) in events::pending_notices(conn)? {
                    if let Some(row) = messages::get(conn, message)? {
                        notices.push((row, state));
                    }
                }
                let mates: Vec<(AgentId, qm_types::ids::SecondmateId)> =
                    mates.iter().map(|m| (m.agent, m.id)).collect();
                Ok((questions, notices, mates))
            })
            .await;
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
            if row.agent != seat {
                continue;
            }
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
