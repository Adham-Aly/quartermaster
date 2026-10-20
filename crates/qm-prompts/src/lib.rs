//! Prompts, briefs, charters and the context block.
//!
//! The prompt sources are Markdown files in the repository's `prompts/` folder, embedded at build
//! time with `include_str!` and listed in [`index::PROMPTS`]. This crate renders them: the
//! Quartermaster's and secondmates' system prompts, crewmate prompts with their instruction files,
//! briefs from the template, secondmate charters, and the per-turn context block. Rendering is
//! pure: the core passes in the facts, this crate returns text.

pub mod brief;
pub mod charter;
pub mod context_block;
pub mod index;
pub mod session_summary;
