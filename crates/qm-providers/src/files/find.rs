//! `find` (Claude Code's `Glob`): files matching a glob, newest first, respecting `.gitignore`.

use super::{FileToolOutcome, FileTools, paths};
use globset::GlobBuilder;
use ignore::WalkBuilder;
use qm_tools::defs::files::FindArgs;
use std::path::PathBuf;
use std::time::SystemTime;

const MAX_PATHS: usize = 100;

pub(super) fn run(tools: &FileTools, args: &FindArgs) -> FileToolOutcome {
    let cwd = &tools.config.cwd;
    let base = args
        .path
        .as_deref()
        .map_or_else(|| cwd.clone(), |p| paths::resolve(cwd, p));
    if !base.is_dir() {
        return FileToolOutcome::error(format!("{} is not a folder", base.display()));
    }
    let glob = match GlobBuilder::new(&args.pattern)
        .literal_separator(true)
        .build()
    {
        Ok(g) => g.compile_matcher(),
        Err(e) => return FileToolOutcome::error(format!("invalid glob: {e}")),
    };
    // A pattern without a folder part matches file names anywhere, as Claude Code's Glob does
    // for `*.rs`.
    let names_only = !args.pattern.contains('/');

    let mut found: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in WalkBuilder::new(&base).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(&base) else {
            continue;
        };
        let hit = if names_only {
            path.file_name().is_some_and(|n| glob.is_match(n))
        } else {
            glob.is_match(relative)
        };
        if hit {
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            found.push((mtime, path.to_path_buf()));
        }
    }
    if found.is_empty() {
        return FileToolOutcome::ok("No files found");
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let total = found.len();
    let mut out: Vec<String> = found
        .iter()
        .take(MAX_PATHS)
        .map(|(_, p)| paths::display(cwd, p))
        .collect();
    if total > MAX_PATHS {
        out.push(format!(
            "(showing the newest {MAX_PATHS} of {total} files; narrow the pattern or the path)"
        ));
    }
    FileToolOutcome::ok(out.join("\n"))
}
