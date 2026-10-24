//! Pair consecutive deletion/addition runs without inventing line numbers for empty sides.

use qm_types::api::{DiffFile, DiffLine, DiffLineKind};

pub(super) enum Row<'a> {
    Hunk(&'a str),
    Code {
        old: Option<&'a DiffLine>,
        new: Option<&'a DiffLine>,
    },
    NoNewline {
        old: bool,
        new: bool,
    },
    Message(&'a str),
}

pub(super) fn rows(file: &DiffFile, partial: bool) -> Vec<Row<'_>> {
    let mut rows = Vec::new();
    for metadata in &file.metadata {
        if metadata.starts_with("old mode ") || metadata.starts_with("new mode ") {
            rows.push(Row::Message(metadata));
        }
    }
    if file.binary {
        rows.push(Row::Message("Binary file changed · no text preview"));
    } else if file.hunks.is_empty() {
        rows.push(Row::Message(match file.status {
            _ if partial => "No text hunks available in this partial diff",
            qm_types::api::DiffFileStatus::Added => "Empty file added",
            qm_types::api::DiffFileStatus::Deleted => "Empty file deleted",
            _ => "No textual changes · file metadata only",
        }));
    }
    for hunk in &file.hunks {
        rows.push(Row::Hunk(&hunk.header));
        let mut lines = hunk.lines.as_slice();
        while let Some(line) = lines.first() {
            if line.kind == DiffLineKind::Context {
                push_code(&mut rows, Some(line), Some(line));
                lines = &lines[1..];
            } else {
                let end = lines
                    .iter()
                    .position(|l| l.kind == DiffLineKind::Context)
                    .unwrap_or(lines.len());
                let group = &lines[..end];
                let old: Vec<_> = group
                    .iter()
                    .filter(|l| l.kind == DiffLineKind::Deletion)
                    .collect();
                let new: Vec<_> = group
                    .iter()
                    .filter(|l| l.kind == DiffLineKind::Addition)
                    .collect();
                for index in 0..old.len().max(new.len()) {
                    push_code(&mut rows, old.get(index).copied(), new.get(index).copied());
                }
                lines = &lines[end..];
            }
        }
    }
    rows
}

fn push_code<'a>(rows: &mut Vec<Row<'a>>, old: Option<&'a DiffLine>, new: Option<&'a DiffLine>) {
    rows.push(Row::Code { old, new });
    let (old, new) = (
        old.is_some_and(|l| l.no_newline),
        new.is_some_and(|l| l.no_newline),
    );
    if old || new {
        rows.push(Row::NoNewline { old, new });
    }
}
