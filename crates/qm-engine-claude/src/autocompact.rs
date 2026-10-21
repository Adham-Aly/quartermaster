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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn restart_only_when_the_window_changed() {
        assert_eq!(restart(200_000, 200_000, false, false), Restart::NotNeeded);
        assert_eq!(restart(200_000, 200_000, true, true), Restart::NotNeeded);
        assert_eq!(restart(200_000, 300_000, false, false), Restart::Now);
    }

    #[test]
    fn a_turn_or_a_job_holds_the_restart() {
        assert_eq!(restart(200_000, 300_000, true, false), Restart::Later);
        assert_eq!(restart(200_000, 300_000, false, true), Restart::Later);
        assert_eq!(restart(200_000, 300_000, true, true), Restart::Later);
    }

    #[test]
    fn settings_source_with_autocompact_on_passes() {
        let context = json!({
            "autocompactSource": "settings",
            "isAutoCompactEnabled": true,
            "autoCompactThreshold": 167_000,
        });
        assert_eq!(check(&context), Ok(()));
    }

    #[test]
    fn another_source_is_named() {
        let context = json!({"autocompactSource": "model-default", "isAutoCompactEnabled": true});
        let problem = check(&context).unwrap_err();
        assert!(problem.contains("model-default"), "{problem}");
    }

    #[test]
    fn a_missing_source_or_switch_is_reported() {
        let no_source = json!({"isAutoCompactEnabled": true});
        assert!(
            check(&no_source)
                .unwrap_err()
                .contains("didn't report where")
        );
        let no_switch = json!({"autocompactSource": "settings"});
        assert!(
            check(&no_switch)
                .unwrap_err()
                .contains("whether autocompact is on")
        );
    }

    #[test]
    fn autocompact_off_is_reported_even_from_settings() {
        let context = json!({"autocompactSource": "settings", "isAutoCompactEnabled": false});
        assert_eq!(
            check(&context),
            Err("Claude Code has autocompact turned off".into())
        );
    }

    #[test]
    fn an_error_answer_fails_the_check() {
        // `init_report` stores a failed query as `{"error": ...}`.
        let context = json!({"error": "no answer"});
        assert!(check(&context).is_err());
    }
}
