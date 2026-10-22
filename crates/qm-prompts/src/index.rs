//! The prompts index: every prompt source embedded in the binary.
//!
//! Add new entries at the end; never reorder. Each entry names one file under the
//! repository's `prompts/` folder and embeds it, for example `("quartermaster",
//! include_str!("../../../prompts/quartermaster.md"))`.

/// Every embedded prompt source as `(name, text)`, in the order they were added.
pub const PROMPTS: &[(&str, &str)] = &[
    // The Quartermaster's system prompt.
    (
        "quartermaster",
        include_str!("../../../prompts/quartermaster.md"),
    ),
    // The crewmate prompt, on both engines.
    ("crewmate", include_str!("../../../prompts/crewmate.md")),
    // The provider engine's stand-in for Claude Code's preset, before the crewmate prompt.
    (
        "crewmate-provider",
        include_str!("../../../prompts/crewmate-provider.md"),
    ),
    // The secondmate prompt, before its charter.
    ("secondmate", include_str!("../../../prompts/secondmate.md")),
    // A plan task's crewmate under normal autonomy, after the crewmate prompt.
    (
        "crewmate-plan-interactive",
        include_str!("../../../prompts/crewmate-plan-interactive.md"),
    ),
    // A plan task's crewmate under auto autonomy, after the crewmate prompt.
    (
        "crewmate-plan-internal",
        include_str!("../../../prompts/crewmate-plan-internal.md"),
    ),
];

/// Looks a prompt up by its name.
pub fn get(name: &str) -> Option<&'static str> {
    PROMPTS
        .iter()
        .find_map(|(n, text)| (*n == name).then_some(*text))
}
