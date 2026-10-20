//! `edit`: an exact text replacement that must match once, unless `replace_all`.

use super::write::{check_read_first, remember_written};
use super::{FileToolOutcome, FileTools, paths};
use qm_tools::defs::files::EditArgs;
use std::fmt::Write as _;

/// Lines of context shown around the changed region.
const CONTEXT: usize = 3;

pub(super) fn run(tools: &FileTools, args: &EditArgs) -> FileToolOutcome {
    let path = paths::resolve(&tools.config.cwd, &args.file_path);
    let target = match tools.policy.check(&path) {
        Ok(target) => target,
        Err(refusal) => return FileToolOutcome::error(refusal),
    };
    if !target.is_file() {
        return FileToolOutcome::error(format!(
            "{} does not exist; use write to create it",
            path.display()
        ));
    }
    if args.old_string.is_empty() {
        return FileToolOutcome::error("old_string is empty; use write to create a file");
    }
    if args.old_string == args.new_string {
        return FileToolOutcome::error("old_string and new_string are the same; nothing to change");
    }
    if let Err(refusal) = check_read_first(tools, &target, &path) {
        return FileToolOutcome::error(refusal);
    }
    let text = match std::fs::read_to_string(&target) {
        Ok(text) => text,
        Err(e) => return FileToolOutcome::error(format!("couldn't read {}: {e}", path.display())),
    };
    let count = text.matches(args.old_string.as_str()).count();
    match count {
        0 => return FileToolOutcome::error("old_string not found"),
        1 => {}
        n if !args.replace_all => {
            return FileToolOutcome::error(format!(
                "old_string occurs {n} times; add context or use replace_all"
            ));
        }
        _ => {}
    }
    let Some(first_at) = text.find(args.old_string.as_str()) else {
        return FileToolOutcome::error("old_string not found");
    };
    let updated = if args.replace_all {
        text.replace(args.old_string.as_str(), &args.new_string)
    } else {
        text.replacen(args.old_string.as_str(), &args.new_string, 1)
    };
    if let Err(e) = std::fs::write(&target, &updated) {
        return FileToolOutcome::error(format!("couldn't write {}: {e}", path.display()));
    }
    remember_written(tools, &target);

    // The first changed region, numbered like `read`.
    let start_line = text[..first_at].matches('\n').count();
    // The lines the new text occupies: a trailing newline ends its last line, it doesn't start
    // another (and a deletion occupies none).
    let new_lines = args.new_string.lines().count();
    let lines: Vec<&str> = updated.lines().collect();
    let from = start_line.saturating_sub(CONTEXT);
    let to = (start_line + new_lines + CONTEXT).min(lines.len());
    let mut out = if count > 1 {
        format!(
            "edited {}: replaced {count} occurrences; the first is now:\n",
            path.display()
        )
    } else {
        format!("edited {}:\n", path.display())
    };
    for (i, line) in lines.iter().enumerate().take(to).skip(from) {
        let _ = writeln!(out, "{:>6}\t{line}", i + 1);
    }
    out.pop();
    FileToolOutcome::ok(out)
}
