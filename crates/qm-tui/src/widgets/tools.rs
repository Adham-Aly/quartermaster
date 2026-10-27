//! Tool calls as a session and the task pane show them: each tool's name in its family's colour
//! (reading blue, writing the code colour, shell violet, the app's own tools warn), its target
//! after it, and its result after an arrow.

use crate::theme;
use ratatui::style::Style;

/// A tool line's text split into its name, its target and its result: `Bash cargo test  → 41
/// passed` is `("Bash", "cargo test", Some("41 passed"))`.
pub(crate) fn split(text: &str) -> (&str, &str, Option<&str>) {
    let (call, result) = text
        .split_once("  → ")
        .map_or((text, None), |(call, result)| (call, Some(result.trim())));
    let (name, target) = call.split_once(' ').unwrap_or((call, ""));
    (name, target.trim(), result)
}

/// The colour of a tool's name, by what kind of tool it is. Built-in names are matched in any
/// case (Claude's `Read`, the provider engine's `read`); anything else is one of the app's own
/// tools.
pub(crate) fn style(name: &str) -> Style {
    match name.to_ascii_lowercase().as_str() {
        "read" | "grep" | "glob" | "ls" | "find" | "webfetch" | "websearch" | "toolsearch" => {
            theme::blue()
        }
        "write" | "edit" | "multiedit" | "notebookedit" => theme::code(),
        "bash" | "bashoutput" | "killshell" | "killbash" | "job_output" | "job_kill" => {
            theme::violet()
        }
        "agent" | "task" | "todowrite" | "skill" => theme::title(),
        _ => theme::warn(),
    }
}

#[cfg(test)]
mod tests {
    use super::split;

    #[test]
    fn splits_name_target_and_result() {
        assert_eq!(
            split("Bash cargo test auth  → 41 passed"),
            ("Bash", "cargo test auth", Some("41 passed"))
        );
        assert_eq!(split("Read AGENTS.md"), ("Read", "AGENTS.md", None));
        assert_eq!(split("report"), ("report", "", None));
        assert_eq!(
            split("delivery_check  → push ok · PR ok"),
            ("delivery_check", "", Some("push ok · PR ok"))
        );
    }
}
