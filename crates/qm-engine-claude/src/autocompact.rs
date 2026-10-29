//! The autocompact window, as a running `claude` reports it.
//!
//! The window reaches `claude` at launch, in the `--settings` value ([`crate::argv::settings_json`]).
//! A running `claude` keeps the window it launched with, even when `apply_flag_settings` changes it,
//! so a change needs a restart with `--resume` ([`restart`] says when). [`check`] reads what
//! `get_context_usage` reports after the launch: the window must be the source of the trigger
//! point, and autocompact must be on.

use crate::protocol::str_at;
use serde_json::Value;

/// What to do about a changed autocompact window for a running `claude`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restart {
    /// The process runs with the current window.
    NotNeeded,
    /// The window changed, but a turn or a background job is running: at the next idle moment.
    Later,
    /// The window changed and the agent is idle with no job: restart now.
    Now,
}

/// Whether a running `claude` launched with `launched` must restart to run with `current`.
/// Restarting stops its background shells, so a turn or a job always waits.
pub fn restart(launched: u32, current: u32, in_turn: bool, jobs_running: bool) -> Restart {
    if launched == current {
        Restart::NotNeeded
    } else if in_turn || jobs_running {
        Restart::Later
    } else {
        Restart::Now
    }
}

/// The start-up check: `get_context_usage` must name the settings flag as the source of the
/// autocompact window, with autocompact on. The sentence says what was found otherwise.
pub fn check(context: &Value) -> Result<(), String> {
    match str_at(context, "/autocompactSource") {
        Some("settings") => {}
        Some(source) => {
            return Err(format!(
                "Claude Code takes its autocompact window from {source}, not from our settings flag"
            ));
        }
        None => {
            return Err("Claude Code didn't report where its autocompact window comes from".into());
        }
    }
    match context.get("isAutoCompactEnabled").and_then(Value::as_bool) {
        Some(true) => Ok(()),
        Some(false) => Err("Claude Code has autocompact turned off".into()),
        None => Err("Claude Code didn't report whether autocompact is on".into()),
    }
}
