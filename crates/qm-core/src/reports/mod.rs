//! Agent-authored static findings, independent of interactive plans.
//!
//! Answer tasks (lookup, research, reproduce, teach) always deliver `report.html`. A review
//! delivers one only under normal autonomy, where the user reads it before its supervisor acts on
//! the findings ([`crate::tasks::review`]). Under auto autonomy, as with `plan.html`, there is no
//! document: the findings go to the supervisor at once.

pub mod api;

use qm_types::data_dir::{TaskPaths, private_folder};
use qm_types::domain::{Autonomy, TaskKind};
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

pub const PAGE: &str = "report.html";
const TEMPLATE: &str = include_str!("template.html");
const SKILL: &str = include_str!("SKILL.md");
const MAX_PAGE_BYTES: u64 = 4 * 1024 * 1024;

/// The answer kinds: they deliver a human-readable document under every autonomy.
pub const fn answers_with_report(kind: TaskKind) -> bool {
    matches!(
        kind,
        TaskKind::Lookup | TaskKind::Research | TaskKind::Reproduce | TaskKind::Teach
    )
}

/// Whether a done report of `kind`, in a project at `autonomy`, needs `report.html` as well as its
/// machine-readable answer: every answer kind, and a review under normal autonomy.
pub const fn required(kind: TaskKind, autonomy: Autonomy) -> bool {
    answers_with_report(kind) || matches!((kind, autonomy), (TaskKind::Review, Autonomy::Normal))
}

/// Whether `kind` gets the template and skill when it starts. A review always does: its project's
/// autonomy can change while it runs.
pub const fn seeded(kind: TaskKind) -> bool {
    answers_with_report(kind) || matches!(kind, TaskKind::Review)
}

/// Supply a separate baseline, never overwrite a crewmate's report on resume.
pub fn prepare(paths: &TaskPaths, kind: TaskKind) -> io::Result<()> {
    if !seeded(kind) {
        return Ok(());
    }
    let dir = paths.artifacts.join("report");
    private_folder(&dir)?;
    private_folder(&dir.join("assets"))?;
    seed(&dir.join("template.html"), TEMPLATE)?;
    seed(&dir.join("SKILL.md"), SKILL)
}

fn seed(path: &Path, text: &str) -> io::Result<()> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut file) => file.write_all(text.as_bytes()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            if fs::symlink_metadata(path)?.is_file() {
                Ok(())
            } else {
                Err(io::Error::other("a report seed isn't a plain file"))
            }
        }
        Err(e) => Err(e),
    }
}

/// Check delivery structure, not the truth of findings or arbitrary HTML safety. The supplied
/// CSP and skill keep reports offline; documents are agent-authored, not rendered by the app.
pub fn check(paths: &TaskPaths) -> io::Result<()> {
    let dir = paths.artifacts.join("report");
    private_folder(&dir)?;
    let page = dir.join(PAGE);
    let meta = fs::symlink_metadata(&page)?;
    if !meta.is_file() || meta.len() > MAX_PAGE_BYTES {
        return Err(io::Error::other(
            "report.html must be a plain UTF-8 file under 4 MiB",
        ));
    }
    let text = fs::read_to_string(page)?;
    let lower = text.to_ascii_lowercase();
    if !lower.contains("<!doctype html>")
        || !lower.contains("</html>")
        || !lower.contains("<title>")
        || !lower.contains("<body")
        || lower.contains("qm-report-placeholder")
    {
        return Err(io::Error::other(
            "replace the report template's sample content with a complete HTML document",
        ));
    }
    check_assets(&dir.join("assets"), 0)
}

fn check_assets(dir: &Path, depth: usize) -> io::Result<()> {
    if depth > 16 || !fs::symlink_metadata(dir)?.is_dir() {
        return Err(io::Error::other(
            "report assets must be plain folders, at most 16 levels deep",
        ));
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            check_assets(&entry.path(), depth + 1)?;
        } else if !kind.is_file() {
            return Err(io::Error::other(
                "report assets must be plain files, not links",
            ));
        }
    }
    Ok(())
}
