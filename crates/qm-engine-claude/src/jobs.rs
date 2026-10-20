//! Claude Code's background shell tasks and background helper agents as our jobs.
//!
//! - `system/task_started` for a shell task (`task_type: "local_bash"`, or `is_backgrounded: true`
//!   with the `tool_use_id` of a `Bash` call) becomes `JobStarted { job: <task_id>, command:
//!   <description> }`. A backgrounded `Bash` call carries how long it had run since its
//!   `tool_use` (`ran_for_ms`): Claude reports it only once it moves it to the background.
//! - A background helper agent (`subagent_type` set, `is_backgrounded: true`; claude 2.1.304
//!   reports `task_type: "local_agent"`) becomes `JobStarted { job: helper:<task_id> }`
//!   (`JobLabel::helper`): it outlives the turn that started it, so like a shell job it keeps the
//!   process from idling out, wakes the crewmate when it finishes, and dies with the process. A
//!   helper in the foreground ends inside its turn and is no job.
//! - `system/task_notification` (or a `task_updated` reaching a final status) for a started job
//!   becomes exactly one `JobFinished`: `completed` → exit code 0, `failed` → the code parsed from
//!   the summary ("exit code N") or none, `stopped` → none; the tail is the last 20 lines (at most
//!   2,000 characters) of the task's output file, else its summary unless that only repeats the
//!   job's description (claude 2.1.304 sends a shell task's `output_file` empty and its
//!   description as the summary; a helper's summary is its final reply).
//! - A crash or a stop reports every running job finished with no exit code first.
//!
//! Whether Claude Code's job processes stay in the agent's process group, so that stopping the
//! group stops them too, isn't established against the real `claude`. The engine assumes they do
//! (they are its children and nothing moves them out), and never releases the process while a job
//! runs.

use crate::protocol::str_at;
use qm_engine::EngineEvent;
use qm_types::ids::JobLabel;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

/// The most lines of output a `JobFinished` tail carries.
const TAIL_LINES: usize = 20;
/// The most characters a `JobFinished` tail carries.
const TAIL_CHARS: usize = 2_000;

/// The session's running jobs, by Claude's task ID, with their descriptions.
#[derive(Debug, Default)]
pub struct Jobs {
    running: BTreeMap<String, (JobLabel, String)>,
    used: BTreeSet<String>,
    /// Tool-use IDs of `Bash` calls seen in this session, to recognise backgrounded ones, with
    /// when each was seen (when its command started).
    bash_calls: BTreeMap<String, Instant>,
}

impl Jobs {
    pub fn is_idle(&self) -> bool {
        self.running.is_empty()
    }

    /// Remembers a `Bash` tool use seen at `at`, so a backgrounded task naming it counts as a job
    /// that started then.
    pub fn saw_bash_call(&mut self, tool_use_id: &str, at: Instant) {
        self.bash_calls.insert(tool_use_id.to_owned(), at);
    }

    /// `system/task_started` at `now`: the `JobStarted` to send, if it is a new shell job or a
    /// new background helper agent.
    pub fn started(&mut self, msg: &Value, now: Instant) -> Option<EngineEvent> {
        let task_id = str_at(msg, "/task_id")?;
        let backgrounded = msg.get("is_backgrounded").and_then(Value::as_bool) == Some(true);
        let helper = msg
            .get("subagent_type")
            .is_some_and(|v| !v.is_null() && v.as_str() != Some(""));
        if self.used.contains(task_id) {
            return None;
        }
        let job = if helper {
            if !backgrounded {
                return None;
            }
            JobLabel::helper(&label_for(task_id)?.to_string()).ok()?
        } else {
            let shell = str_at(msg, "/task_type") == Some("local_bash")
                || (backgrounded
                    && str_at(msg, "/tool_use_id")
                        .is_some_and(|id| self.bash_calls.contains_key(id)));
            if !shell {
                return None;
            }
            label_for(task_id)?
        };
        let command = str_at(msg, "/description")
            .or_else(|| str_at(msg, "/command"))
            .unwrap_or("")
            .to_owned();
        self.used.insert(task_id.to_owned());
        self.running
            .insert(task_id.to_owned(), (job.clone(), command.clone()));
        let ran_for_ms = str_at(msg, "/tool_use_id")
            .and_then(|id| self.bash_calls.get(id))
            .map(|seen| {
                u64::try_from(now.saturating_duration_since(*seen).as_millis()).unwrap_or(u64::MAX)
            });
        Some(EngineEvent::JobStarted {
            job,
            command,
            log_path: None,
            ran_for_ms,
        })
    }

    /// `system/task_notification`, or `system/task_updated` with a final status: the
    /// `JobFinished` to send, if it ends a running job.
    pub fn notification(&mut self, msg: &Value) -> Option<EngineEvent> {
        let task_id = str_at(msg, "/task_id")?;
        let status = str_at(msg, "/status")
            .or_else(|| str_at(msg, "/patch/status"))
            .unwrap_or("");
        let final_status = matches!(
            status,
            "completed" | "failed" | "stopped" | "killed" | "cancelled" | "error"
        );
        if !final_status && str_at(msg, "/subtype") == Some("task_updated") {
            return None;
        }
        let (job, command) = self.running.remove(task_id)?;
        let summary = str_at(msg, "/summary").unwrap_or("");
        let exit_code = match status {
            "completed" => Some(0),
            "failed" | "error" => parse_exit_code(summary),
            _ => None,
        };
        let tail = str_at(msg, "/output_file")
            .filter(|p| !p.is_empty())
            .and_then(read_tail)
            .unwrap_or_else(|| {
                if summary.trim() == command.trim() {
                    String::new()
                } else {
                    summary.to_owned()
                }
            });
        Some(EngineEvent::JobFinished {
            job,
            exit_code,
            tail,
        })
    }

    /// Every running job, reported finished with no exit code (a crash or a stop).
    pub fn finish_all(&mut self) -> Vec<EngineEvent> {
        std::mem::take(&mut self.running)
            .into_values()
            .map(|(job, _)| EngineEvent::JobFinished {
                job,
                exit_code: None,
                tail: String::new(),
            })
            .collect()
    }
}

/// A valid job label for Claude's task ID (labels have no whitespace).
fn label_for(task_id: &str) -> Option<JobLabel> {
    let cleaned: String = task_id
        .chars()
        .map(|c| {
            if c.is_whitespace() || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    JobLabel::new(cleaned).ok()
}

/// "… exit code 3 …" → 3.
fn parse_exit_code(summary: &str) -> Option<i32> {
    let lower = summary.to_ascii_lowercase();
    let at = lower.find("exit code")?;
    lower[at + "exit code".len()..]
        .trim_start_matches([' ', ':'])
        .split(|c: char| !(c.is_ascii_digit() || c == '-'))
        .next()
        .and_then(|n| n.parse().ok())
}

/// The last lines of a job's output file, within the tail limits.
fn read_tail(path: &str) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(TAIL_LINES);
    let tail = lines[start..].join("\n");
    let count = tail.chars().count();
    Some(if count > TAIL_CHARS {
        tail.chars().skip(count - TAIL_CHARS).collect()
    } else {
        tail
    })
}
