//! `grep`: content search with ripgrep's own libraries, respecting `.gitignore`.

use super::{FileToolOutcome, FileTools, elide_middle, paths};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use ignore::types::TypesBuilder;
use qm_tools::defs::files::{GrepArgs, GrepOutputMode};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The most characters a result carries; longer ones are elided in the middle.
const MAX_CHARS: usize = 30_000;

#[allow(clippy::too_many_lines)] // One pass over the walk, kept in one place.
pub(super) fn run(tools: &FileTools, args: &GrepArgs) -> FileToolOutcome {
    let cwd = &tools.config.cwd;
    let root = args
        .path
        .as_deref()
        .map_or_else(|| cwd.clone(), |p| paths::resolve(cwd, p));
    if !root.exists() {
        return FileToolOutcome::error(format!("{} does not exist", root.display()));
    }
    let multiline = args.multiline.unwrap_or(false);
    let matcher = match RegexMatcherBuilder::new()
        .case_insensitive(args.case_insensitive.unwrap_or(false))
        .multi_line(true)
        .dot_matches_new_line(multiline)
        .build(&args.pattern)
    {
        Ok(m) => m,
        Err(e) => return FileToolOutcome::error(format!("invalid pattern: {e}")),
    };
    let walker = match walker(&root, args) {
        Ok(w) => w,
        Err(e) => return FileToolOutcome::error(e),
    };
    let mode = args.output_mode.unwrap_or(GrepOutputMode::FilesWithMatches);
    let line_numbers = args.line_numbers.unwrap_or(true);
    let (before, after) = match mode {
        GrepOutputMode::Content => (
            args.before.or(args.context).unwrap_or(0) as usize,
            args.after.or(args.context).unwrap_or(0) as usize,
        ),
        _ => (0, 0),
    };
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .multi_line(multiline)
        .before_context(before)
        .after_context(after)
        .binary_detection(BinaryDetection::quit(0))
        .build();

    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    let mut entries: Vec<String> = Vec::new();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let shown = paths::display(cwd, path);
        let mut sink = Collect {
            mode,
            shown: &shown,
            line_numbers,
            out: Vec::new(),
            matched_lines: 0,
            last_line: None,
        };
        if searcher.search_path(&matcher, path, &mut sink).is_err() {
            continue;
        }
        if sink.matched_lines == 0 {
            continue;
        }
        match mode {
            GrepOutputMode::FilesWithMatches => {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                files.push((mtime, path.to_path_buf()));
            }
            GrepOutputMode::Count => entries.push(format!("{shown}:{}", sink.matched_lines)),
            GrepOutputMode::Content => {
                if !entries.is_empty() {
                    entries.push("--".to_owned());
                }
                entries.extend(sink.out);
            }
        }
    }
    if mode == GrepOutputMode::FilesWithMatches {
        files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        entries = files
            .into_iter()
            .map(|(_, p)| paths::display(cwd, &p))
            .collect();
    }
    if entries.is_empty() {
        return FileToolOutcome::ok("No matches found");
    }
    let total = entries.len();
    let mut note = String::new();
    if let Some(limit) = args.head_limit {
        let limit = limit as usize;
        if total > limit {
            entries.truncate(limit);
            note = format!("\n(showing the first {limit} of {total} results)");
        }
    }
    let header = match mode {
        GrepOutputMode::FilesWithMatches => format!("Found {total} files\n"),
        _ => String::new(),
    };
    FileToolOutcome::ok(elide_middle(
        &format!("{header}{}{note}", entries.join("\n")),
        MAX_CHARS,
    ))
}

fn walker(root: &Path, args: &GrepArgs) -> Result<ignore::Walk, String> {
    let mut builder = WalkBuilder::new(root);
    builder.sort_by_file_path(Ord::cmp);
    if let Some(glob) = &args.glob {
        let base = if root.is_dir() {
            root
        } else {
            root.parent().unwrap_or(root)
        };
        let mut overrides = OverrideBuilder::new(base);
        for pattern in split_globs(glob) {
            overrides
                .add(&pattern)
                .map_err(|e| format!("invalid glob {pattern}: {e}"))?;
        }
        builder.overrides(
            overrides
                .build()
                .map_err(|e| format!("invalid glob {glob}: {e}"))?,
        );
    }
    if let Some(file_type) = &args.file_type {
        let mut types = TypesBuilder::new();
        types.add_defaults();
        types.select(file_type);
        builder.types(
            types
                .build()
                .map_err(|e| format!("unknown file type {file_type}: {e}"))?,
        );
    }
    Ok(builder.build())
}

/// `"*.{ts,tsx}"` stays one glob; `"*.rs *.toml"` or `"*.rs,*.toml"` (outside braces) are two.
fn split_globs(glob: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for c in glob.chars() {
        match c {
            '{' => {
                depth += 1;
                current.push(c);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            ' ' | ',' if depth == 0 => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Collects one file's results.
struct Collect<'a> {
    mode: GrepOutputMode,
    shown: &'a str,
    line_numbers: bool,
    out: Vec<String>,
    matched_lines: u64,
    last_line: Option<u64>,
}

impl Collect<'_> {
    fn push(&mut self, line_number: Option<u64>, bytes: &[u8], sep: char) {
        let text = String::from_utf8_lossy(bytes);
        let first = line_number.unwrap_or(0);
        for (i, line) in text.lines().enumerate() {
            let n = first + i as u64;
            if self.last_line.is_some_and(|last| n <= last) {
                continue;
            }
            self.last_line = Some(n);
            if self.line_numbers {
                self.out.push(format!("{}{sep}{n}{sep}{line}", self.shown));
            } else {
                self.out.push(format!("{}{sep}{line}", self.shown));
            }
        }
    }
}

impl Sink for Collect<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, std::io::Error> {
        let lines = mat
            .bytes()
            .split(|&b| b == b'\n')
            .count()
            .saturating_sub(1)
            .max(1);
        self.matched_lines += lines as u64;
        match self.mode {
            GrepOutputMode::FilesWithMatches => Ok(false),
            GrepOutputMode::Count => Ok(true),
            GrepOutputMode::Content => {
                self.push(mat.line_number(), mat.bytes(), ':');
                Ok(true)
            }
        }
    }

    fn context(&mut self, _: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, std::io::Error> {
        if self.mode == GrepOutputMode::Content {
            self.push(ctx.line_number(), ctx.bytes(), '-');
        }
        Ok(true)
    }

    fn context_break(&mut self, _: &Searcher) -> Result<bool, std::io::Error> {
        if self.mode == GrepOutputMode::Content && !self.out.is_empty() {
            self.out.push("--".to_owned());
        }
        Ok(true)
    }
}
