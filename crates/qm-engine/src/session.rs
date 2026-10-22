//! The session handle: the core's only way to drive a session.

use crate::engine::EngineError;
use crate::event::EngineEvent;
use crate::tool::{ToolCallId, ToolResult};
use qm_types::ids::ModelId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::mpsc;

/// How many events an engine may get ahead of the core before it waits.
pub const EVENT_BUFFER: usize = 256;

/// What the core tells a session to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionCommand {
    /// Run one turn with this input (one turn at a time per agent).
    StartTurn { input: TurnInput },
    /// Slip this into the running turn: a background job finishing, or your direct message to a
    /// crewmate. Between turns it starts a turn instead.
    Inject { input: TurnInput },
    /// The result of the tool call with this ID.
    ToolResult {
        call: ToolCallId,
        result: ToolResult,
    },
    /// Stop the running turn and keep the session.
    Interrupt,
    /// Switch model at the next idle moment, within this engine. Moving between engines is a new
    /// session instead.
    SetModel { model: ModelId },
    /// Compact the conversation now, as you asked (`/compact`): the engine runs it as a turn of
    /// its own (rule 7 of [`EngineEvent`]'s order). Sent only between turns.
    Compact,
    /// End the session.
    Close,
    /// End the session and drop what the engine kept of it: a later resume of it can't find it.
    /// Sent when the conversation is cleared, a secondmate retires, or a task closes.
    Forget,
}

/// A turn's input: the core assembles it (context block, wakes, your message, markers) and the
/// engine passes it on as one user message.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnInput {
    pub blocks: Vec<InputBlock>,
}

impl TurnInput {
    /// A single block of text.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            blocks: vec![InputBlock::Text { text: text.into() }],
        }
    }
}

/// One block of a turn's input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputBlock {
    Text {
        text: String,
    },
    /// An image file the engine reads and attaches; models without image input never get one
    /// (routing skips them).
    Image {
        path: PathBuf,
        media_type: String,
    },
}

/// The core's end of a session.
#[derive(Debug)]
pub struct SessionHandle {
    commands: mpsc::UnboundedSender<SessionCommand>,
    events: mpsc::Receiver<EngineEvent>,
}

/// The engine's end of a session.
#[derive(Debug)]
pub struct EngineSide {
    pub commands: mpsc::UnboundedReceiver<SessionCommand>,
    pub events: mpsc::Sender<EngineEvent>,
}

/// A connected handle and engine side. Every engine builds its sessions with this.
pub fn session_channel() -> (SessionHandle, EngineSide) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::channel(EVENT_BUFFER);
    (
        SessionHandle {
            commands: cmd_tx,
            events: ev_rx,
        },
        EngineSide {
            commands: cmd_rx,
            events: ev_tx,
        },
    )
}

impl SessionHandle {
    /// Sends a command; fails with `EngineError::Closed` once the session is gone.
    pub fn send(&self, command: SessionCommand) -> Result<(), EngineError> {
        self.commands.send(command).map_err(|_| EngineError::Closed)
    }

    /// The next event; `None` after `SessionClosed` once the engine side is gone.
    pub async fn next_event(&mut self) -> Option<EngineEvent> {
        self.events.recv().await
    }

    pub fn start_turn(&self, input: TurnInput) -> Result<(), EngineError> {
        self.send(SessionCommand::StartTurn { input })
    }

    pub fn inject(&self, input: TurnInput) -> Result<(), EngineError> {
        self.send(SessionCommand::Inject { input })
    }

    pub fn tool_result(&self, call: ToolCallId, result: ToolResult) -> Result<(), EngineError> {
        self.send(SessionCommand::ToolResult { call, result })
    }

    pub fn compact(&self) -> Result<(), EngineError> {
        self.send(SessionCommand::Compact)
    }

    pub fn interrupt(&self) -> Result<(), EngineError> {
        self.send(SessionCommand::Interrupt)
    }

    pub fn close(&self) -> Result<(), EngineError> {
        self.send(SessionCommand::Close)
    }

    pub fn forget(&self) -> Result<(), EngineError> {
        self.send(SessionCommand::Forget)
    }
}
