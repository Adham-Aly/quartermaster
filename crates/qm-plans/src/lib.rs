//! Planner-authored user-facing HTML, separate from the technical `plan.md`.
//! [`authoring_template`], [`check_html`] and [`compose_html`] define the authoring contract.
//! Composition preserves authored markup and adds the trusted runtime and controls.
//! Legacy Markdown [`check`] and [`render`] remain available for existing callers.

mod authored;
mod highlight;
pub use authored::{authoring_template, check_html, compose_html};
pub mod markdown;
mod page;
pub mod question;
pub mod reply;
pub mod svg;
mod vendor;

use qm_types::domain::PlanMode;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

pub use question::{Choice, Question};

/// The technical agent plan, in the plan folder.
pub const PLAN_FILE: &str = "plan.md";
/// The planner-authored user-facing document (interactive plans only).
pub const PAGE_FILE: &str = "plan.html";
/// The folder for the plan's images, inside the plan folder.
pub const ASSETS_DIR: &str = "assets";
/// The largest `plan.md` accepted.
pub const MAX_PLAN_BYTES: usize = 256 * 1024;
/// The most questions one round may ask.
pub const MAX_QUESTIONS: usize = 60;
/// The most choices one question may offer (the same as `ask_user`).
pub const MAX_CHOICES: usize = 9;
/// The most bytes all assets together may hold.
pub const MAX_ASSET_BYTES: u64 = 50 * 1024 * 1024;
/// The required heading of an interactive plan: what the planner still assumes.
pub const ASSUMPTIONS_ID: &str = "assumptions";
/// The required heading of an internal plan: what the planner decided on its own.
pub const DECISIONS_ID: &str = "decisions-made-without-the-user";

/// Words that belong to the interactive question flow only. None may appear in anything an
/// internal (auto) planner is shown: its brief section, prompt addendum, skill, tool descriptions.
pub const INTERACTIVE_ONLY_MARKERS: &[&str] = &[
    "```question",
    "plan-reply",
    "assumptions",
    "round",
    "page",
    "grill",
];

/// The first interactive-only marker in `text`, matched case-insensitively as a whole word (so
/// "around" doesn't match "round").
pub fn interactive_marker(text: &str) -> Option<&'static str> {
    let lower = text.to_lowercase();
    INTERACTIVE_ONLY_MARKERS.iter().copied().find(|marker| {
        lower.match_indices(marker).any(|(at, found)| {
            let word = |c: char| c.is_alphanumeric() || c == '_';
            let before = lower[..at].chars().next_back();
            let after = lower[at + found.len()..].chars().next();
            // A marker that starts or ends with punctuation (```question) needs no boundary there.
            let open = !marker.starts_with(word) || !before.is_some_and(word);
            let close = !marker.ends_with(word) || !after.is_some_and(word);
            open && close
        })
    })
}

/// The plan-format skill for a mode, written into the planner's skills as `quartermaster-plan`.
pub const fn skill(mode: PlanMode) -> &'static str {
    match mode {
        PlanMode::Interactive => include_str!("../skill/interactive.md"),
        PlanMode::Internal => include_str!("../skill/internal.md"),
    }
}

/// One thing wrong with a plan, with its line in `plan.md` when it has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub line: Option<usize>,
    pub message: String,
}

impl Problem {
    pub(crate) fn at(line: usize, message: impl Into<String>) -> Self {
        Self {
            line: Some(line),
            message: message.into(),
        }
    }

    pub(crate) fn whole(message: impl Into<String>) -> Self {
        Self {
            line: None,
            message: message.into(),
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "plan.md line {line}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// Everything wrong with a plan, in document order. Shown to the planner whole, so it fixes them
/// in one pass.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub struct Problems(pub Vec<Problem>);

impl fmt::Display for Problems {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, problem) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("\n")?;
            }
            write!(f, "- {problem}")?;
        }
        Ok(())
    }
}

/// The plan's images, by their path in `plan.md` (`assets/flow.svg`), in name order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Assets {
    files: BTreeMap<String, Vec<u8>>,
}

/// The image types a plan may use, by extension.
pub(crate) const IMAGE_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("svg", "image/svg+xml"),
];

impl Assets {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an asset by its path in `plan.md`.
    pub fn insert(&mut self, name: impl Into<String>, bytes: Vec<u8>) {
        self.files.insert(name.into(), bytes);
    }

    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.files.get(name).map(Vec::as_slice)
    }

    /// Every asset, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.files.iter().map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    /// Reads `<plan folder>/assets/`: regular files only (a symlink or a folder is skipped, never
    /// followed), image extensions only, at most [`MAX_ASSET_BYTES`] together.
    pub fn load(plan_dir: &Path) -> Result<Self, Problems> {
        let mut assets = Self::new();
        let dir = plan_dir.join(ASSETS_DIR);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(assets),
            Err(e) => {
                return Err(Problems(vec![Problem::whole(format!(
                    "couldn't read the assets folder: {e}"
                ))]));
            }
        };
        let mut problems = Vec::new();
        let mut total: u64 = 0;
        let mut names: Vec<_> = entries.filter_map(Result::ok).collect();
        names.sort_by_key(std::fs::DirEntry::file_name);
        for entry in names {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if !meta.file_type().is_file() || image_type(&name).is_none() {
                continue;
            }
            total += meta.len();
            if total > MAX_ASSET_BYTES {
                problems.push(Problem::whole(format!(
                    "the assets are over {} MiB together; make the images smaller",
                    MAX_ASSET_BYTES / 1024 / 1024
                )));
                break;
            }
            match std::fs::read(entry.path()) {
                Ok(bytes) => assets.insert(format!("{ASSETS_DIR}/{name}"), bytes),
                Err(e) => problems.push(Problem::whole(format!("couldn't read {name}: {e}"))),
            }
        }
        if problems.is_empty() {
            Ok(assets)
        } else {
            Err(Problems(problems))
        }
    }
}

/// The MIME type of an image name, by its extension.
pub(crate) fn image_type(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    IMAGE_TYPES
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, mime)| *mime)
}

/// What the rules for a report need to know besides the plan itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rules {
    /// The live mode, from the project's autonomy now.
    pub mode: PlanMode,
    /// The round this check is for, from 1.
    pub round: u32,
    /// How many rounds the user has answered.
    pub user_rounds: u32,
    /// The user approved and asked the planner to take its recommendations.
    pub take_recommended: bool,
}

/// An h2 or h3 section: its heading id, its title, and its fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub id: String,
    pub title: String,
    pub level: u8,
    /// Over the section's Markdown and the bytes of every asset it shows.
    pub fingerprint: String,
}

/// What a valid plan holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checked {
    /// The H1, if any.
    pub title: Option<String>,
    pub questions: Vec<Question>,
    pub sections: Vec<Section>,
    pub images: usize,
    pub diagrams: usize,
    pub mockups: usize,
    /// What doesn't stop the plan but the planner should know: parts of a drawing the cleaner
    /// dropped.
    pub notes: Vec<Problem>,
}

impl Checked {
    /// The sections whose fingerprint differs from `previous` (a new section counts too).
    pub fn updated(&self, previous: &BTreeMap<String, String>) -> Vec<&Section> {
        if previous.is_empty() {
            return Vec::new();
        }
        self.sections
            .iter()
            .filter(|s| previous.get(&s.id) != Some(&s.fingerprint))
            .collect()
    }

    /// Section id → fingerprint, to hand to the next round's [`Meta::previous`].
    pub fn fingerprints(&self) -> BTreeMap<String, String> {
        self.sections
            .iter()
            .map(|s| (s.id.clone(), s.fingerprint.clone()))
            .collect()
    }

    /// One line for the planner: "3 questions open (a, b, c) · 2 diagrams, 1 mockup, 3 images".
    pub fn summary(&self) -> String {
        let questions = match self.questions.len() {
            0 => "no questions open".to_owned(),
            n => format!(
                "{n} question{} open ({})",
                if n == 1 { "" } else { "s" },
                self.questions
                    .iter()
                    .map(|q| q.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        let count = |n: usize, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
        format!(
            "{questions} · {}, {}, {}",
            count(self.diagrams, "diagram"),
            count(self.mockups, "mockup"),
            count(self.images, "image")
        )
    }
}

/// Checks the technical agent handoff without imposing user-review round rules.
/// Interactive questions belong in `plan.html`, not this Markdown document.
/// For internal plans use [`check`] with internal rules to require the decisions record too.
pub fn check_technical(md: &str, assets: &Assets) -> Result<Checked, Problems> {
    markdown::parse(md, assets, PlanMode::Internal).map(|parsed| parsed.checked)
}

/// Checks legacy `plan.md` against the format and the mode's rules.
pub fn check(md: &str, assets: &Assets, rules: &Rules) -> Result<Checked, Problems> {
    let parsed = markdown::parse(md, assets, rules.mode)?;
    check_rules(parsed.checked, rules)
}

pub(crate) fn check_rules(checked: Checked, rules: &Rules) -> Result<Checked, Problems> {
    let mut problems = Vec::new();
    let has = |id: &str| checked.sections.iter().any(|s| s.id == id);
    match rules.mode {
        PlanMode::Interactive => {
            if rules.round <= 1 && checked.questions.is_empty() {
                problems.push(Problem::whole(
                    "under normal autonomy the user goes through the plan's open points first: \
                     put your assumptions to them as question blocks",
                ));
            }
            if checked.questions.is_empty() && rules.user_rounds == 0 && !rules.take_recommended {
                problems.push(Problem::whole(
                    "the plan can't be final before the user has answered a round: keep asking \
                     until they have",
                ));
            }
            if !has(ASSUMPTIONS_ID) {
                problems.push(Problem::whole(
                    "add a \"## Assumptions\" section listing what you still assume (it may be \
                     empty once the user has confirmed everything)",
                ));
            }
        }
        PlanMode::Internal => {
            if !has(DECISIONS_ID) {
                problems.push(Problem::whole(
                    "add a \"## Decisions made without the user\" section listing each open point \
                     you decided and why (write \"None\" if there were none)",
                ));
            }
        }
    }
    if problems.is_empty() {
        Ok(checked)
    } else {
        Err(Problems(problems))
    }
}

/// Where a plan stands, for its page's banner and widgets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageState {
    /// Questions are open; the user answers them in the page.
    Open,
    /// No questions are open; the user may still suggest changes, then approves.
    Final,
    /// Approved, at this local time (shown as given).
    Approved { at: String },
    /// The page takes no more replies, for this reason ("Autonomy changed to auto").
    Locked { reason: String },
}

/// The path the app's plan endpoint answers on, on `127.0.0.1:<port>`.
pub const SUBMIT_PATH: &str = "/v1/plan-reply";

/// Where the page sends its reply: the app's endpoint on this Mac, and the round's key.
#[derive(Clone, PartialEq, Eq)]
pub struct Submit {
    pub port: u16,
    /// The round's capability token, 64 hex digits. Never logged, so `Debug` leaves it out.
    pub token: String,
}

impl Submit {
    /// `http://127.0.0.1:<port>`, the page's only `connect-src`.
    pub fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The URL the page posts to.
    pub fn url(&self) -> String {
        format!("{}{SUBMIT_PATH}", self.origin())
    }
}

impl fmt::Debug for Submit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Submit")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

/// What the page shows besides `plan.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// `C-0042`.
    pub task: String,
    /// The plan's random id, 24 hex digits: the key for the reply and the browser's storage.
    pub plan_uid: String,
    /// The round this page is for, from 1.
    pub round: u32,
    pub project: String,
    pub state: PageState,
    /// The previous round's section fingerprints, for "updated" badges.
    pub previous: BTreeMap<String, String>,
    /// Where Send to Quartermaster posts; `None` leaves the page with Copy reply only. Used only
    /// while the page takes replies ([`PageState::Open`] or [`PageState::Final`]).
    pub submit: Option<Submit>,
}

impl Meta {
    /// The endpoint the page may send to: [`Meta::submit`] while the page takes replies.
    pub(crate) fn sends_to(&self) -> Option<&Submit> {
        match self.state {
            PageState::Open | PageState::Final => self.submit.as_ref(),
            PageState::Approved { .. } | PageState::Locked { .. } => None,
        }
    }
}

/// A rendered page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    pub html: String,
    pub checked: Checked,
}

/// Renders an interactive plan's page. The caller checks the plan first ([`check`]); this
/// refuses only what it can't draw.
pub fn render(md: &str, assets: &Assets, meta: &Meta) -> Result<Rendered, Problems> {
    let parsed = markdown::parse(md, assets, PlanMode::Interactive)?;
    let html = page::assemble(&parsed, meta);
    Ok(Rendered {
        html,
        checked: parsed.checked,
    })
}

/// The sha256 of `plan.md` and every asset's name and bytes, in hex: what a page was rendered
/// from, so the app can tell whether re-rendering it would show the user something new.
pub fn digest(md: &str, assets: &Assets) -> String {
    let mut hasher = Sha256::new();
    let mut part = |bytes: &[u8]| {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    part(md.as_bytes());
    for (name, bytes) in assets.iter() {
        part(name.as_bytes());
        part(bytes);
    }
    hex(&hasher.finalize())
}

/// A short hex digest (16 digits) of `parts`, each length-prefixed so boundaries count.
pub(crate) fn fingerprint<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    hex(&hasher.finalize()[..8])
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Escapes text for HTML content and double-quoted attributes.
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests;
