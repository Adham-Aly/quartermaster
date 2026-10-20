//! The shared vocabulary every Quartermaster crate speaks.
//!
//! This crate sits under every other one and holds only what several of them must agree on:
//!
//! - [`ids`]: display IDs (`T-0042`, `SM-02`, `C-0193`, `J3`, `SI-5`, `SC-7`), the task branch
//!   `qm/task-0042` and kept ref `refs/quartermaster/kept/T-0042`, database row IDs, commit SHAs
//!   and model IDs;
//! - [`domain`]: the app's closed sets of values (roles, kinds, tiers, the effort ladder,
//!   verification levels, delivery modes, task states, …) and a turn's [`domain::StartedBy`];
//! - [`clock`]: wall and monotonic time behind one trait, with a fake clock for watchdog ladders
//!   and limits;
//! - [`data_dir`] and [`env`](mod@env): the data directory layout and the `QUARTERMASTER_HOME`
//!   override;
//! - [`config`]: the schema of `config.toml` (no secrets);
//! - [`api`]: the messages between the core and the terminal UI;
//! - [`error`]: [`error::Refusal`], the plain reason a rule gives an agent;
//! - [`rules`]: what the rules in `qm-policy` judge: [`rules::TurnCounters`], [`rules::ToolClass`]
//!   and [`rules::HoldReason`].
//!
//! No logic beyond parsing, formatting and these small rules; no I/O beyond reading the clock and
//! the environment.

pub mod api;
pub mod clock;
pub mod config;
pub mod data_dir;
pub mod domain;
pub mod env;
pub mod error;
pub mod ids;
pub mod rules;

use std::future::Future;
use std::pin::Pin;

/// A boxed, sendable future: the return type of every object-safe async method in the workspace.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
