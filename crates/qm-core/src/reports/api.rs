//! Completed static reports belong in task details, not the Actions approval inbox.

use crate::api::{ApiPart, Handled};
use crate::tasks::TaskService;
use qm_store::tasks;
use qm_types::BoxFuture;
use qm_types::api::{AgentRef, Command, CrewRow, Snapshot, TaskDetail};
use qm_types::data_dir::DataDir;
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
            .call(move |conn| {
                Ok(tasks::load(conn, task)?.is_some_and(|row| row.reported_at.is_some()))
            })
            .await
            .map_err(|_| "The report's task couldn't be read.".to_owned())?;
        if !reported {
            return Err(format!("{task} has no completed report yet."));
        }
        report_page(&self.svc.deps().data_dir, task).map_err(|_| {
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
        let deps = self.svc.deps();
        if let Err(e) = deps.chat.app_note(deps.seat_agent, None, &problem).await {
            tracing::error!(error = %e, "couldn't post a report note");
        }
    }
}

/// No caller-supplied path, and no symlink in the entry point or any parent, including the
/// artifacts root and data directory. Check parents before the leaf, without following links.
fn report_page(data: &DataDir, task: TaskId) -> std::io::Result<PathBuf> {
    let page = std::path::absolute(data.task(task).artifacts.join("report").join("report.html"))?;
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
            _ => None,
        }
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // Closed answer-only tasks leave the live crew tree. Retained reports get a
            // resting crew row so their task detail action survives closure and restart.
            let Ok(entries) = std::fs::read_dir(self.svc.deps().data_dir.artifacts_dir()) else {
                return;
            };
            let mut reports = Vec::new();
            for entry in entries.flatten() {
                let Some(task) = entry
                    .file_name()
                    .to_str()
                    .and_then(|s| s.parse::<TaskId>().ok())
                else {
                    continue;
                };
                if report_page(&self.svc.deps().data_dir, task).is_err() {
                    continue;
                }
                if let Ok(Some(row)) = self
                    .svc
                    .deps()
                    .db
                    .call(move |conn| tasks::load(conn, task))
                    .await
                    && row.reported_at.is_some()
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
        let data = DataDir::new(base.join("data"));
        let task = TaskId::new(42).unwrap();
        let page = data.task(task).artifacts.join("report/report.html");
        std::fs::create_dir_all(page.parent().unwrap()).unwrap();
        assert!(report_page(&data, task).is_err(), "missing page");
        std::fs::write(&page, "<!doctype html><title>Report</title>").unwrap();
        assert_eq!(report_page(&data, task).unwrap(), page);
        assert!(report_page(&data, TaskId::new(43).unwrap()).is_err());

        // Leaf, report folder, task artifact folder, artifacts root and data root.
        for path in page.ancestors().take(5) {
            let moved = base.join("moved");
            std::fs::rename(path, &moved).unwrap();
            symlink(&moved, path).unwrap();
            assert!(
                report_page(&data, task).is_err(),
                "accepted {}",
                path.display()
            );
            std::fs::remove_file(path).unwrap();
            std::fs::rename(moved, path).unwrap();
        }
        std::fs::remove_file(&page).unwrap();
        std::fs::create_dir(&page).unwrap();
        assert!(report_page(&data, task).is_err(), "directory page");
        std::fs::remove_dir_all(base).unwrap();
    }
}
