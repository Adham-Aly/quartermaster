//! The plan's card in the Quartermaster's chat, and its buttons: Open (the page, in your browser
//! on this Mac), Copy path (in the UI) and Approve.
//!
//! The card hangs on the app note the plan's last round posted ([`super::PlanState::card`]); the
//! note's words say what the round asked, and the card says where the plan stands now.

use super::{PlanState, Stage};
use crate::api::{ApiPart, Handled};
use crate::tasks::TaskService;
use crate::tasks::plan::Approver;
use qm_plans::PAGE_FILE;
use qm_store::tasks as task_store;
use qm_types::BoxFuture;
use qm_types::api::{
    Card, CardAction, CardButton, CardKind, CardLine, Command, Snapshot, Styled, Tone,
};
use qm_types::data_dir::PLANS;
use qm_types::domain::TaskKind;
use qm_types::ids::{MessageId, TaskId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The card's heading while a round's questions wait on you.
pub const QUESTIONS_HEADING: &str = "Plan questions";
/// The card's heading while the final plan waits on your approval.
pub const FINAL_HEADING: &str = "Plan ready";
/// The card's heading otherwise.
pub const HEADING: &str = "Plan";

/// The plans' commands and cards.
#[derive(Clone, Debug)]
pub struct PlanPart {
    svc: TaskService,
}

impl PlanPart {
    pub fn new(svc: TaskService) -> Self {
        Self { svc }
    }

    /// Every plan with a card: the card's message, the task, its state and its folder.
    fn plans(&self) -> Vec<(MessageId, TaskId, PlanState, PathBuf)> {
        live(&self.svc.deps().data_dir.scratch().join(PLANS))
            .into_iter()
            .filter_map(|(task, state, dir)| Some((state.card?, task, state, dir)))
            .collect()
    }

    async fn open(&self, task: TaskId) {
        let page = self.svc.deps().data_dir.task(task).plan.join(PAGE_FILE);
        let problem = match std::fs::symlink_metadata(&page) {
            Ok(meta) if meta.is_file() => match open_page(&page).await {
                Ok(()) => return,
                Err(e) => format!("couldn't open {task}'s plan in your browser: {e}"),
            },
            Ok(_) => format!("{task}'s plan page isn't a plain file, so it wasn't opened"),
            Err(_) => format!("{task}'s plan has no page yet"),
        };
        self.note(&format!("{problem}. Its page is at {}", page.display()))
            .await;
    }

    async fn approve(&self, task: TaskId) {
        if let Err(e) = crate::tasks::plan::approve(&self.svc, Approver::Card, task, false).await {
            self.note(&format!("{task}'s plan wasn't approved: {}", e.render()))
                .await;
        }
    }

    async fn note(&self, text: &str) {
        let deps = self.svc.deps();
        if let Err(e) = deps.chat.app_note(deps.seat_agent, None, text).await {
            tracing::error!(error = %e, "couldn't post a plan note");
        }
    }
}

/// Every plan folder under `root` (the scratch `plans/` folder) with a readable state: its task,
/// state and folder.
pub(crate) fn live(root: &Path) -> Vec<(TaskId, PlanState, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let task = e.file_name().to_str()?.parse::<TaskId>().ok()?;
            let dir = e.path();
            let state = super::load(&dir).ok()??;
            Some((task, state, dir))
        })
        .collect()
}

/// Opens the page with macOS's `open`, which hands it to your default browser.
async fn open_page(page: &Path) -> std::io::Result<()> {
    let status = tokio::process::Command::new("/usr/bin/open")
        .arg("--")
        .arg(page)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("open exited with {status}")))
    }
}

/// The card for a plan in `state`.
pub(crate) fn card(
    task: TaskId,
    title: &str,
    project: &str,
    state: &PlanState,
    page: &Path,
) -> Card {
    let final_ready = state.stage == Stage::Final && state.locked.is_none() && !state.pending_reply;
    let heading = match state.stage {
        _ if state.locked.is_some() || state.pending_reply => HEADING,
        Stage::WithUser => QUESTIONS_HEADING,
        Stage::Final => FINAL_HEADING,
        Stage::Drafting | Stage::Approved => HEADING,
    };
    let mut card = Card::new(CardKind::Plan, heading);
    card.title = Some(format!("{task} {title}"));
    card.note = Some(format!("· {project} · round {}", state.round.max(1)));
    let words = match (&state.locked, state.stage) {
        (Some(reason), _) => reason.clone(),
        (None, _) if state.pending_reply => format!(
            "Your reply went to the planner; it's writing round {}.",
            state.round + 1
        ),
        (None, Stage::WithUser) => format!(
            "{} open. Open the page, answer, then Copy reply on the page and paste it here.",
            match state.open.len() {
                1 => "1 question".to_owned(),
                n => format!("{n} questions"),
            }
        ),
        (None, Stage::Final) => {
            "No questions left. Approve it, or change a section on the page and paste the reply here."
                .to_owned()
        }
        (None, Stage::Drafting) => "The planner is writing the plan.".to_owned(),
        (None, Stage::Approved) => "Approved; the build works from a copy of it.".to_owned(),
    };
    card.lines = vec![
        CardLine::new(2, vec![Styled::new(words, Tone::Text)]),
        CardLine::new(2, vec![Styled::new(page.display().to_string(), Tone::Dim)]),
    ];
    card.buttons = vec![
        CardButton::new("Open", 'o', CardAction::OpenPlan { task }),
        CardButton::new(
            "Copy path",
            'c',
            CardAction::CopyPlanPath {
                path: page.display().to_string(),
            },
        ),
    ];
    if final_ready {
        card.buttons.push(CardButton::new(
            "Approve",
            'a',
            CardAction::ApprovePlan { task },
        ));
    }
    if state.stage == Stage::Approved {
        card.answered = Some(match &state.approved_at {
            Some(at) if !at.is_empty() => format!("Approved {at}"),
            _ => "Approved".to_owned(),
        });
    }
    card
}

impl ApiPart for PlanPart {
    fn handle<'a>(&'a self, command: &'a Command) -> Option<BoxFuture<'a, Handled>> {
        match command {
            Command::OpenPlan { task } => Some(Box::pin(async move {
                self.open(*task).await;
                Handled::Changed
            })),
            Command::ApprovePlan { task } => Some(Box::pin(async move {
                self.approve(*task).await;
                Handled::Changed
            })),
            _ => None,
        }
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let plans = self.plans();
            if plans.is_empty() {
                return;
            }
            let ids: Vec<TaskId> = plans.iter().map(|p| p.1).collect();
            let rows = self
                .svc
                .db()
                .call(move |conn| {
                    let mut out = HashMap::new();
                    for id in ids {
                        if let Some(row) = task_store::load(conn, id)?
                            && row.kind == TaskKind::Plan
                        {
                            let project = crate::tasks::load_project(conn, row.project)?
                                .map(|p| p.name)
                                .unwrap_or_default();
                            out.insert(id, (row.title, project));
                        }
                    }
                    Ok(out)
                })
                .await
                .unwrap_or_default();
            let cards: HashMap<MessageId, Card> = plans
                .into_iter()
                .filter_map(|(message, task, state, dir)| {
                    let (title, project) = rows.get(&task)?;
                    Some((
                        message,
                        card(task, title, project, &state, &dir.join(PAGE_FILE)),
                    ))
                })
                .collect();
            for line in &mut snapshot.conversation {
                if let Some(card) = cards.get(&line.id) {
                    line.card = Some(card.clone());
                }
            }
        })
    }
}
