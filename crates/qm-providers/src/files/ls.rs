//! `ls`: one folder level, folders first.

use super::{FileToolOutcome, FileTools, paths};
use globset::{Glob, GlobSetBuilder};
use qm_tools::defs::files::LsArgs;

const MAX_ENTRIES: usize = 1_000;

pub(super) fn run(tools: &FileTools, args: &LsArgs) -> FileToolOutcome {
    let path = paths::resolve(&tools.config.cwd, &args.path);
    let mut set = GlobSetBuilder::new();
    for pattern in &args.ignore {
        match Glob::new(pattern) {
            Ok(g) => {
                set.add(g);
            }
            Err(e) => return FileToolOutcome::error(format!("invalid glob {pattern}: {e}")),
        }
    }
    let ignore = match set.build() {
        Ok(s) => s,
        Err(e) => return FileToolOutcome::error(format!("invalid ignore globs: {e}")),
    };
    let read = match std::fs::read_dir(&path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return FileToolOutcome::error(format!("{} does not exist", path.display()));
        }
        Err(e) => return FileToolOutcome::error(format!("couldn't list {}: {e}", path.display())),
    };
    let mut folders = Vec::new();
    let mut files = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if ignore.is_match(&name) {
            continue;
        }
        // A symlink to a folder lists as a folder.
        if entry.path().is_dir() {
            folders.push(format!("{name}/"));
        } else {
            files.push(name);
        }
    }
    folders.sort();
    files.sort();
    let total = folders.len() + files.len();
    if total == 0 {
        return FileToolOutcome::ok(format!("{} is empty.", path.display()));
    }
    let mut out: Vec<String> = folders.into_iter().chain(files).take(MAX_ENTRIES).collect();
    if total > MAX_ENTRIES {
        out.push(format!(
            "(showing {MAX_ENTRIES} of {total} entries; use find or a narrower path)"
        ));
    }
    FileToolOutcome::ok(out.join("\n"))
}
