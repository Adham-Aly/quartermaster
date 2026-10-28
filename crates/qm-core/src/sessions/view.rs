//! A worker's session view: the header, the last [`SESSION_LINES`] lines plus the block still
//! arriving, the status line, and whether you can still write to the worker.

use super::recorder::Recorder;
use crate::api::{chat_lines, load_conversation, thinking};
use crate::crew_view::minutes_label;
use crate::crew_view::{SECONDMATE_IDLE, SECONDMATE_WORKING};
use crate::secondmates::SecondmateService;
use crate::secondmates::charter::charter_text;
use crate::tasks::lifecycle::{is_exiting, is_open};
use crate::turn_io::SeatError;
use jiff::Timestamp;
use qm_store::StoreError;
use qm_store::db::Db;
use qm_store::sessions::{self, LineKind, SessionHeader, SessionLineRow};
use qm_types::api::{AgentRef, SessionLine, SessionLineKind, SessionView};
use qm_types::clock::Clock;
use qm_types::domain::{AgentStatus, Role, TaskStatus};
use qm_types::ids::{SecondmateId, TaskId};

/// The most stored lines a session view carries.
pub const SESSION_LINES: usize = 400;

/// The view of `agent`'s session, or `None` when there is nothing to show (a task with no
/// crewmate yet, a secondmate that doesn't exist).
pub async fn load(
    db: &Db,
    recorder: &Recorder,
    agent: AgentRef,
    clock: &dyn Clock,
) -> Result<Option<SessionView>, StoreError> {
    let AgentRef::Crewmate { task } = agent else {
        return Ok(None);
    };
    let loaded = db
        .call(move |conn| {
            let Some(header) = sessions::header(conn, task)? else {
                return Ok(None);
            };
            let rows = sessions::recent(conn, header.agent, SESSION_LINES)?;
            Ok(Some((header, rows)))
        })
        .await?;
    let Some((header, rows)) = loaded else {
        return Ok(None);
    };
    let streaming = recorder.streaming(header.agent);
    let mut view = build(agent, task, &header, &rows, clock.now());
    if let Some(block) = streaming {
        let mut line = SessionLine::new(0, SessionLineKind::Agent, block.at, block.text);
        line.streaming = true;
        view.lines.push(line);
    }
    Ok(Some(view))
}

/// The view from the stored facts.
pub fn build(
    agent: AgentRef,
    _task: TaskId,
    header: &SessionHeader,
    rows: &[SessionLineRow],
    now: Timestamp,
) -> SessionView {
    let mut view = SessionView::new(agent);
    view.title.clone_from(&header.title);
    view.note = note(header);
    view.model.clone_from(&header.model);
    view.effort = header.effort;
    view.place = place(header);
    view.turn_running = header.agent_status == AgentStatus::Running;
    view.status = status(header, now);
    view.open = is_open(header.status) && !is_exiting(header.status);
    view.placeholder = if view.open {
        "message this crewmate directly…".to_owned()
    } else {
        format!(
            "This crewmate is {}, so it takes no more messages.",
            header.status
        )
    };
    view.lines = rows.iter().map(line).collect();
    view
}

/// "api-server · sonnet-5-5 · supervisor: Quartermaster" (`S-02` for a secondmate's crewmate).
fn note(header: &SessionHeader) -> String {
    let supervisor = supervisor(header);
    let mut parts = vec![header.project.clone()];
    if let Some(model) = &header.model {
        parts.push(model.to_string());
    }
    parts.push(format!("supervisor: {supervisor}"));
    parts.join(" · ")
}

/// "api-server · qm/task-0042 ← main · supervisor: S-02", or the folder a folder task works in.
fn place(header: &SessionHeader) -> String {
    let at = match &header.branch {
        Some((branch, target)) => format!("{branch} ← {target}"),
        None => header.folder.clone(),
    };
    format!(
        "{} · {at} · supervisor: {}",
        header.project,
        supervisor(header)
    )
}

/// `Quartermaster`, or the secondmate's `S-02`.
fn supervisor(header: &SessionHeader) -> String {
    match header.supervisor {
        (Role::Secondmate, Some(id)) => id.to_string(),
        _ => "Quartermaster".to_owned(),
    }
}

/// "running 14m · job J3 idle", "running 14m · working", "reported 40m · idle".
fn status(header: &SessionHeader, now: Timestamp) -> String {
    let word = header.status.as_str().replace('_', " ");
    if matches!(
        header.status,
        TaskStatus::Closed | TaskStatus::Failed | TaskStatus::Stopped | TaskStatus::Discarded
    ) {
        return word;
    }
    let minutes = (now.as_second() - header.since.as_second()).max(0) / 60;
    let age = minutes_label(minutes);
    let doing = if header.agent_status == AgentStatus::Running {
        "working".to_owned()
    } else if let [job] = header.jobs.as_slice() {
        format!("job {job} idle")
    } else if !header.jobs.is_empty() {
        format!("jobs {} idle", header.jobs.join(", "))
    } else {
        "idle".to_owned()
    };
    format!("{word} {age} · {doing}")
}

fn line(row: &SessionLineRow) -> SessionLine {
    let kind = match row.kind {
        LineKind::Supervisor => SessionLineKind::Supervisor,
        LineKind::App => SessionLineKind::App,
        LineKind::You => SessionLineKind::You,
        LineKind::Agent => SessionLineKind::Agent,
        LineKind::Tool => SessionLineKind::Tool,
    };
    let mut line = SessionLine::new(
        u64::try_from(row.id).unwrap_or_default(),
        kind,
        row.at,
        super::recorder::display_text(row),
    );
    line.detail.clone_from(&row.detail);
    line
}

/// A secondmate's session: its conversation as the Bridge draws the Quartermaster's, under "S-02 ·
/// secondmate · {scope} · {status}" and its charter's first line. `None` when there's no such
/// secondmate.
pub async fn load_secondmate(
    db: &Db,
    secondmates: &SecondmateService,
    id: SecondmateId,
) -> Result<Option<SessionView>, SeatError> {
    let Some(record) = db
        .call(move |conn| qm_store::secondmates::load(conn, id))
        .await?
    else {
        return Ok(None);
    };
    let data = load_conversation(db, record.agent).await?;
    let runs_on = db
        .call(move |conn| qm_store::secondmates::session_row(conn, id))
        .await?;
    let status = secondmates.loop_status(id).unwrap_or_default();
    let mut view = SessionView::new(AgentRef::Secondmate { id });
    let word = if status.turn_running {
        SECONDMATE_WORKING
    } else {
        SECONDMATE_IDLE
    };
    "secondmate".clone_into(&mut view.title);
    view.note = format!("{} · {word}", one_line(&record.scope));
    view.place = one_line(&record.scope);
    if let Some(agent) = runs_on {
        view.model = agent.model;
        view.effort = agent.effort;
    }
    word.clone_into(&mut view.status);
    view.subtitle = charter_text(&record)
        .lines()
        .next()
        .map(|l| l.trim_start_matches('#').trim().to_owned())
        .unwrap_or_default();
    view.turn_running = status.turn_running;
    view.thinking = thinking(&data, &status);
    view.open = true;
    view.placeholder = format!("message {id} directly…");
    // Read-only apart from the input: the Bridge's notice and question buttons answer the
    // Quartermaster, so they aren't drawn here.
    view.conversation = chat_lines(&data, &status)
        .into_iter()
        .map(|mut line| {
            line.card = None;
            line
        })
        .collect();
    Ok(Some(view))
}

/// `s` on one line, its whitespace runs collapsed.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use qm_types::ids::AgentId;

    #[test]
    fn session_projections_hide_only_the_brief_wrapper_and_placeholder_ids() {
        let task = TaskId::new(42).unwrap();
        let agent = AgentRef::Crewmate { task };
        let authored = "authored T-0042 and C-0047";
        let detail = "# Brief · C-0042 in T-0042\nCode: let id = \"C-0047\";";
        let mut header = SessionHeader {
            agent: AgentId::new(1),
            task,
            title: authored.into(),
            status: TaskStatus::Running,
            since: Timestamp::UNIX_EPOCH,
            project: "app".into(),
            model: None,
            effort: None,
            branch: None,
            folder: "/work/T-0042/C-0042".into(),
            agent_status: AgentStatus::Running,
            supervisor: (Role::Quartermaster, None),
            jobs: vec![],
        };
        let row = SessionLineRow {
            id: 1,
            agent: header.agent,
            task: Some(task),
            turn: None,
            kind: LineKind::Supervisor,
            text: format!("Brief · {task} · {authored}"),
            detail: Some(detail.into()),
            message: None,
            at: Timestamp::UNIX_EPOCH,
        };
        let original = row.text.clone();
        let mut rows = vec![row];
        for kind in [
            LineKind::Agent,
            LineKind::You,
            LineKind::Tool,
            LineKind::App,
        ] {
            let mut row = rows[0].clone();
            row.kind = kind;
            rows.push(row);
        }
        let view = build(agent, task, &header, &rows, Timestamp::UNIX_EPOCH);
        assert_eq!(view.agent, agent);
        assert_eq!(view.title, authored);
        assert_eq!(view.placeholder, "message this crewmate directly…");
        assert!(view.place.contains("/work/T-0042/C-0042"));
        assert_eq!(view.lines[0].text, format!("Brief · {authored}"));
        assert_eq!(view.lines[0].detail.as_deref(), Some(detail));
        assert!(view.lines[1..].iter().all(|line| line.text == original));
        assert_eq!(
            rows[0].text, original,
            "the stored brief title is untouched"
        );
        header.status = TaskStatus::Closed;
        let view = build(agent, task, &header, &rows, Timestamp::UNIX_EPOCH);
        assert!(!view.open);
        assert_eq!(
            view.placeholder,
            "This crewmate is closed, so it takes no more messages."
        );
    }
}
