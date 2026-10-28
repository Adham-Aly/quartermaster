//! The plan's card, listed in the Actions panel, and its buttons: Open (the page, in your browser
//! on this Mac), Copy path (in the UI) and Approve. The card says where the plan stands now.

use super::{PlanState, Stage};
use crate::api::{ApiPart, Handled};
use crate::tasks::TaskService;
use crate::tasks::plan::Approver;
use qm_plans::PAGE_FILE;
use qm_types::BoxFuture;
use qm_types::api::{
    Card, CardAction, CardButton, CardKind, CardLine, Command, Snapshot, Styled, Tone,
};
use qm_types::data_dir::DataDir;
use qm_types::ids::TaskId;
use std::path::{Path, PathBuf};

/// The card's heading while a round's questions wait on you.
pub const QUESTIONS_HEADING: &str = "Plan questions";
/// The card's heading while the final plan waits on your approval.
pub const FINAL_HEADING: &str = "Plan ready";
/// The card's heading otherwise.
pub const HEADING: &str = "Plan";

/// The plans' commands.
#[derive(Clone, Debug)]
pub struct PlanPart {
    svc: TaskService,
}

impl PlanPart {
    pub fn new(svc: TaskService) -> Self {
        Self { svc }
    }

    /// Every plan with a card: the card's message, the task, its state and its folder.
    async fn open(&self, task: TaskId) {
        let page = match self.svc.paths(task).await {
            Ok(paths) => paths.plan.join(PAGE_FILE),
            Err(e) => {
                self.note(&format!("couldn't find {task}'s plan: {e}"))
                    .await;
                return;
            }
        };
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

/// Every crewmate's plan folder (`artifacts/T-n/C-n/plan`) with a readable state: its crewmate,
/// state and folder.
pub(crate) fn live(data: &DataDir) -> Vec<(TaskId, PlanState, PathBuf)> {
    data.filed_artifacts()
        .into_iter()
        .filter_map(|(_, task, artifacts)| {
            let dir = artifacts.join(qm_types::data_dir::PLAN);
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
    card.title = Some(title.to_owned());
    card.note = Some(format!("· {project} · round {}", state.round.max(1)));
    // The page sends its reply itself while the endpoint it was rendered for listens.
    let sends = state.page_port.is_some();
    let words = match (&state.locked, state.stage) {
        (Some(reason), _) => reason.clone(),
        (None, _) if state.pending_reply => format!(
            "Your reply to round {} went to the planner; it's writing round {}.",
            state.round,
            state.round + 1
        ),
        (None, Stage::WithUser) => format!(
            "{} open. Open the page, answer, then {}",
            match state.open.len() {
                1 => "1 question".to_owned(),
                n => format!("{n} questions"),
            },
            if sends {
                "Send to Quartermaster (or Copy reply and paste it here)."
            } else {
                "Copy reply on the page and paste it here."
            }
        ),
        (None, Stage::Final) if sends => {
            "No questions left. Approve it, or change a section on the page and send the reply."
                .to_owned()
        }
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

    fn fill<'a>(&'a self, _: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        // Plan cards reach the UI only through the Actions list (`actions::ActionsPart`).
        Box::pin(async {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_card_projection_preserves_authored_title_page_path_and_button_targets() {
        let task = TaskId::new(42).unwrap();
        let title = "plan: authored T-0042 and C-0047";
        let state: PlanState = serde_json::from_value(serde_json::json!({
            "uid": "fixture", "stage": "final", "round": 1
        }))
        .unwrap();
        let page = Path::new("/artifacts/T-0042/C-0042/plan/plan.html");
        let card = card(task, title, "app", &state, page);
        assert_eq!(card.title.as_deref(), Some(title));
        assert_eq!(card.lines[1].spans[0].text, page.display().to_string());
        assert_eq!(card.buttons[0].action, CardAction::OpenPlan { task });
        assert_eq!(
            card.buttons[1].action,
            CardAction::CopyPlanPath {
                path: page.display().to_string()
            }
        );
        assert_eq!(card.buttons[2].action, CardAction::ApprovePlan { task });
    }
}
