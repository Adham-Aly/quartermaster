//! What a task delivered, code or an answer, from its newest clear done report
//! (`qm_policy::delivery::deliverable`). The kind never decides it.

use qm_policy::delivery::{Deliverable, DeliveredFacts, deliverable};
use qm_store::StoreError;
use qm_store::reports::{self, ReportRow};
use qm_store::tasks::TaskRow;
use qm_types::domain::{DeliveryMode, ReportOutcome};
use rusqlite::Connection;

/// Whether a stored report carries any of the code headings.
pub fn has_headings(report: &ReportRow) -> bool {
    report.headings.as_object().is_some_and(|m| {
        m.values()
            .any(|v| v.as_str().is_some_and(|s| !s.trim().is_empty()))
    })
}

/// What `row` delivered, by `report` (its newest clear done report, if any).
pub fn of_report(row: &TaskRow, report: Option<&ReportRow>) -> Deliverable {
    let done = report.filter(|r| r.outcome == Some(ReportOutcome::Done));
    deliverable(DeliveredFacts {
        in_place: row.delivery == DeliveryMode::Folder,
        own_commits: done.is_some_and(|r| r.head_commit.is_some()),
        headings: done.is_some_and(has_headings),
    })
}

/// What `row` delivered, read from its newest clear report.
pub fn of_task(conn: &Connection, row: &TaskRow) -> Result<Deliverable, StoreError> {
    Ok(of_report(
        row,
        reports::latest_clear(conn, row.id)?.as_ref(),
    ))
}
