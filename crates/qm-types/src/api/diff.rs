//! Structured, display-ready files and lines from a unified git patch.

use serde::{Deserialize, Serialize};

/// One changed file. Paths are repository-relative, decoded from git's quoting.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct DiffFile {
    /// `None` for a newly added file (or an unreadable path).
    pub old_path: Option<String>,
    /// `None` for a deleted file (or an unreadable path).
    pub new_path: Option<String>,
    pub status: DiffFileStatus,
    pub hunks: Vec<DiffHunk>,
    /// Binary patches have no text hunks; their notice/payload is kept in `metadata`.
    pub binary: bool,
    /// Original file/extended headers, binary notices/payload and unrecognised lines.
    /// Text hunk headers and body lines are represented in `hunks` instead.
    pub metadata: Vec<String>,
}

impl DiffFile {
    pub fn new(old_path: Option<String>, new_path: Option<String>) -> Self {
        Self {
            old_path,
            new_path,
            ..Self::default()
        }
    }

    /// The destination path, falling back to the source for a deletion.
    pub fn path(&self) -> Option<&str> {
        self.new_path.as_deref().or(self.old_path.as_deref())
    }
}

/// The kind of file change, independent of whether it has text hunks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiffFileStatus {
    Added,
    #[default]
    Modified,
    Deleted,
    Renamed,
    Copied,
}

/// One unified hunk. Ranges are exactly those declared by git, even if the patch was cut.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct DiffHunk {
    /// The complete `@@ -old,count +new,count @@` line, including any section heading.
    pub header: String,
    /// Only lines actually read; a truncated hunk is never padded to its declared counts.
    pub lines: Vec<DiffLine>,
    pub old_start: u64,
    pub old_count: u64,
    pub new_start: u64,
    pub new_count: u64,
}

impl DiffHunk {
    pub fn new(
        header: impl Into<String>,
        old_start: u64,
        old_count: u64,
        new_start: u64,
        new_count: u64,
    ) -> Self {
        Self {
            header: header.into(),
            old_start,
            old_count,
            new_start,
            new_count,
            ..Self::default()
        }
    }
}

/// A text line in a hunk, with separate coordinates on each side of the change.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct DiffLine {
    pub kind: DiffLineKind,
    /// The content without the patch's single leading ` `, `+` or `-`; whitespace is preserved.
    pub text: String,
    pub old_number: Option<u64>,
    pub new_number: Option<u64>,
    /// Git's `\ No newline at end of file` marker applies to this line, not a separate row.
    pub no_newline: bool,
}

impl DiffLine {
    pub fn new(kind: DiffLineKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            ..Self::default()
        }
    }
}

/// Whether a hunk line belongs to both versions, only the destination, or only the source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiffLineKind {
    #[default]
    Context,
    Addition,
    Deletion,
}
