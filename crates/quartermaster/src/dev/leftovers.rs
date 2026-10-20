//! `quartermaster dev leftovers`: the startup leftover report.
//!
//! Checks every process group the last run recorded and never saw exit, and anything listening
//! on a closed task's ports. Reports; never kills. Prints `nothing alive` when all is clean.

use super::{Startup, dev_fail};
use crate::Failure;
use qm_core::processes::startup_leftovers;
use qm_engine::supervisor::OsSupervisor;
use qm_types::clock::SystemClock;

pub(super) async fn run(startup: &Startup) -> Result<(), Failure> {
    let supervisor = OsSupervisor::new(None);
    let report = startup_leftovers(&startup.db, &supervisor, &SystemClock)
        .await
        .map_err(|e| dev_fail(format!("the leftover check failed: {e}")))?;
    for leftover in &report.leftovers {
        let members: Vec<String> = leftover
            .members
            .iter()
            .map(|m| format!("{}:{}", m.pid, m.program))
            .collect();
        println!(
            "leftover group={} program={} owner={:?} started_at={} members={}",
            leftover.recorded.group,
            leftover.recorded.program,
            leftover.recorded.owner,
            leftover.recorded.started_at,
            members.join(",")
        );
    }
    for listener in &report.listeners {
        println!(
            "listener port={} pid={} program={}",
            listener.port, listener.pid, listener.program
        );
    }
    for problem in &report.problems {
        println!("problem {problem}");
    }
    if report.leftovers.is_empty() && report.listeners.is_empty() {
        println!("nothing alive");
    }
    Ok(())
}
