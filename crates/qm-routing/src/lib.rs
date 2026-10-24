//! Routing: typed agent decisions within app-enforced tiers and user authority.
//!
//! The Quartermaster declares task facts; a rule table turns them into a tier. It can also
//! select the smartest model meeting all user constraints among actual tier entries via a typed
//! preference. The app validates the selection, provider, model and effort against live eligible
//! entries; an invalid preference gets ordinary routing with a note, never a tier escape.
//! Explicit user-turn model/effort overrides retain authority over preferences on later turns.
//! Every decision is recorded with a plain-language "why". A future external decision model can
//! return the same typed preference; none is configured today. The routing rules remain plain,
//! testable functions over declared facts, never task-text keyword heuristics.
//!
//! - [`decider`]: the `Decider` interface.
//! - [`rules`]: the rule table (each task kind's tier, yours or the default) and the `builds_on`
//!   fact checks.
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
