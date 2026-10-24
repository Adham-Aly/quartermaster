//! Completed static reports belong in task details, not the Actions approval inbox. A review's
//! report under normal autonomy also has a card in the Actions panel while it waits on your
//! go-ahead ([`review_card`]; [`crate::tasks::review`]): Open, and Go ahead.

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
        let problem = match self.page(task).await {
            Ok(page) => {
                let result = tokio::process::Command::new("/usr/bin/open")
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
    card.title = Some(format!("{task} {title}"));
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

/// No caller-supplied path, and no symlink in the entry point or any parent, including the
/// artifacts root and data directory. Check parents before the leaf, without following links.
fn report_page(paths: &TaskPaths) -> std::io::Result<PathBuf> {
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
            Command::ApproveReview { task } => Some(Box::pin(async move {
                self.go_ahead(*task).await;
                Handled::Changed
            })),
            _ => None,
        }
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // Closed answer-only tasks leave the live crew tree. Retained reports get a
            // resting crew row so their task detail action survives closure and restart.
            let mut reports = Vec::new();
            for (_, task, _) in self.svc.deps().data_dir.filed_artifacts() {
                if let Ok(Some(row)) = self
                    .svc
                    .deps()
                    .db
                    .call(move |conn| tasks::load(conn, task))
                    .await
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
            .push(CrewRow::new(agent, 1, format!("{task} report"), "reported"));
    }
}

#[cfg(test)]
#[path = "delivery_tests.rs"]
mod delivery_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn retained_reports_survive_closure_without_becoming_actions() {
        let task = TaskId::new(42).unwrap();
        let mut snapshot = Snapshot::default();
        surface(&mut snapshot, task, "Research results");
        assert_eq!(snapshot.crew.len(), 1);
        assert!(!snapshot.crew[0].busy);
        assert!(!snapshot.crew[0].needs_you);
        assert!(snapshot.tasks[0].report_available);
        assert_eq!(snapshot.actions, [] as [qm_types::api::ActionItem; 0]);
        surface(&mut snapshot, task, "Research results");
        assert_eq!(snapshot.crew.len(), 1);
        assert_eq!(snapshot.tasks.len(), 1);
        // An existing live detail isn't replaced by the retained-report view.
        snapshot.tasks[0].running = true;
        surface(&mut snapshot, task, "Research results");
        assert!(snapshot.tasks[0].running);
    }

    #[test]
    fn report_entry_and_every_parent_must_be_plain() {
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("qm-report-api-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let data = qm_types::data_dir::DataDir::new(base.join("data"));
        let task = TaskId::new(42).unwrap();
        let paths = data.task(task, task.own_label());
        let page = paths.artifacts.join("report/report.html");
        std::fs::create_dir_all(page.parent().unwrap()).unwrap();
        assert!(report_page(&paths).is_err(), "missing page");
        std::fs::write(&page, "<!doctype html><title>Report</title>").unwrap();
        assert_eq!(report_page(&paths).unwrap(), page);
        let other = TaskId::new(43).unwrap();
        assert!(report_page(&data.task(other, other.own_label())).is_err());

        // Leaf, report folder, crewmate and task artifact folders, artifacts root and data root.
        for path in page.ancestors().take(6) {
            let moved = base.join("moved");
            std::fs::rename(path, &moved).unwrap();
            symlink(&moved, path).unwrap();
            assert!(report_page(&paths).is_err(), "accepted {}", path.display());
            std::fs::remove_file(path).unwrap();
            std::fs::rename(moved, path).unwrap();
        }
        std::fs::remove_file(&page).unwrap();
        std::fs::create_dir(&page).unwrap();
        assert!(report_page(&paths).is_err(), "directory page");
        std::fs::remove_dir_all(base).unwrap();
    }
}
