//! `read`: a file's lines numbered as `cat -n` does, a range of them, or an image.

use super::{FileToolOutcome, FileTools, paths, thousands};
use qm_tools::defs::files::ReadArgs;
use std::fmt::Write as _;

const DEFAULT_LIMIT: usize = 2_000;
const MAX_LINE_CHARS: usize = 2_000;

pub(super) fn run(tools: &FileTools, args: &ReadArgs) -> FileToolOutcome {
    let path = paths::resolve(&tools.config.cwd, &args.file_path);
    let meta = match std::fs::metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return FileToolOutcome::error(format!("{} does not exist", path.display()));
        }
        Err(e) => return FileToolOutcome::error(format!("couldn't read {}: {e}", path.display())),
    };
    if meta.is_dir() {
        return FileToolOutcome::error(format!(
            "{} is a folder; use ls to list it",
            path.display()
        ));
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => return FileToolOutcome::error(format!("couldn't read {}: {e}", path.display())),
    };
    let remember = || {
        if let (Ok(real), Ok(mtime)) = (path.canonicalize(), meta.modified()) {
            tools.remember_read(real, mtime);
        }
    };

    if let Some(media_type) = image_type(&bytes) {
        remember();
        let mut out = FileToolOutcome::ok(format!(
            "{} is an image ({media_type}, {} bytes), attached.",
            path.display(),
            thousands(bytes.len())
        ));
        out.images.push((media_type.to_owned(), bytes));
        return out;
    }
    if bytes.starts_with(b"%PDF") {
        return FileToolOutcome::error(format!(
            "{} is a PDF, which read can't show; use bash with a converter such as `pdftotext \
             file.pdf -` to read its text",
            path.display()
        ));
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return FileToolOutcome::error(format!(
            "{} is a binary file, which read can't show; inspect it with bash (`file`, `xxd | \
             head`)",
            path.display()
        ));
    };
    if text.as_bytes().iter().take(8_192).any(|&b| b == 0) {
        return FileToolOutcome::error(format!(
            "{} is a binary file, which read can't show; inspect it with bash (`file`, `xxd | \
             head`)",
            path.display()
        ));
    }
    remember();
    if text.is_empty() {
        return FileToolOutcome::ok(format!("{} is empty.", path.display()));
    }

    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let first = args.offset.map_or(1, |o| (o as usize).max(1));
    let limit = args.limit.map_or(DEFAULT_LIMIT, |l| (l as usize).max(1));
    if first > total {
        return FileToolOutcome::error(format!(
            "{} has {} lines; offset {first} is past the end",
            path.display(),
            thousands(total)
        ));
    }
    let last = (first - 1 + limit).min(total);
    let mut out = String::new();
    for (i, line) in lines[first - 1..last].iter().enumerate() {
        let n = first + i;
        if line.chars().count() > MAX_LINE_CHARS {
            let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
            let _ = writeln!(
                out,
                "{n:>6}\t{cut}… (line cut at {} characters)",
                thousands(MAX_LINE_CHARS)
            );
        } else {
            let _ = writeln!(out, "{n:>6}\t{line}");
        }
    }
    if first > 1 || last < total {
        let _ = write!(
            out,
            "\n(lines {first}–{last} of {}; use offset and limit to read more)",
            thousands(total)
        );
    } else {
        out.pop();
    }
    FileToolOutcome::ok(out)
}

/// The media type of a PNG, JPEG, GIF or WebP file, by its magic bytes.
fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}
