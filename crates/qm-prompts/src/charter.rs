//! A secondmate's charter and its system prompt.
//!
//! **The charter** persists on the secondmate's record (`secondmates`, `secondmate_projects`):
//! its scope words, its project list, and its standing base branch if it has one. The plan:
//! "Its charter, which persists: scope words, project list, and its standing base branch if it has
//! one", and the standing branch is "a typed field on the secondmate's record, not just prose in
//! its charter … and the charter text shows it". [`render_charter`] is that text. The project list
//! is derived from the folders its open crewmates work in and restricts nothing; the app fills the
//! base branch in itself; the charter only tells the secondmate.
//!
//! **The system prompt** ([`secondmate_prompt`]) is the static `prompts/secondmate.md` with the
//! charter added after it ("As the Quartermaster, with its charter added to the prompt"). It is
//! rendered when the secondmate's session opens. A charter changed later reaches a running session
//! as a `charter_changed` app wake carrying the new text, and the next session's prompt carries it
//! from the start.
//!
//! Every string that didn't come from this module (the name, the scope words, project names, the
//! branch) is neutralised with [`qm_policy::markers::neutralise`], so nothing in a charter can
//! open or close a marker, whatever the caller passed. Rendering is pure: the core passes in the
//! facts, this module returns text.

use std::fmt::Write as _;

use qm_policy::markers::neutralise;
use qm_types::ids::SecondmateId;

use crate::index;

/// The most characters of scope words a charter carries. `start_secondmate` and
/// `configure_secondmate` refuse longer scope words; the renderer cuts at the same length as a
/// backstop, so the prompt budget holds whatever is stored.
pub const SCOPE_CHARS: usize = 300;

/// The most projects a charter lists by name; the rest are counted.
pub const LISTED_PROJECTS: usize = 20;

/// Everything a charter states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CharterFacts<'a> {
    pub id: SecondmateId,
    /// The secondmate's short name, e.g. "web-app".
    pub name: &'a str,
    /// The scope words, e.g. "web-app frontend".
    pub scope: &'a str,
    /// The projects its open crewmates work in, by name. They refuse nothing.
    pub projects: &'a [&'a str],
    /// The standing base branch, if one is set.
    pub standing_branch: Option<&'a str>,
}

/// The charter as the secondmate reads it: a `# Your charter` section, for example
///
/// ```text
/// # Your charter · SM-02 · web-app
///
/// - Scope: web-app frontend
/// - Projects: web-app, design-system, where your crewmates work now. They may work in any folder.
/// - Standing base branch: release/2.4. Your crewmates start from it unless a request names
///   another branch.
/// ```
pub fn render_charter(f: &CharterFacts<'_>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Your charter · {} · {}", f.id, one_line(f.name));
    out.push('\n');
    let _ = writeln!(out, "- Scope: {}", cut(&one_line(f.scope), SCOPE_CHARS));
    out.push_str(&projects_line(f.projects));
    out.push('\n');
    match f.standing_branch {
        Some(branch) => {
            let _ = writeln!(
                out,
                "- Standing base branch: {}. Your crewmates start from it unless a request names \
                 another branch.",
                one_line(branch)
            );
        }
        None => out.push_str(
            "- Standing base branch: none. Your crewmates start from each project's default \
             branch unless a request names another.\n",
        ),
    }
    out
}

/// The secondmate's whole system prompt: `prompts/secondmate.md`, then the charter.
pub fn secondmate_prompt(f: &CharterFacts<'_>) -> String {
    let base = index::get("secondmate").unwrap_or_default().trim_end();
    format!("{base}\n\n{}", render_charter(f))
}

fn projects_line(projects: &[&str]) -> String {
    if projects.is_empty() {
        return "- Projects: none yet. Your crewmates may work in any folder; your scope words decide what fits.".to_owned();
    }
    let mut names: Vec<String> = projects
        .iter()
        .take(LISTED_PROJECTS)
        .map(|p| one_line(p))
        .collect();
    if projects.len() > LISTED_PROJECTS {
        names.push(format!("and {} more", projects.len() - LISTED_PROJECTS));
    }
    format!(
        "- Projects: {}, where your crewmates work now. They may work in any folder.",
        names.join(", ")
    )
}

/// Neutralised, on one line: line breaks and runs of whitespace become single spaces.
fn one_line(text: &str) -> String {
    neutralise(&text.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// At most `max` characters, ending in "…" when cut.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut kept: String = text.chars().take(max.saturating_sub(1)).collect();
    kept.push('…');
    kept
}
