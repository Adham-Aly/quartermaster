//! A forgiving parser for git's two-sided unified patches. The raw patch remains the fallback.

use qm_types::api::{DiffFile, DiffFileStatus, DiffHunk, DiffLine, DiffLineKind};

/// Parse files in patch order, retaining only the hunk lines actually present.
///
/// Git C-quoted paths are decoded (including octal UTF-8 bytes); non-UTF-8 path bytes are
/// replaced lossily because the display API uses strings. File headers and unknown metadata
/// are preserved verbatim. Read errors and preambles without a `diff --git` header yield no files.
/// The caller must retain the reader's truncation flag: a cut between files is not detectable here.
pub fn parse_patch(lines: &[String]) -> Vec<DiffFile> {
    parse_with_status(lines).0
}

/// Also report incomplete/malformed text hunks, even if a reader did not flag a cut.
pub(super) fn parse_with_status(lines: &[String]) -> (Vec<DiffFile>, bool) {
    let mut parser = Parser::default();
    for line in lines {
        parser.line(line);
    }
    parser.finish_file();
    (parser.files, parser.incomplete)
}

#[derive(Default)]
struct Parser {
    files: Vec<DiffFile>,
    file: Option<DiffFile>,
    hunk: Option<Hunk>,
    incomplete: bool,
}

impl Parser {
    fn line(&mut self, line: &str) {
        if let Some(paths) = line.strip_prefix("diff --git ") {
            self.finish_file();
            let (old, new) = header_paths(paths).unwrap_or_default();
            let mut file = DiffFile::new(old, new);
            file.metadata.push(line.to_owned());
            self.file = Some(file);
            return;
        }
        if self.file.is_none() {
            return;
        }
        if let Some(hunk) = &mut self.hunk {
            if line == "\\ No newline at end of file" {
                if let Some(previous) = hunk.view.lines.last_mut() {
                    previous.no_newline = true;
                }
                return;
            }
            // Consume content before looking for ---/+++ headers: those are valid body lines.
            if hunk.push(line) {
                return;
            }
            self.finish_hunk();
        }
        if let Some(view) = hunk_header(line) {
            self.hunk = Some(Hunk::new(view));
        } else if let Some(file) = &mut self.file {
            self.incomplete |= line.starts_with("@@ ");
            // File labels belong before the first hunk, never in trailing text or a mail footer.
            if file.hunks.is_empty() {
                file_metadata(file, line);
            }
            file.metadata.push(line.to_owned());
        }
    }

    fn finish_hunk(&mut self) {
        if let Some(hunk) = self.hunk.take() {
            self.incomplete |= hunk.old_left != 0 || hunk.new_left != 0;
            if let Some(file) = &mut self.file {
                file.hunks.push(hunk.view);
            }
        }
    }

    fn finish_file(&mut self) {
        self.finish_hunk();
        if let Some(file) = self.file.take() {
            self.files.push(file);
        }
    }
}

struct Hunk {
    view: DiffHunk,
    old_left: u64,
    new_left: u64,
}

impl Hunk {
    fn new(view: DiffHunk) -> Self {
        Self {
            old_left: view.old_count,
            new_left: view.new_count,
            view,
        }
    }

    fn push(&mut self, line: &str) -> bool {
        let (kind, old, new) = match line.as_bytes().first() {
            Some(b' ') if self.old_left > 0 && self.new_left > 0 => {
                (DiffLineKind::Context, true, true)
            }
            Some(b'+') if self.new_left > 0 => (DiffLineKind::Addition, false, true),
            Some(b'-') if self.old_left > 0 => (DiffLineKind::Deletion, true, false),
            _ => return false,
        };
        let mut parsed = DiffLine::new(kind, &line[1..]);
        if old {
            parsed.old_number = Some(self.view.old_start + (self.view.old_count - self.old_left));
            self.old_left -= 1;
        }
        if new {
            parsed.new_number = Some(self.view.new_start + (self.view.new_count - self.new_left));
            self.new_left -= 1;
        }
        self.view.lines.push(parsed);
        true
    }
}

fn hunk_header(line: &str) -> Option<DiffHunk> {
    let (ranges, _) = line.strip_prefix("@@ ")?.split_once(" @@")?;
    let (old, new) = ranges.split_once(' ')?;
    let (old_start, old_count) = range(old.strip_prefix('-')?)?;
    let (new_start, new_count) = range(new.strip_prefix('+')?)?;
    Some(DiffHunk::new(
        line, old_start, old_count, new_start, new_count,
    ))
}

fn range(text: &str) -> Option<(u64, u64)> {
    let (start, count) = text.split_once(',').unwrap_or((text, "1"));
    let number = |s: &str| {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u64>().ok())
            .flatten()
    };
    let (start, count) = (number(start)?, number(count)?);
    // Coordinates must remain representable, without allocating based on untrusted counts.
    start.checked_add(count.saturating_sub(1))?;
    Some((start, count))
}

fn file_metadata(file: &mut DiffFile, line: &str) {
    if line.starts_with("new file mode ") {
        file.status = DiffFileStatus::Added;
        file.old_path = None;
    } else if line.starts_with("deleted file mode ") {
        file.status = DiffFileStatus::Deleted;
        file.new_path = None;
    } else if let Some(path) = line.strip_prefix("--- ").and_then(label_path) {
        file.old_path = prefixed_path(path, "a/");
        if file.old_path.is_none() {
            file.status = DiffFileStatus::Added;
        }
    } else if let Some(path) = line.strip_prefix("+++ ").and_then(label_path) {
        file.new_path = prefixed_path(path, "b/");
        if file.new_path.is_none() {
            file.status = DiffFileStatus::Deleted;
        }
    } else if let Some(path) = line.strip_prefix("rename from ").and_then(decode_path) {
        file.status = DiffFileStatus::Renamed;
        file.old_path = Some(path);
    } else if let Some(path) = line.strip_prefix("rename to ").and_then(decode_path) {
        file.status = DiffFileStatus::Renamed;
        file.new_path = Some(path);
    } else if let Some(path) = line.strip_prefix("copy from ").and_then(decode_path) {
        file.status = DiffFileStatus::Copied;
        file.old_path = Some(path);
    } else if let Some(path) = line.strip_prefix("copy to ").and_then(decode_path) {
        file.status = DiffFileStatus::Copied;
        file.new_path = Some(path);
    } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
        file.binary = true;
    }
}

fn prefixed_path(path: String, prefix: &str) -> Option<String> {
    if path == "/dev/null" {
        None
    } else {
        Some(path.strip_prefix(prefix).map(str::to_owned).unwrap_or(path))
    }
}

fn label_path(text: &str) -> Option<String> {
    if text.starts_with('"') {
        quoted_path(text).map(|(path, _)| path)
    } else {
        // Git separates an unquoted label from an optional timestamp with a tab, not a space.
        Some(
            text.split_once('\t')
                .map_or(text, |(path, _)| path)
                .to_owned(),
        )
    }
}

fn header_paths(text: &str) -> Option<(Option<String>, Option<String>)> {
    let (old, new) = if text.starts_with('"') {
        let (old, rest) = quoted_path(text)?;
        (old, decode_path(rest.strip_prefix(' ')?)?)
    } else if let Some((old, _)) = text.split_once(" \"") {
        (old.to_owned(), decode_path(&text[old.len() + 1..])?)
    } else {
        // Git leaves spaces unquoted. For unchanged names (mode-only/empty/binary files),
        // choose the split whose paths agree, even when the filename itself contains " b/".
        // Rename/copy headers below authoritatively replace this fallback for differing names.
        let split = text
            .match_indices(" b/")
            .map(|(at, _)| at)
            .find(|&at| text[..at].strip_prefix("a/") == text[at + 1..].strip_prefix("b/"))
            .or_else(|| text.find(" b/"))
            .or_else(|| text.find(' '))?;
        (text[..split].to_owned(), text[split + 1..].to_owned())
    };
    Some((prefixed_path(old, "a/"), prefixed_path(new, "b/")))
}

fn decode_path(text: &str) -> Option<String> {
    if text.starts_with('"') {
        let (path, rest) = quoted_path(text)?;
        rest.is_empty().then_some(path)
    } else {
        Some(text.to_owned())
    }
}

/// Decode bytes before UTF-8 conversion: git quotes each UTF-8 byte as a separate octal escape.
fn quoted_path(text: &str) -> Option<(String, &str)> {
    let bytes = text.strip_prefix('"')?.as_bytes();
    let mut decoded = Vec::new();
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        at += 1;
        match byte {
            b'"' => {
                return Some((
                    String::from_utf8_lossy(&decoded).into_owned(),
                    &text[at + 1..],
                ));
            }
            b'\\' => {
                let escaped = *bytes.get(at)?;
                at += 1;
                decoded.push(match escaped {
                    b'a' => 7,
                    b'b' => 8,
                    b't' => b'\t',
                    b'n' => b'\n',
                    b'v' => 11,
                    b'f' => 12,
                    b'r' => b'\r',
                    b'\\' => b'\\',
                    b'"' => b'"',
                    b'0'..=b'7' => {
                        let mut value = u16::from(escaped - b'0');
                        for _ in 0..2 {
                            let Some(&digit @ b'0'..=b'7') = bytes.get(at) else {
                                break;
                            };
                            value = value * 8 + u16::from(digit - b'0');
                            at += 1;
                        }
                        u8::try_from(value).ok()?
                    }
                    _ => return None,
                });
            }
            _ => decoded.push(byte),
        }
    }
    None
}
