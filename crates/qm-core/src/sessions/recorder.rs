//! Recording a worker's session.
//!
//! Every crewmate's runner hands what it sees to the [`Recorder`] through the task service's
//! [`SessionSink`], without blocking: an unbounded channel, so nothing is lost. One task turns the
//! records into `session_lines` rows and writes them in batches (every [`BATCH_EVERY`] or
//! [`BATCH_LINES`] rows, one transaction each), then pings the UI so an open session re-renders.
//!
//! | From the runner | Line |
//! |---|---|
//! | the first turn's brief | `supervisor` "Brief · C-0042 · {title}", the brief as its detail |
//! | a reopened session | `app` with the note, the brief as its detail |
//! | a `steer` wake | `supervisor`, the message |
//! | any other wake | `app`, its `short` (or first sentence), its text as the detail |
//! | your message | `you` (unless direct intervention already wrote it) |
//! | text lines | `agent`: one row per text block, committed when the block ends; the block in progress is kept in memory and shown streaming |
//! | our tool and its result, a built-in tool | `tool`: "{name} {summary}", then "  → {result}" when known |

use crate::tasks::runner::{InputSource, SessionRecord, SessionRecordKind, SessionSink};
use crate::turn_io::ChatLog;
use jiff::Timestamp;
use qm_engine::{EngineEvent, ToolCallId};
use qm_store::db::Db;
use qm_store::sessions::{self, LineKind, NewSessionLine};
use qm_types::clock::Clock;
use qm_types::domain::WakeKind;
use qm_types::ids::{AgentId, TaskId, TurnId};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Rows wait at most this long before they are written.
pub const BATCH_EVERY: Duration = Duration::from_millis(250);
/// Rows are written at once when this many wait.
pub const BATCH_LINES: usize = 50;
/// A tool's result, as its line quotes it.
pub const TOOL_RESULT_CHARS: usize = 60;
/// An app wake's line when it has no `short`: its first sentence, cut to this.
pub const WAKE_LINE_CHARS: usize = 120;
/// The Claude engine's prefix for our tools.
const MCP_PREFIX: &str = "mcp__quartermaster__";

/// The text block a worker is writing right now, shown streaming and not yet stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Streaming {
    pub text: String,
    pub at: Timestamp,
}

type Live = Arc<Mutex<HashMap<AgentId, Streaming>>>;

/// The session recorder: the task service's sink. Cheap to clone.
#[derive(Clone)]
pub struct Recorder {
    tx: mpsc::UnboundedSender<SessionRecord>,
    live: Live,
}

impl fmt::Debug for Recorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Recorder").finish_non_exhaustive()
    }
}

impl SessionSink for Recorder {
    fn record(&self, record: SessionRecord) {
        if self.tx.send(record).is_err() {
            tracing::debug!("a session record arrived after the recorder stopped");
        }
    }
}

impl Recorder {
    /// Starts the recorder's task; `ui` is pinged after each write so an open session re-renders.
    /// Needs a Tokio runtime.
    pub fn spawn(db: Db, clock: Arc<dyn Clock>, ui: Arc<dyn ChatLog>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let live: Live = Arc::new(Mutex::new(HashMap::new()));
        let worker = Worker {
            db,
            clock,
            ui,
            live: Arc::clone(&live),
            blocks: HashMap::new(),
            tools: HashMap::new(),
            rows: Vec::new(),
            due: None,
            live_changed: false,
        };
        tokio::spawn(worker.run(rx));
        Self { tx, live }
    }

    /// The text block `agent` is writing, if any.
    pub fn streaming(&self, agent: AgentId) -> Option<Streaming> {
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&agent)
            .cloned()
    }
}

/// A text block in progress.
#[derive(Debug)]
struct Block {
    task: TaskId,
    turn: Option<TurnId>,
    lines: Vec<String>,
    at: Timestamp,
}

/// One of our tools, waiting for its result.
#[derive(Debug)]
struct PendingTool {
    call: ToolCallId,
    task: TaskId,
    turn: Option<TurnId>,
    name: String,
    args: serde_json::Value,
    at: Timestamp,
}

/// The recorder's task.
struct Worker {
    db: Db,
    clock: Arc<dyn Clock>,
    ui: Arc<dyn ChatLog>,
    live: Live,
    blocks: HashMap<AgentId, Block>,
    tools: HashMap<AgentId, PendingTool>,
    rows: Vec<NewSessionLine>,
    /// When the waiting rows (or a changed streaming block) must be written or shown.
    due: Option<Instant>,
    live_changed: bool,
}

impl Worker {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<SessionRecord>) {
        loop {
            let clock = Arc::clone(&self.clock);
            let due = self.due;
            let sleep = async move {
                match due {
                    Some(d) => clock.sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                record = rx.recv() => {
                    let Some(record) = record else {
                        self.end_all();
                        self.flush().await;
                        return;
                    };
                    self.take(record).await;
                    if self.rows.len() >= BATCH_LINES {
                        self.flush().await;
                    } else if (!self.rows.is_empty() || self.live_changed) && self.due.is_none() {
                        self.due = Some(self.clock.instant() + BATCH_EVERY);
                    }
                }
                () = sleep => self.flush().await,
            }
        }
    }

    /// Writes the waiting rows in one transaction and pings the UI.
    async fn flush(&mut self) {
        self.due = None;
        let changed = std::mem::take(&mut self.live_changed);
        if self.rows.is_empty() {
            if changed {
                self.ui.changed();
            }
            return;
        }
        let rows = std::mem::take(&mut self.rows);
        let written = self
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                for row in &rows {
                    if row.message.is_some() {
                        sessions::insert_message_once(&tx, row)?;
                    } else {
                        sessions::insert(&tx, row)?;
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .await;
        if let Err(e) = written {
            tracing::error!(error = %e, "couldn't record a worker's session lines");
        }
        self.ui.changed();
    }

    /// Turns one record into rows (or into the block in progress).
    async fn take(&mut self, record: SessionRecord) {
        let SessionRecord {
            task,
            agent,
            turn,
            at,
            kind,
        } = record;
        match kind {
            SessionRecordKind::TurnStarted { .. } => {}
            SessionRecordKind::TurnEnded { .. } => self.end(agent),
            SessionRecordKind::Input {
                source,
                text,
                wake,
                short,
                message,
            } => {
                self.end(agent);
                let line = match source {
                    InputSource::Brief => {
                        let title = self.title(task).await;
                        let mut line = NewSessionLine::new(
                            agent,
                            LineKind::Supervisor,
                            format!("Brief · {task} · {title}"),
                            at,
                        );
                        line.detail = Some(text);
                        line
                    }
                    InputSource::Reopened => {
                        let why = short.unwrap_or_else(|| "A new session started.".to_owned());
                        let mut line = NewSessionLine::new(agent, LineKind::App, why, at);
                        line.detail = Some(text);
                        line
                    }
                    InputSource::Wake if wake == Some(WakeKind::Steer) => {
                        NewSessionLine::new(agent, LineKind::Supervisor, text, at)
                    }
                    InputSource::Wake => {
                        let shown = short
                            .filter(|s| !s.trim().is_empty())
                            .unwrap_or_else(|| first_sentence(&text));
                        let mut line = NewSessionLine::new(agent, LineKind::App, shown, at);
                        line.detail = Some(text);
                        line
                    }
                    InputSource::You => {
                        let mut line = NewSessionLine::new(agent, LineKind::You, text, at);
                        line.message = message;
                        line
                    }
                };
                self.push(task, turn, line);
            }
            SessionRecordKind::Engine(ev) => self.engine(task, agent, turn, at, ev),
            SessionRecordKind::ToolResult {
                call,
                name,
                text,
                is_error,
            } => {
                let Some(tool) = self.tools.remove(&agent) else {
                    return;
                };
                if tool.call != call {
                    // Not the call waiting: write that one bare and drop this result.
                    self.write_tool(agent, tool, None);
                    return;
                }
                let result = result_summary(&name, &tool.args, &text, is_error);
                self.write_tool(agent, tool, result);
            }
        }
    }

    fn engine(
        &mut self,
        task: TaskId,
        agent: AgentId,
        turn: Option<TurnId>,
        at: Timestamp,
        ev: EngineEvent,
    ) {
        match ev {
            EngineEvent::TextLine { text } => {
                if let Some(tool) = self.tools.remove(&agent) {
                    self.write_tool(agent, tool, None);
                }
                let block = self.blocks.entry(agent).or_insert_with(|| Block {
                    task,
                    turn,
                    lines: Vec::new(),
                    at,
                });
                block.lines.push(text);
                let streaming = Streaming {
                    text: block.lines.join("\n"),
                    at: block.at,
                };
                self.live
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(agent, streaming);
                self.live_changed = true;
            }
            EngineEvent::ToolCall { call, name, args } => {
                self.end(agent);
                let name = name.strip_prefix(MCP_PREFIX).unwrap_or(&name).to_owned();
                self.tools.insert(
                    agent,
                    PendingTool {
                        call,
                        task,
                        turn,
                        name,
                        args,
                        at,
                    },
                );
            }
            EngineEvent::BuiltinToolUse { name, summary } => {
                self.end(agent);
                let text = format!("{name} {summary}").trim().to_owned();
                self.push(
                    task,
                    turn,
                    NewSessionLine::new(agent, LineKind::Tool, text, at),
                );
            }
            EngineEvent::TurnEnded { .. } | EngineEvent::SessionClosed { .. } => self.end(agent),
            _ => {}
        }
    }

    /// The task's title, for the brief's line.
    async fn title(&self, task: TaskId) -> String {
        self.db
            .call(move |conn| sessions::task_title(conn, task))
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    /// Queues a row with its task and turn.
    fn push(&mut self, task: TaskId, turn: Option<TurnId>, mut line: NewSessionLine) {
        line.task = Some(task);
        line.turn = turn;
        self.rows.push(line);
    }

    /// Ends whatever `agent` had in progress: its text block becomes a row, a tool still waiting
    /// for its result is written bare.
    fn end(&mut self, agent: AgentId) {
        if let Some(tool) = self.tools.remove(&agent) {
            self.write_tool(agent, tool, None);
        }
        if let Some(block) = self.blocks.remove(&agent) {
            let removed = self
                .live
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&agent);
            self.live_changed |= removed.is_some();
            let text = block.lines.join("\n");
            if !text.trim().is_empty() {
                let line = NewSessionLine::new(agent, LineKind::Agent, text, block.at);
                self.push(block.task, block.turn, line);
            }
        }
    }

    /// Ends every agent's block (the recorder is stopping).
    fn end_all(&mut self) {
        let agents: Vec<AgentId> = self
            .blocks
            .keys()
            .chain(self.tools.keys())
            .copied()
            .collect();
        for agent in agents {
            self.end(agent);
        }
    }

    fn write_tool(&mut self, agent: AgentId, tool: PendingTool, result: Option<String>) {
        let text = match result {
            Some(result) if !result.is_empty() => format!("{}  → {result}", tool.name),
            _ => tool.name,
        };
        self.push(
            tool.task,
            tool.turn,
            NewSessionLine::new(agent, LineKind::Tool, text, tool.at),
        );
    }
}

/// A wake's first sentence, cut to [`WAKE_LINE_CHARS`].
fn first_sentence(text: &str) -> String {
    let text = text.trim();
    let end = text.find(". ").map_or(text.len(), |i| i + 1);
    cut(&text[..end], WAKE_LINE_CHARS)
}

/// `s` cut to `max` characters, ending in `…` when cut.
fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// What a tool line says after its arrow: `delivery_check` as "push ok · PR ok · checks
/// readable", `report` as "reported done", an error or anything else by its first line.
fn result_summary(
    name: &str,
    args: &serde_json::Value,
    text: &str,
    is_error: bool,
) -> Option<String> {
    let first_line = || {
        text.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(|l| cut(l, TOOL_RESULT_CHARS))
    };
    if is_error {
        return first_line();
    }
    let json: Option<serde_json::Value> = serde_json::from_str(text).ok();
    match name {
        "delivery_check" => {
            let json = json?;
            let word = |key: &str| json.get(key).and_then(serde_json::Value::as_str);
            let mut parts = Vec::new();
            for (key, label, ok, missing) in [
                ("push", "push", "push ok", "no push"),
                ("pull_request", "PR", "PR ok", "no PR"),
                ("checks", "checks", "checks readable", "checks unreadable"),
            ] {
                match word(key) {
                    Some("ok") => parts.push(ok.to_owned()),
                    Some("missing") => parts.push(missing.to_owned()),
                    Some("not_needed") | None => {}
                    Some(other) => parts.push(format!("{label} {other}")),
                }
            }
            Some(cut(&parts.join(" · "), TOOL_RESULT_CHARS))
        }
        "report" => {
            let outcome = args.get("outcome").and_then(serde_json::Value::as_str);
            Some(match outcome {
                Some(outcome) => format!("reported {}", outcome.replace('_', " ")),
                None => "reported".to_owned(),
            })
        }
        _ => match json
            .as_ref()
            .and_then(|j| j.get("note"))
            .and_then(serde_json::Value::as_str)
        {
            Some(note) => Some(cut(note.lines().next().unwrap_or(note), TOOL_RESULT_CHARS)),
            None => first_line(),
        },
    }
}
