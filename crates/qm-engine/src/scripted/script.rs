//! The script format (TOML).
//!
//! ```toml
//! name = "m1-scripted"
//! models = ["scripted-1"]                       # Engine::models
//!
//! [[session]]                                   # sessions are matched in order of open
//! role = "crewmate"                             # optional filters: role, agent ("12"), model
//! resumable = true                              # false → a resume gets ResumeFailed
//!
//!   [[session.turn]]
//!   expect_input_contains = "fix the bug"       # optional; a mismatch fails the turn (Internal)
//!
//!     [[session.turn.call]]
//!     errors = [{ kind = "overloaded", status = 529, message = "overloaded" }]
//!     usage = { input_tokens = 1200, output_tokens = 40 }
//!     steps = [
//!       { thinking = 45_000 },
//!       { text = "Looking at it.\nOne moment." },
//!       { tool_call = "ping", args = { text = "hello" }, expect_result_contains = "pong" },
//!     ]
//! ```
//!
//! One call is one model call. Steps, each a table with one leading key:
//!
//! | Step | Plays |
//! |---|---|
//! | `{ text = "…" }` | assistant text, committed a line at a time |
//! | `{ thinking = ms }` | silent work: `Activity { Thinking }` every 30 s of clock |
//! | `{ tool_call = "name", args = {…}, expect_result_contains = "…" }` | a call to one of our tools; waits for its result |
//! | `{ builtin = "bash", summary = "…", for_ms = ms }` | a built-in tool running, with `Activity { BuiltinTool }` |
//! | `{ job = "J1", command = "…", finish_after_ms = ms, exit_code = 0, tail = "…" }` | a background job |
//! | `{ stall = ms }` | nothing at all for that long |
//! | `{ crash = true }` | the process dies mid-turn |
//! | `{ rate_limit = "allowed_warning" }` | a rate-limit report |
//! | `{ compact = [before, after], compact_ms = ms }` | an automatic compaction: `CompactionStarted`, then `Compacted` after `compact_ms` (default 0) |
//! | `{ end_conversation = true }` | `EndConversation`: the turn ends and the session closes |
//! | `{ wait_for_inject = ms }` | waits for the next `Inject`; a timeout fails the turn |
//!
//! A call may instead (or before its steps) carry `errors` (each a retried provider error) or
//! `account_limit = { window = "five_hour", resets_in_ms = 3_600_000 }`.

use crate::event::{CallUsage, ProviderError, ProviderErrorKind, RateLimitStatus};
use qm_types::domain::Role;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// A whole script: what every session opened on the engine plays.
#[derive(Clone, Debug, PartialEq)]
pub struct Script {
    pub(super) name: String,
    pub(super) models: Vec<String>,
    pub(super) sessions: Vec<SessionScript>,
}

/// What went wrong loading a script.
#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    #[error("couldn't read the script {path}: {message}")]
    Read { path: PathBuf, message: String },
    #[error("the script isn't valid TOML: {0}")]
    Toml(String),
    #[error("the script is invalid: {0}")]
    Invalid(String),
}

impl Script {
    pub fn from_toml(text: &str) -> Result<Script, ScriptError> {
        let raw: RawScript = toml::from_str(text).map_err(|e| ScriptError::Toml(e.to_string()))?;
        raw.validate()
    }

    pub fn load(path: &Path) -> Result<Script, ScriptError> {
        let text = std::fs::read_to_string(path).map_err(|e| ScriptError::Read {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        Self::from_toml(&text)
    }

    /// The script's name, reported in every session's init report.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// One session's script.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct SessionScript {
    pub(super) role: Option<Role>,
    pub(super) agent: Option<String>,
    pub(super) model: Option<String>,
    pub(super) resumable: bool,
    pub(super) turns: Vec<TurnScript>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct TurnScript {
    pub(super) expect_input_contains: Option<String>,
    pub(super) calls: Vec<CallScript>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct CallScript {
    pub(super) errors: Vec<ProviderError>,
    pub(super) usage: CallUsage,
    pub(super) account_limit: Option<AccountLimit>,
    pub(super) steps: Vec<Step>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AccountLimit {
    pub(super) window: String,
    pub(super) resets_in_ms: u64,
}

/// One step of a model call.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Step {
    Text(String),
    Thinking {
        ms: u64,
    },
    ToolCall {
        name: String,
        args: serde_json::Value,
        expect_result_contains: Option<String>,
    },
    Builtin {
        name: String,
        summary: String,
        for_ms: u64,
    },
    Job {
        label: String,
        command: String,
        finish_after_ms: u64,
        exit_code: Option<i32>,
        tail: String,
    },
    Stall {
        ms: u64,
    },
    Crash,
    RateLimit(RateLimitStatus),
    Compact {
        before: u64,
        after: u64,
        ms: u64,
    },
    EndConversation,
    WaitForInject {
        ms: u64,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScript {
    name: String,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    session: Vec<RawSession>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSession {
    role: Option<Role>,
    agent: Option<String>,
    model: Option<String>,
    #[serde(default = "yes")]
    resumable: bool,
    #[serde(default)]
    turn: Vec<RawTurn>,
}

const fn yes() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTurn {
    expect_input_contains: Option<String>,
    #[serde(default)]
    call: Vec<RawCall>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCall {
    #[serde(default)]
    errors: Vec<RawError>,
    #[serde(default)]
    usage: RawUsage,
    account_limit: Option<AccountLimit>,
    #[serde(default)]
    steps: Vec<RawStep>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawError {
    kind: ProviderErrorKind,
    status: Option<u16>,
    message: String,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_field_names)] // The names are CallUsage's.
struct RawUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    thinking_tokens: u64,
    #[serde(default)]
    cache_read_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
}

/// Every key any step may carry; [`RawStep::into_step`] checks the combination.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    text: Option<String>,
    thinking: Option<u64>,
    tool_call: Option<String>,
    args: Option<serde_json::Value>,
    expect_result_contains: Option<String>,
    builtin: Option<String>,
    summary: Option<String>,
    for_ms: Option<u64>,
    job: Option<String>,
    command: Option<String>,
    finish_after_ms: Option<u64>,
    exit_code: Option<i32>,
    tail: Option<String>,
    stall: Option<u64>,
    crash: Option<bool>,
    rate_limit: Option<RateLimitStatus>,
    compact: Option<[u64; 2]>,
    compact_ms: Option<u64>,
    end_conversation: Option<bool>,
    wait_for_inject: Option<u64>,
}

impl RawScript {
    fn validate(self) -> Result<Script, ScriptError> {
        let mut sessions = Vec::new();
        for (s, session) in self.session.into_iter().enumerate() {
            let mut turns = Vec::new();
            for (t, turn) in session.turn.into_iter().enumerate() {
                let mut calls = Vec::new();
                for (c, call) in turn.call.into_iter().enumerate() {
                    let at = format!("session {}, turn {}, call {}", s + 1, t + 1, c + 1);
                    let steps = call
                        .steps
                        .into_iter()
                        .enumerate()
                        .map(|(i, step)| {
                            step.into_step().map_err(|e| {
                                ScriptError::Invalid(format!("{at}, step {}: {e}", i + 1))
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    calls.push(CallScript {
                        errors: call
                            .errors
                            .into_iter()
                            .map(|e| ProviderError {
                                kind: e.kind,
                                status: e.status,
                                message: e.message,
                            })
                            .collect(),
                        usage: CallUsage {
                            input_tokens: call.usage.input_tokens,
                            output_tokens: call.usage.output_tokens,
                            thinking_tokens: call.usage.thinking_tokens,
                            cache_read_tokens: call.usage.cache_read_tokens,
                            cache_write_tokens: call.usage.cache_write_tokens,
                        },
                        account_limit: call.account_limit,
                        steps,
                    });
                }
                turns.push(TurnScript {
                    expect_input_contains: turn.expect_input_contains,
                    calls,
                });
            }
            sessions.push(SessionScript {
                role: session.role,
                agent: session.agent,
                model: session.model,
                resumable: session.resumable,
                turns,
            });
        }
        Ok(Script {
            name: self.name,
            models: self.models,
            sessions,
        })
    }
}

impl RawStep {
    #[allow(clippy::too_many_lines)] // One arm per step kind, each checking its own keys.
    fn into_step(self) -> Result<Step, String> {
        let leading = [
            self.text.is_some(),
            self.thinking.is_some(),
            self.tool_call.is_some(),
            self.builtin.is_some(),
            self.job.is_some(),
            self.stall.is_some(),
            self.crash.is_some(),
            self.rate_limit.is_some(),
            self.compact.is_some(),
            self.end_conversation.is_some(),
            self.wait_for_inject.is_some(),
        ]
        .into_iter()
        .filter(|&b| b)
        .count();
        if leading != 1 {
            return Err(format!(
                "a step has exactly one of text, thinking, tool_call, builtin, job, stall, crash, \
                 rate_limit, compact, end_conversation, wait_for_inject (found {leading})"
            ));
        }
        let extras = |allowed: &[&str]| -> Result<(), String> {
            let present = [
                ("args", self.args.is_some()),
                (
                    "expect_result_contains",
                    self.expect_result_contains.is_some(),
                ),
                ("summary", self.summary.is_some()),
                ("for_ms", self.for_ms.is_some()),
                ("command", self.command.is_some()),
                ("finish_after_ms", self.finish_after_ms.is_some()),
                ("exit_code", self.exit_code.is_some()),
                ("tail", self.tail.is_some()),
                ("compact_ms", self.compact_ms.is_some()),
            ];
            match present
                .iter()
                .find(|(name, set)| *set && !allowed.contains(name))
            {
                Some((name, _)) => Err(format!("{name} doesn't belong to this step")),
                None => Ok(()),
            }
        };
        if let Some(text) = self.text.clone() {
            extras(&[])?;
            return Ok(Step::Text(text));
        }
        if let Some(ms) = self.thinking {
            extras(&[])?;
            return Ok(Step::Thinking { ms });
        }
        if let Some(name) = self.tool_call.clone() {
            extras(&["args", "expect_result_contains"])?;
            return Ok(Step::ToolCall {
                name,
                args: self
                    .args
                    .clone()
                    .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new())),
                expect_result_contains: self.expect_result_contains.clone(),
            });
        }
        if let Some(name) = self.builtin.clone() {
            extras(&["summary", "for_ms"])?;
            return Ok(Step::Builtin {
                name,
                summary: self.summary.clone().unwrap_or_default(),
                for_ms: self.for_ms.unwrap_or(0),
            });
        }
        if let Some(label) = self.job.clone() {
            extras(&["command", "finish_after_ms", "exit_code", "tail"])?;
            if qm_types::ids::JobLabel::new(label.clone()).is_err() {
                return Err(format!("job label {label:?} is empty or has whitespace"));
            }
            return Ok(Step::Job {
                label,
                command: self.command.clone().unwrap_or_default(),
                finish_after_ms: self.finish_after_ms.unwrap_or(0),
                exit_code: self.exit_code,
                tail: self.tail.clone().unwrap_or_default(),
            });
        }
        if let Some(ms) = self.stall {
            extras(&[])?;
            return Ok(Step::Stall { ms });
        }
        if let Some(crash) = self.crash {
            extras(&[])?;
            return if crash {
                Ok(Step::Crash)
            } else {
                Err("crash = false does nothing; leave the step out".into())
            };
        }
        if let Some(status) = self.rate_limit {
            extras(&[])?;
            return Ok(Step::RateLimit(status));
        }
        if let Some([before, after]) = self.compact {
            extras(&["compact_ms"])?;
            return Ok(Step::Compact {
                before,
                after,
                ms: self.compact_ms.unwrap_or(0),
            });
        }
        if let Some(end) = self.end_conversation {
            extras(&[])?;
            return if end {
                Ok(Step::EndConversation)
            } else {
                Err("end_conversation = false does nothing; leave the step out".into())
            };
        }
        if let Some(ms) = self.wait_for_inject {
            extras(&[])?;
            return Ok(Step::WaitForInject { ms });
        }
        Err("empty step".into())
    }
}
