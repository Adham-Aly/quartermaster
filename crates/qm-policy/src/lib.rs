//! Every code-enforced rule, as plain functions.
//!
//! A rule here takes facts as arguments and returns a decision: no I/O, no clock reads beyond a
//! `&dyn Clock` passed in, no engine. That lets each one be enforced the same way whichever engine
//! an agent runs on. Tool handlers and the core call these and turn a "no" into a
//! [`qm_types::error::Refusal`] the agent can read.
//!
//! - [`overrides`]: the one override rule.
//! - [`merge_gate`], [`delivery`]: when work may push, finish and land, and what settling means per
//!   delivery mode.
//! - [`holds`], [`repair`], [`review`]: held tasks and their fixed choices, repair counting, the
//!   review round limit.
//! - [`verification`]: choosing a task's level and changing it on a running task.
//! - [`names`], [`markers`]: commit, PR and other names, marker neutralising.
//! - [`scope`], [`base_branch`], [`github`]: scope and chain of command, base branches, the GitHub
//!   write fence.
//! - [`limits`], [`action_rule`]: the orchestrators' limits and the effective-action rule.
//! - [`watchdog`]: both watchdog ladders, on the app's `Clock`.

pub mod action_rule;
pub mod base_branch;
pub mod delivery;
pub mod github;
pub mod holds;
pub mod limits;
pub mod markers;
pub mod merge_gate;
pub mod names;
pub mod overrides;
pub mod release;
pub mod repair;
pub mod review;
pub mod scope;
pub mod verification;
pub mod watchdog;
