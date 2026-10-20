//! `write`: creates or replaces a file, inside the write roots only.

use super::{FileToolOutcome, FileTools, paths, thousands};
use qm_tools::defs::files::WriteArgs;
use std::path::Path;

pub(super) fn run(tools: &FileTools, args: &WriteArgs) -> FileToolOutcome {
    let path = paths::resolve(&tools.config.cwd, &args.file_path);
    let target = match tools.policy.check(&path) {
        Ok(target) => target,
        Err(refusal) => return FileToolOutcome::error(refusal),
    };
    let existed = target.exists();
    if existed {
        if target.is_dir() {
            return FileToolOutcome::error(format!("{} is a folder", path.display()));
        }
        if let Err(refusal) = check_read_first(tools, &target, &path) {
            return FileToolOutcome::error(refusal);
        }
    } else if let Some(parent) = target.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return FileToolOutcome::error(format!("couldn't create {}: {e}", parent.display()));
    }
    if let Err(e) = std::fs::write(&target, &args.content) {
        return FileToolOutcome::error(format!("couldn't write {}: {e}", path.display()));
    }
    remember_written(tools, &target);
    let lines = args.content.lines().count();
    FileToolOutcome::ok(format!(
        "{} {} ({} lines)",
        if existed { "updated" } else { "created" },
        path.display(),
        thousands(lines)
    ))
}

/// An existing file may be changed only after this session read it, and only if it hasn't
/// changed since.
pub(super) fn check_read_first(
    tools: &FileTools,
    target: &Path,
    shown: &Path,
) -> Result<(), String> {
    let Some(seen) = tools.read_mtime(target) else {
        return Err(format!(
            "read {} first: an existing file is changed only after reading it",
            shown.display()
        ));
    };
    let now = std::fs::metadata(target)
        .and_then(|m| m.modified())
        .map_err(|e| format!("couldn't check {}: {e}", shown.display()))?;
    if now == seen {
        Ok(())
    } else {
        Err(format!(
            "{} changed since you read it; read it again first",
            shown.display()
        ))
    }
}

/// Records the file's new modification time, so this session may change it again.
pub(super) fn remember_written(tools: &FileTools, target: &Path) {
    if let Ok(mtime) = std::fs::metadata(target).and_then(|m| m.modified()) {
        tools.remember_read(target.to_path_buf(), mtime);
    }
}
