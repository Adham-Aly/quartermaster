//! The store: SQLite, the source of truth for everything the app knows.
//!
//! `rusqlite` with SQLite compiled in; migrations with `rusqlite_migration` ([`schema`]); WAL,
//! foreign keys and a busy timeout on every connection ([`connection`]); STRICT tables.
//!
//! **One database thread.** In the running app every access goes through a single database thread
//! that owns the one connection: callers send it closures over `&mut Connection` and await the
//! result, so nothing else ever touches SQLite and writes never contend. The thread lives in
//! `db.rs`, behind this contract:
//!
//! ```text
//! #[derive(Clone)] pub struct Db                     // a cheap handle; the thread stops when the last one drops
//! Db::open(path: &Path) -> Result<Db, StoreError>    // open + configure + migrate in one transaction, then spawn
//! Db::call(&self, f: FnOnce(&mut Connection) -> Result<R, StoreError>) -> impl Future<Output = Result<R, StoreError>>
//! ```
//!
//! Everything else in this crate is a plain function over a `&Connection` or `&mut Connection`,
//! called inside `Db::call`, so it is testable on a throwaway database without the thread. The
//! functions here never open their own transactions except where the contract says so (the wake
//! claim): a fact and the wake it causes are written in the caller's one transaction.
//!
//! There is one query module for each part of the app (`tasks.rs`, `agents.rs`, …).

pub mod accounts;
pub mod agents;
pub mod attachments;
pub mod connection;
pub mod context;
pub mod crew_view;
pub mod db;
pub mod decisions;
pub mod delivery;
mod error;
pub mod events;
pub mod held;
pub mod ids;
pub mod inbox;
pub mod jobs;
pub mod messages;
pub mod models;
pub mod parent_channel;
pub mod processes;
pub mod projects;
pub mod reports;
pub mod routing_decisions;
pub mod schedules;
pub mod schema;
pub mod seat;
pub mod secondmates;
pub mod seq;
pub mod sessions;
pub mod standing_instructions;
pub mod tasks;
pub mod time;
pub mod turns;
pub mod verification;
pub mod wakes;

pub use error::StoreError;
