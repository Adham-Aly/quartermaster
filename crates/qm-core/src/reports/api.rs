//! Completed static reports: the task pane's Open report, and their cards in the Actions panel.
//!
//! A finished answer task's report (lookup, research, reproduce, teach) has a card
//! ([`report_card`]: Open, Dismiss) until you explicitly dismiss it. Opening never consumes it.
//! `tasks.report_read_at` records dismissal; a newer report brings the card back. Completed
//! crewmates stay visible with their reports until dismissal, even after supervisor closure.
//! A review's report under normal autonomy has its own card while it waits on your go-ahead
//! ([`review_card`]; [`crate::tasks::review`]): Open, and Go ahead.

use crate::api::{ApiPart, Handled};
use crate::tasks::TaskService;
use crate::tasks::review::{GoAhead, awaits_go_ahead, go_ahead};
use qm_store::reports::ReportRow;
use qm_store::tasks;
use qm_types::BoxFuture;
use qm_types::api::{
    AgentRef, Card, CardAction, CardButton, CardKind, CardLine, Command, CrewRow, Snapshot, Styled,
    TaskDetail, Tone,
};
use qm_types::data_dir::TaskPaths;
use qm_types::domain::{TaskKind, TaskStatus};
use qm_types::ids::TaskId;
use std::path::PathBuf;

/// Presents completed report artifacts and opens only their fixed entry point.
#[derive(Clone, Debug)]
pub struct ReportPart {
    svc: TaskService,
}

impl ReportPart {
    pub fn new(svc: TaskService) -> Self {
        Self { svc }
    }

    async fn page(&self, task: TaskId) -> Result<PathBuf, String> {
        let reported = self
            .svc
            .deps()
            .db
            .call(move |conn| Ok(tasks::load(conn, task)?.filter(has_report)))
            .await
            .map_err(|_| "The report's task couldn't be read.".to_owned())?;
        let Some(reported) = reported else {
            return Err(format!("{task} has no completed report yet."));
        };
        report_page(&self.svc.paths_of(&reported)).map_err(|_| {
            format!("{task}'s report is missing or isn't a plain file with plain parent folders.")
        })
    }

    async fn open(&self, task: TaskId) {
        self.open_with(task, "/usr/bin/open").await;
    }

    async fn open_with(&self, task: TaskId, opener: &str) {
        let problem = match self.page(task).await {
            Ok(page) => {
                let result = tokio::process::Command::new(opener)
                    .arg("--")
                    .arg(page)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await;
                if result.is_ok_and(|status| status.success()) {
                    return;
                }
                format!("{task}'s report couldn't be opened in your browser on this Mac.")
            }
            Err(problem) => problem,
        };
        self.note(&problem).await;
    }

    /// Only explicit dismissal acknowledges the report and releases its retained files.
    async fn dismiss(&self, task: TaskId) {
        let deps = self.svc.deps();
        let Ok(Some(row)) = deps.db.call(move |conn| tasks::load(conn, task)).await else {
            return;
        };
        if !has_report_card(&row) {
            return;
        }
        if row.status == TaskStatus::Reported
            && let Err(e) =
                crate::tasks::close::retire(&self.svc, task, crate::tasks::close::Exit::Answered)
                    .await
        {
            self.note(&format!("{task}'s report couldn't be dismissed: {e}"))
                .await;
            return;
        }
        let at = deps.clock.now();
        if let Err(e) = deps
            .db
            .call(move |conn| tasks::mark_report_read(conn, task, at))
            .await
        {
            tracing::error!(error = %e, %task, "couldn't record that a report was read");
            return;
        }
        crate::retention::after_report_dismissed(
            deps.db.clone(),
            deps.data_dir.clone(),
            deps.clock.clone(),
        );
    }

    /// The Go ahead on a review's card.
    async fn go_ahead(&self, task: TaskId) {
        if let Err(e) = go_ahead(&self.svc, GoAhead::Card, task).await {
            self.note(&format!(
                "{task}'s go-ahead wasn't recorded: {}",
                e.render()
            ))
            .await;
        }
    }

    async fn note(&self, text: &str) {
        let deps = self.svc.deps();
        if let Err(e) = deps.chat.app_note(deps.seat_agent, None, text).await {
            tracing::error!(error = %e, "couldn't post a report note");
        }
    }
}

/// A finished report the user may open: reported, or a review waiting on the user's go-ahead.
fn has_report(row: &tasks::TaskRow) -> bool {
    row.reported_at.is_some() || awaits_go_ahead(row)
}

/// The card's heading for a review waiting on your go-ahead.
pub const REVIEW_HEADING: &str = "Review ready";

/// The card for a review waiting on your go-ahead: what it found, and its two buttons. `who` is
/// its supervisor as you know it: "the Quartermaster" or "S-02".
pub(crate) fn review_card(
    task: TaskId,
    title: &str,
    project: &str,
    report: Option<&ReportRow>,
    who: &str,
) -> Card {
    let mut card = Card::new(CardKind::Review, REVIEW_HEADING);
    card.title = Some(title.to_owned());
    card.note = Some(format!("· {project}"));
    let mut lines = Vec::new();
    if let Some(report) = report {
        let found = crate::verification::review::findings(report);
        let mut parts = Vec::new();
        if let Some(verdict) = report.review_verdict {
            parts.push(format!("Verdict {verdict}"));
        }
        if let Some(risk) = report.review_risk {
            parts.push(format!("risk {risk}"));
        }
        parts.push(if found.is_empty() {
            "no findings".to_owned()
        } else {
            crate::verification::review::count_words(&found).replacen(
                if found.len() == 1 { "thing" } else { "things" },
                if found.len() == 1 {
                    "finding"
                } else {
                    "findings"
                },
                1,
            )
        });
        lines.push(CardLine::new(
            2,
            vec![Styled::new(parts.join(" · "), Tone::Text)],
        ));
    }
    lines.push(CardLine::new(
        2,
        vec![Styled::new(
            format!(
                "Open the report. Go ahead lets {who} do with the findings what you asked for, or \
                 nothing if you didn't say; or tell {who} what to do."
            ),
            Tone::Dim,
        )],
    ));
    card.lines = lines;
    card.buttons = vec![
        CardButton::new("Open", 'o', CardAction::OpenReport { task }),
        CardButton::new("Go ahead", 'g', CardAction::ApproveReview { task }),
    ];
    card
}

/// The card's heading for a finished answer task's report.
pub const REPORT_HEADING: &str = "Report ready";

/// The longest piece of the answer a report's card quotes.
const ANSWER_CHARS: usize = 160;

/// Whether `row`'s report has a card in Actions until you dismiss it: a finished report task,
/// still reported or closed with any outcome. Closing or stopping work is not dismissal.
/// Whether it is pending dismissal is the store's ([`tasks::unread_reports`]); whether its
/// page is there is [`report_page`]'s.
pub(crate) fn has_report_card(row: &tasks::TaskRow) -> bool {
    crate::reports::answers_with_report(row.kind)
        && row.reported_at.is_some()
        && matches!(row.status, TaskStatus::Reported | TaskStatus::Closed)
}

/// The kinds whose reports get a card, for [`tasks::unread_reports`].
pub(crate) fn card_kinds() -> Vec<TaskKind> {
    TaskKind::ALL
        .iter()
        .copied()
        .filter(|k| crate::reports::answers_with_report(*k))
        .collect()
}

/// The card for a finished answer task's report you haven't dismissed: the answer's first line, and
/// Open and Dismiss.
pub(crate) fn report_card(
    task: TaskId,
    kind: TaskKind,
    title: &str,
    project: &str,
    report: Option<&ReportRow>,
) -> Card {
    let mut card = Card::new(CardKind::Report, REPORT_HEADING);
    card.title = Some(title.to_owned());
    card.note = Some(format!("· {project}"));
    let mut lines = Vec::new();
    if let Some(first) = report
        .and_then(|r| r.answer.as_deref())
        .and_then(|a| a.lines().map(str::trim).find(|l| !l.is_empty()))
    {
        lines.push(CardLine::new(
            2,
            vec![Styled::new(
                qm_prompts::context_block::cut(first, ANSWER_CHARS),
                Tone::Text,
            )],
        ));
    }
    lines.push(CardLine::new(
        2,
        vec![Styled::new(
            format!(
                "The {kind} is done. Open its report in your browser as often as you like. \
                 Dismiss removes the report and its completed crewmate."
            ),
            Tone::Dim,
        )],
    ));
    card.lines = lines;
    card.buttons = vec![
        CardButton::new("Open", 'o', CardAction::ReadReport { task }),
        CardButton::new("Dismiss", 'd', CardAction::DismissReport { task }),
    ];
    card
}

/// No caller-supplied path, and no symlink in the entry point or any parent, including the
/// artifacts root and data directory. Check parents before the leaf, without following links.
pub(crate) fn report_page(paths: &TaskPaths) -> std::io::Result<PathBuf> {
    let page = std::path::absolute(paths.artifacts.join("report").join("report.html"))?;
    let ancestors: Vec<_> = page.ancestors().collect();
    for path in ancestors.into_iter().rev() {
        let metadata = std::fs::symlink_metadata(path)?;
        let valid = if path == page {
            metadata.is_file()
        } else {
            metadata.is_dir()
        };
        if !valid {
            return Err(std::io::Error::other(
                "report path isn't a plain file with plain parent folders",
            ));
        }
    }
    Ok(page)
}

impl ApiPart for ReportPart {
    fn handle<'a>(&'a self, command: &'a Command) -> Option<BoxFuture<'a, Handled>> {
        match command {
            Command::OpenReport { task } => Some(Box::pin(async move {
                self.open(*task).await;
                Handled::Changed
            })),
            Command::DismissReport { task } => Some(Box::pin(async move {
                self.dismiss(*task).await;
                Handled::Changed
            })),
            Command::ApproveReview { task } => Some(Box::pin(async move {
                self.go_ahead(*task).await;
                Handled::Changed
            })),
            _ => None,
        }
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // Closed report tasks keep their report and completed crew identity until dismissal.
            let pending = self
                .svc
                .deps()
                .db
                .call(|conn| tasks::unread_reports(conn, &card_kinds()))
                .await
                .unwrap_or_default();
            let mut reports = Vec::new();
            for (_, task, _) in self.svc.deps().data_dir.filed_artifacts() {
                if let Ok(Some(row)) = self
                    .svc
                    .deps()
                    .db
                    .call(move |conn| tasks::load(conn, task))
                    .await
                    && (row.status != TaskStatus::Closed
                        || (has_report_card(&row)
                            && pending.iter().any(|pending| pending.id == task)))
                    && has_report(&row)
                    && report_page(&self.svc.paths_of(&row)).is_ok()
                {
                    reports.push(row);
                }
            }
            reports.sort_by_key(|row| std::cmp::Reverse(row.reported_at));
            for row in reports {
                surface(snapshot, row.id, &row.title);
            }
        })
    }
}

/// Add the report without changing a live task's status or creating an approval.
fn surface(snapshot: &mut Snapshot, task: TaskId, title: &str) {
    if let Some(detail) = snapshot.tasks.iter_mut().find(|d| d.task == task) {
        detail.report_available = true;
    } else {
        let mut detail = TaskDetail::new(task, title);
        detail.report_available = true;
        snapshot.tasks.push(detail);
    }
    let agent = AgentRef::Crewmate { task };
    if !snapshot.crew.iter().any(|row| row.agent == agent) {
        snapshot
            .crew
            .push(CrewRow::new(agent, 1, title, "reported"));
    }
}
