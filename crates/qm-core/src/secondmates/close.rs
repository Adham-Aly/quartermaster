//! `close_secondmate`: retiring one of the Quartermaster's secondmates.
//!
//! The safer of the two behaviours: a secondmate with any open task is refused, never retired
//! with work still running, so nothing it supervises is dropped silently. Retiring marks its
//! `secondmates` and `agents` rows retired and stops its loop, closing its session. History
//! remains only in memory until forgotten or restarted; numbers can be reused after restart.

use super::{SecondmateService, event_kind, failed, refuse};
use qm_policy::scope::check_own_secondmate;
use qm_store::StoreError;
use qm_store::events::{self, NewEvent};
use qm_store::secondmates;
use qm_tools::defs::Ack;
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::ids::{AgentId, SecondmateId, TurnId};
use rusqlite::Connection;

impl SecondmateService {
    /// `close_secondmate`: the caller's own secondmate only, and only once it has no open tasks
    /// and no running turn.
    pub async fn close(&self, caller: &Caller, id: SecondmateId) -> Result<Ack, ToolFailure> {
        if self.is_working(id) {
            return Err(refuse(format!(
                "{id} is in the middle of a turn; wait for it to finish, then call \
                 close_secondmate again"
            )));
        }
        let caller_agent = caller.agent;
        let turn = caller.turn.map(|t| t.turn);
        let at = self.clock().now();
        self.db()
            .call(move |conn| close_in(conn, caller_agent, turn, id, at))
            .await
            .map_err(|e| failed(&e))??;
        self.stop_loop(id);
        // The folders go now, not at the next start: the loop is stopped and no turn runs.
        let data = self.inner.deps.data_dir.clone();
        let removed = tokio::task::spawn_blocking(move || remove_retired_folders(&data, id)).await;
        if let Ok(Err(e)) = removed {
            tracing::warn!(error = %e, "couldn't remove a retired secondmate's folders; the next start will");
        }
        Ok(Ack {
            note: retired_note(id),
        })
    }
}

fn retired_note(id: SecondmateId) -> String {
    format!(
        "{id} is retired: its session is closed and it has left the workers list. Its \
         chat history is memory-only and is lost after restart; numbers can then be reused."
    )
}

/// Removes a retired secondmate's folders: its home, its session and its plugin folder.
pub(crate) fn remove_retired_folders(
    data: &qm_types::data_dir::DataDir,
    id: SecondmateId,
) -> std::io::Result<()> {
    qm_types::data_dir::remove_path(&data.secondmate(id).root)
}

/// [`SecondmateService::close`]'s transaction.
pub(crate) fn close_in(
    conn: &mut Connection,
    caller: AgentId,
    turn: Option<TurnId>,
    id: SecondmateId,
    at: jiff::Timestamp,
) -> Result<Result<(), ToolFailure>, StoreError> {
    let tx = conn.transaction()?;
    let Some(record) = secondmates::load(&tx, id)? else {
        return Ok(Err(refuse(format!(
            "There's no active secondmate {id}; it may already be retired"
        ))));
    };
    if let Err(r) = check_own_secondmate(caller, id, record.parent) {
        return Ok(Err(r.into()));
    }
    let open = secondmates::open_tasks_of(&tx, record.agent)?;
    if !open.is_empty() {
        let list: Vec<String> = open
            .iter()
            .map(|(task, _, status)| format!("{task} ({status})"))
            .collect();
        return Ok(Err(refuse(format!(
            "{id} still has open tasks: {}. Route it a request to settle or stop them, or wait \
             until they are closed, then call close_secondmate again",
            list.join(", ")
        ))));
    }
    secondmates::retire(&tx, id, at)?;
    let summary = format!("{id} ({}) was retired", record.name);
    events::insert(
        &tx,
        &NewEvent {
            kind: event_kind::RETIRED,
            agent: Some(caller),
            task: None,
            turn,
            summary: &summary,
            payload: serde_json::json!({ "secondmate": id, "name": record.name }),
            at,
        },
    )?;
    tx.commit()?;
    Ok(Ok(()))
}
