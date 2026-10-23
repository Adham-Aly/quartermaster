//! The orchestration core.
//!
//! Tasks, agents, turns, the durable wake inbox, the scheduler and the watchdogs. The core is a
//! library with no dependency on the terminal UI: the UI talks to it only through the messages in
//! `qm_types::api`, carried in-process today and replaceable by a socket later.
//!
//! The core drives engines through the `qm_engine::Engine` trait and never names a concrete
//! engine; the binary (`crates/quartermaster`) injects them. Every tool call an agent makes comes
//! back here and is dispatched through the registry in `qm-tools`; every enforced rule is a plain
//! function in `qm-policy` or `qm-routing`; all state goes through `qm-store`'s single database
//! thread.

pub mod actions;
pub mod api;
pub mod attachments;
pub mod briefs;
pub mod config_file;
pub mod context_block;
pub mod context_monitor;
pub mod correlation;
pub mod crew_view;
pub mod decisions;
pub mod delivery;
pub mod drafts;
pub mod held;
pub mod inbox;
pub mod jobs;
pub mod markers;
pub mod notifications;
pub mod parent_channel;
pub mod plans;
pub mod processes;
pub mod projects;
pub mod reconcile;
pub mod routing;
pub mod scheduler;
pub mod schedules;
pub mod seat;
pub mod secondmates;
pub mod sessions;
pub mod settings_api;
pub mod settling;
pub mod setup_check;
pub mod skills;
pub mod tasks;
pub mod tool_host;
pub mod turn_io;
pub mod turns;
pub mod updates;
pub mod verification;
pub mod watchdogs;
