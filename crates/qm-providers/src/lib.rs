//! The provider engine.
//!
//! Our own harness for every account that isn't a Claude subscription detected through `claude`,
//! implementing `qm_engine::Engine`. It holds:
//!
//! - the agent loop: call the model, run the tools it asked for, send back the results, repeat;
//!   our own tools go back to the core as `qm_engine::EngineEvent::ToolCall`, the file tools run
//!   here;
//! - four wire adapters (Anthropic Messages, OpenAI Responses, Chat Completions with per-provider
//!   compatibility flags, Gemini), each following its provider's replay rules;
//! - the model catalog snapshot from models.dev with our corrections file, and the mapping from the
//!   effort ladder (`qm_types::domain::Effort`) to each provider's reasoning settings;
//! - compaction and JSONL session files in the agent's `session/` folder;
//! - the crewmate's file and shell tools (`read`, `write`, `edit`, `grep`, `find`, `ls`, `bash`,
//!   `job_output`, `job_kill`), whose argument types and descriptions are defined once in
//!   `qm_tools::defs::files` and implemented here; writes go anywhere except the app's own
//!   state (`files::paths`);
//! - the crewmate's `agent` tool: helper agents, child sessions with the file and shell tools
//!   only, whose final text is the result (`helpers`).
//!
//! Credentials come from `qm-auth`.

pub mod catalog;
pub mod files;

pub mod adapters;
pub mod builtin;
pub mod builtin_files;
pub mod compaction;
pub mod engine;
pub mod helpers;
pub mod http;
pub mod limits;
pub mod model;
pub mod replay;
pub mod retry;
pub mod session_store;
pub mod sse;
pub mod transcript;

mod agent_loop;
mod session;

pub use builtin::{BuiltinCx, BuiltinOutcome, BuiltinTools, BuiltinToolsFactory, JobNotice};
pub use builtin_files::FileToolsFactory;
pub use engine::{ProviderEngine, ProviderEngineConfig};
pub use model::{BuiltinProfiles, CatalogProfiles, ModelProfile, ModelProfiles};
