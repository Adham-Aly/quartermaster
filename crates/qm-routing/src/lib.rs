//! Routing: code picks the model, never an agent.
//!
//! The Quartermaster declares facts about the work; a fixed rule table turns them into a tier;
//! the app picks a (model, account) pair from your pool at that tier and an effort level; and every
//! decision is recorded with a plain-language "why". All of it is plain functions over declared
//! facts, never task text, so every row, fallback and cap is testable exhaustively.
//!
//! - [`decider`]: the `Decider` interface.
//! - [`rules`]: the embedded `routing.toml` rule table, its validation (a table mapping anything
//!   to premium is rejected) and the `builds_on` fact checks.
//! - [`fallback`]: tier fallbacks. [`rank`]: the capability check and ranking.
//! - [`effort`]: the effort level and its caps.
//! - [`later_steps`]: your model choices carried through a workflow.
//! - [`choose`]: [`choose::route`], the whole procedure, producing a [`record::RoutingRecord`].
//! - [`quartermaster_model`]: the orchestrators' own model (Auto).

pub mod choose;
pub mod decider;
pub mod effort;
pub mod fallback;
pub mod later_steps;
pub mod quartermaster_model;
pub mod rank;
pub mod record;
pub mod rules;
