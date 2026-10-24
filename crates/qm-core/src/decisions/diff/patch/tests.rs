use super::*;
use crate::decisions::diff::{RawDiff, fill_view};
use qm_types::api::{DiffView, Tone};
use qm_types::ids::HeldActionId;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_owned).collect()
}

fn parse(text: &str) -> Vec<DiffFile> {
    let (files, incomplete) = parse_with_status(&lines(text));
    assert!(!incomplete, "complete fixture: {text}");
    files
}

#[test]
fn multiple_hunks_number_each_side_and_preserve_header_like_content_and_whitespace() {
    let files = parse(concat!(
        "diff --git a/src/a.rs b/src/a.rs\n",
        "index abc..def 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n",
        "@@ -10,4 +20,5 @@ fn example()\n",
        " context one\n--- content\n+++ content\n-  old  \n+  new  \n+\n context last\n",
        "@@ -40 +51 @@ another section\n-old\n\\ No newline at end of file\n",
        "+new\n\\ No newline at end of file\n",
        "diff --git a/next b/next\nold mode 100644\nnew mode 100755\n",
    ));
    assert_eq!(files.len(), 2);
    let file = &files[0];
    assert_eq!(file.path(), Some("src/a.rs"));
    assert_eq!(file.status, DiffFileStatus::Modified);
    assert_eq!(file.metadata.len(), 4);
    assert_eq!(file.hunks.len(), 2);
    let first = &file.hunks[0];
    assert_eq!(first.header, "@@ -10,4 +20,5 @@ fn example()");
    assert_eq!(
        (
            first.old_start,
            first.old_count,
            first.new_start,
            first.new_count
        ),
        (10, 4, 20, 5)
    );
    let actual: Vec<_> = first
        .lines
        .iter()
        .map(|line| {
            (
                line.kind,
                line.text.as_str(),
                line.old_number,
                line.new_number,
                line.no_newline,
            )
        })
        .collect();
    assert_eq!(
        actual,
        vec![
            (
                DiffLineKind::Context,
                "context one",
                Some(10),
                Some(20),
                false
            ),
            (DiffLineKind::Deletion, "-- content", Some(11), None, false),
            (DiffLineKind::Addition, "++ content", None, Some(21), false),
            (DiffLineKind::Deletion, "  old  ", Some(12), None, false),
            (DiffLineKind::Addition, "  new  ", None, Some(22), false),
            (DiffLineKind::Addition, "", None, Some(23), false),
            (
                DiffLineKind::Context,
                "context last",
                Some(13),
                Some(24),
                false
            ),
        ]
    );
    let second = &file.hunks[1];
    assert_eq!((second.old_count, second.new_count), (1, 1));
    assert_eq!(second.lines.len(), 2);
    assert_eq!(second.lines[0].old_number, Some(40));
    assert_eq!(second.lines[1].new_number, Some(51));
    assert!(second.lines.iter().all(|line| line.no_newline));
    assert_eq!(files[1].hunks, []);
}

#[test]
fn additions_deletions_empty_and_mode_only_files_need_no_text_hunks() {
    let files = parse(concat!(
        "diff --git a/added b/added\nnew file mode 100644\nindex 000..111\n",
        "--- /dev/null\n+++ b/added\n@@ -0,0 +1,2 @@\n+one\n+two\n",
        "diff --git a/deleted b/deleted\ndeleted file mode 100644\n",
        "--- a/deleted\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-one\n-two\n",
        "diff --git a/empty added b/empty added\nnew file mode 100644\nindex 000..e69\n",
        "diff --git a/empty deleted b/empty deleted\ndeleted file mode 100644\nindex e69..000\n",
        "diff --git a/nested b/name b/nested b/name\nold mode 100644\nnew mode 100755\n",
    ));
    assert_eq!(files.len(), 5);
    assert_eq!(files[0].status, DiffFileStatus::Added);
    assert_eq!(files[0].old_path, None);
    assert_eq!(files[0].new_path.as_deref(), Some("added"));
    assert_eq!(files[0].hunks[0].lines[1].new_number, Some(2));
    assert!(
        files[0].hunks[0]
            .lines
            .iter()
            .all(|line| line.old_number.is_none())
    );
    assert_eq!(files[1].status, DiffFileStatus::Deleted);
    assert_eq!(files[1].path(), Some("deleted"));
    assert_eq!(files[1].new_path, None);
    assert_eq!(files[1].hunks[0].lines[1].old_number, Some(2));
    assert!(
        files[1].hunks[0]
            .lines
            .iter()
            .all(|line| line.new_number.is_none())
    );
    assert_eq!(files[2].status, DiffFileStatus::Added);
    assert_eq!(files[2].old_path, None);
    assert_eq!(files[2].path(), Some("empty added"));
    assert_eq!(files[3].status, DiffFileStatus::Deleted);
    assert_eq!(files[3].new_path, None);
    assert_eq!(files[3].path(), Some("empty deleted"));
    assert_eq!(files[4].old_path.as_deref(), Some("nested b/name"));
    assert_eq!(files[4].new_path.as_deref(), Some("nested b/name"));
    assert_eq!(files[4].status, DiffFileStatus::Modified);
    assert_eq!(
        &files[4].metadata[1..],
        ["old mode 100644", "new mode 100755"]
    );
    assert!(
        files[2..]
            .iter()
            .all(|file| file.hunks.is_empty() && !file.binary)
    );
}

#[test]
fn null_labels_also_identify_added_and_deleted_files_without_mode_headers() {
    let files = parse(concat!(
        "diff --git a/new b/new\n--- /dev/null\n+++ b/new\n@@ -0,0 +1 @@\n+x\n",
        "diff --git a/old b/old\n--- a/old\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n",
    ));
    assert_eq!(files[0].status, DiffFileStatus::Added);
    assert_eq!(files[0].old_path, None);
    assert_eq!(files[1].status, DiffFileStatus::Deleted);
    assert_eq!(files[1].new_path, None);
}

#[test]
fn rename_and_copy_metadata_is_authoritative_and_does_not_strip_real_path_prefixes() {
    let files = parse(concat!(
        "diff --git a/a/old b/path b/b/new b/path\nsimilarity index 100%\n",
        "rename from a/old b/path\nrename to b/new b/path\n",
        "diff --git a/source name b/copy name\nsimilarity index 90%\n",
        "copy from source name\ncopy to copy name\n",
        "--- a/source name\t\n+++ b/copy name\t\n@@ -1 +1 @@\n-old\n+new\n",
        "diff --git a/old b/new\nsimilarity index 100%\n",
        "rename from \"caf\\303\\251 old\"\nrename to \"caf\\303\\251 new\"\n",
    ));
    assert_eq!(files.len(), 3);
    assert_eq!(files[0].status, DiffFileStatus::Renamed);
    assert_eq!(files[0].old_path.as_deref(), Some("a/old b/path"));
    assert_eq!(files[0].new_path.as_deref(), Some("b/new b/path"));
    assert_eq!(files[0].hunks, []);
    assert_eq!(files[1].status, DiffFileStatus::Copied);
    assert_eq!(files[1].old_path.as_deref(), Some("source name"));
    assert_eq!(files[1].new_path.as_deref(), Some("copy name"));
    assert_eq!(files[1].hunks.len(), 1);
    assert_eq!(files[2].status, DiffFileStatus::Renamed);
    assert_eq!(files[2].old_path.as_deref(), Some("café old"));
    assert_eq!(files[2].new_path.as_deref(), Some("café new"));
}

#[test]
fn git_quoted_octal_utf8_spaces_and_mixed_header_paths() {
    for (header, expected_old, expected_new) in [
        (
            r#""a/caf\303\251 file\t\"\\.rs" "b/caf\303\251 file\t\"\\.rs""#,
            "café file\t\"\\.rs",
            "café file\t\"\\.rs",
        ),
        (r#"a/old path "b/new\tpath""#, "old path", "new\tpath"),
        (r#""a/old\tpath" b/new path"#, "old\tpath", "new path"),
        ("a/ café  b/ café ", " café ", " café "),
    ] {
        let files = parse(&format!(
            "diff --git {header}\nold mode 100644\nnew mode 100755\n"
        ));
        assert_eq!(files[0].old_path.as_deref(), Some(expected_old));
        assert_eq!(files[0].new_path.as_deref(), Some(expected_new));
    }
    let files = parse(concat!(
        "diff --git a/fallback b/fallback\n",
        "--- \"a/line\\nname\\303\\251\"\tignored timestamp\n",
        "+++ b/name with spaces \tignored timestamp\n@@ -1 +1 @@\n-x\n+y\n",
    ));
    assert_eq!(files[0].old_path.as_deref(), Some("line\nnameé"));
    assert_eq!(files[0].new_path.as_deref(), Some("name with spaces "));
    assert_eq!(
        decode_path(r#""\a\b\t\n\v\f\r\\\"\101""#).as_deref(),
        Some("\x07\x08\t\n\x0b\x0c\r\\\"A")
    );
    assert_eq!(decode_path(r#""\377""#).as_deref(), Some("�"));
}

#[test]
fn binary_notices_and_git_binary_payloads_are_metadata_not_hunks() {
    let files = parse(concat!(
        "diff --git a/image old b/image new\nsimilarity index 80%\n",
        "rename from image old\nrename to image new\nBinary files a/image old and b/image new differ\n",
        "diff --git a/new.bin b/new.bin\nnew file mode 100644\n",
        "Binary files /dev/null and b/new.bin differ\n",
        "diff --git a/gone.bin b/gone.bin\ndeleted file mode 100644\n",
        "GIT binary patch\nliteral 0\nHcmV?d00001\n\nliteral 4\nLcmZQzU|;|M00aO5\n",
    ));
    assert_eq!(files.len(), 3);
    assert!(
        files
            .iter()
            .all(|file| file.binary && file.hunks.is_empty())
    );
    assert_eq!(files[0].status, DiffFileStatus::Renamed);
    assert_eq!(files[1].status, DiffFileStatus::Added);
    assert_eq!(files[1].old_path, None);
    assert_eq!(files[2].status, DiffFileStatus::Deleted);
    assert_eq!(files[2].new_path, None);
    assert!(files[2].metadata.iter().any(|line| line == "literal 4"));
    assert_eq!(files[2].metadata.last().unwrap(), "LcmZQzU|;|M00aO5");
}

#[test]
fn no_newline_marker_can_follow_context_and_does_not_consume_coordinates() {
    let files = parse("diff --git a/a b/a\n@@ -8 +9 @@\n same\n\\ No newline at end of file\n");
    let hunk = &files[0].hunks[0];
    assert_eq!(hunk.lines.len(), 1);
    assert_eq!(hunk.lines[0].old_number, Some(8));
    assert_eq!(hunk.lines[0].new_number, Some(9));
    assert!(hunk.lines[0].no_newline);
}

#[test]
fn no_newline_marker_applies_only_to_the_immediately_previous_line() {
    let files = parse(concat!(
        "diff --git a/a b/a\n@@ -1 +1 @@\n-old\n",
        "\\ No newline at end of file\n+new\n",
    ));
    let hunk = &files[0].hunks[0];
    assert_eq!(hunk.lines.len(), 2);
    assert!(hunk.lines[0].no_newline);
    assert!(!hunk.lines[1].no_newline);
    assert_eq!(hunk.lines[1].new_number, Some(1));
}

#[test]
fn trailing_header_like_metadata_cannot_change_a_files_paths() {
    let files = parse(concat!(
        "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n",
        "--- a/not-a-file\n+++ b/not-a-file\n",
    ));
    assert_eq!(files[0].old_path.as_deref(), Some("a"));
    assert_eq!(files[0].new_path.as_deref(), Some("a"));
    assert_eq!(files[0].status, DiffFileStatus::Modified);
    assert_eq!(files[0].metadata.last().unwrap(), "+++ b/not-a-file");
}

#[test]
fn every_truncated_prefix_is_safe_and_partial_hunks_are_not_padded() {
    let patch = lines(concat!(
        "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,2 +1,2 @@\n first\n-old\n+new\n",
        "diff --git a/b b/b\nold mode 100644\nnew mode 100755\n",
    ));
    for end in 0..=patch.len() {
        let _ = parse_patch(&patch[..end]);
    }
    let (files, incomplete) = parse_with_status(&patch[..6]);
    assert!(incomplete);
    let hunk = &files[0].hunks[0];
    assert_eq!((hunk.old_count, hunk.new_count), (2, 2));
    assert_eq!(hunk.lines.len(), 2);
    assert_eq!(hunk.lines[1].old_number, Some(2));
    assert_eq!(hunk.lines[1].new_number, None);
    assert!(!parse_with_status(&patch).1);
    let interrupted = lines("diff --git a/a b/a\n@@ -1,2 +1,2 @@\n x\ndiff --git a/b b/b\n");
    let (files, incomplete) = parse_with_status(&interrupted);
    assert!(incomplete);
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].hunks[0].lines.len(), 1);
}

#[test]
fn diff_view_keeps_raw_styles_stats_and_honest_truncation() {
    let patch = lines("diff --git a/a b/a\n@@ -1,2 +1,2 @@\n same\n-old\n+new\n");
    for (input, reader_truncated, expected_truncated) in [
        (patch.clone(), false, false),
        (patch.clone(), true, true), // A cut between complete hunks/files is only known by the reader.
        (patch[..4].to_vec(), false, true),
        (patch[..4].to_vec(), true, true),
    ] {
        let mut view = DiffView::new(HeldActionId::new(1), "held diff");
        fill_view(
            &mut view,
            RawDiff {
                insertions: 1,
                deletions: 1,
                files: 1,
                lines: input.clone(),
                truncated: reader_truncated,
            },
        );
        assert_eq!(view.stat, "+1 −1 · 1 file");
        assert_eq!(view.truncated, expected_truncated);
        assert_eq!(
            view.lines
                .iter()
                .map(|line| line.text.clone())
                .collect::<Vec<_>>(),
            input
        );
        assert_eq!(view.lines[0].tone, Tone::Accent);
        assert_eq!(view.lines[2].tone, Tone::Text);
        assert_eq!(view.lines[3].tone, Tone::Error);
        assert_eq!(view.files.len(), 1);
    }
    let mut view = DiffView::new(HeldActionId::new(2), "empty diff");
    fill_view(&mut view, RawDiff::default());
    assert_eq!(view.stat, "+0 −0 · 0 files");
    assert!(!view.truncated);
    assert_eq!(view.files, []);
    assert_eq!(view.lines, []);
}

#[test]
fn malformed_input_is_retained_without_inventing_hunks_or_panicking() {
    assert_eq!(parse_patch(&lines("read failed\n+not a patch\n")), []);
    for header in [
        "@@ -1 +1",
        "@@ -x +1 @@",
        "@@ -1, +1 @@",
        "@@ -+1 +1 @@",
        "@@ -18446744073709551616 +1 @@",
        "@@ -18446744073709551615,2 +1 @@",
    ] {
        let text = format!("diff --git a/a b/a\n{header}\n+unparsed\n");
        let (files, incomplete) = parse_with_status(&lines(&text));
        assert!(incomplete);
        assert_eq!(files[0].hunks, []);
        assert_eq!(files[0].metadata[1], header);
        assert_eq!(files[0].metadata[2], "+unparsed");
    }
    let files = parse("diff --git \"unterminated\nunknown extended header\n");
    assert_eq!(files[0].path(), None);
    assert_eq!(files[0].metadata.len(), 2);
    for quoted in [r#""bad\q""#, r#""bad\777""#, "\"unfinished\\", "\"no close"] {
        assert!(decode_path(quoted).is_none(), "{quoted}");
    }
    let files =
        parse("diff --git a/a b/a\n@@ -18446744073709551615 +18446744073709551615 @@\n x\n");
    assert_eq!(files[0].hunks[0].lines[0].old_number, Some(u64::MAX));
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "qm-patch-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&path).expect("isolated git fixture");
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", repo)
        .env("TMPDIR", repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(repo)
        .args(["-c", "core.quotePath=true"])
        .args(args)
        .output()
        .expect("local git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn file<'a>(files: &'a [DiffFile], path: &str) -> &'a DiffFile {
    files
        .iter()
        .find(|file| file.path() == Some(path))
        .unwrap_or_else(|| panic!("missing {path}: {files:?}"))
}

#[test]
fn real_local_git_patch_covers_quoting_renames_copies_empty_modes_and_binary() {
    use std::os::unix::fs::PermissionsExt;

    let scratch = Scratch::new();
    let repo = &scratch.0;
    git(repo, &["init", "--quiet"]);
    let odd = "café space\t\"\\.txt";
    for (name, content) in [
        (odd, "before\n"),
        ("rename old space", "renamed text\n"),
        ("copy source", "copied text\n"),
        ("mode only", "mode\n"),
        ("empty deleted", ""),
        ("deleted", "gone\n"),
        ("no newline", "old"),
        ("nested b/name", "same\n"),
    ] {
        let path = repo.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    git(repo, &["add", "--all"]);
    // The index is enough: compare against a local tree without commits, identities or hooks.
    let base = git(repo, &["write-tree"]);
    fs::write(repo.join(odd), "after\n").unwrap();
    fs::rename(repo.join("rename old space"), repo.join("rename new space")).unwrap();
    fs::copy(repo.join("copy source"), repo.join("copy dest space")).unwrap();
    fs::set_permissions(repo.join("mode only"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(
        repo.join("nested b/name"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fs::remove_file(repo.join("empty deleted")).unwrap();
    fs::remove_file(repo.join("deleted")).unwrap();
    fs::write(repo.join("empty added"), "").unwrap();
    fs::write(repo.join("binary added"), [0, 1, 2, 255]).unwrap();
    fs::write(repo.join("no newline"), "new").unwrap();
    git(repo, &["add", "--all"]);
    let args = [
        "diff",
        "--cached",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--find-renames",
        "--find-copies-harder",
        "--no-rename-empty",
        base.trim(),
        "--",
    ];
    let patch = git(repo, &args);
    let files = parse(&patch);
    assert_eq!(files.len(), 10, "{patch}");
    assert_eq!(file(&files, odd).old_path.as_deref(), Some(odd));
    assert_eq!(file(&files, odd).hunks[0].lines[1].text, "after");
    assert_eq!(
        file(&files, "rename new space").status,
        DiffFileStatus::Renamed
    );
    assert_eq!(
        file(&files, "rename new space").old_path.as_deref(),
        Some("rename old space")
    );
    assert_eq!(
        file(&files, "copy dest space").status,
        DiffFileStatus::Copied
    );
    assert_eq!(file(&files, "empty added").status, DiffFileStatus::Added);
    assert_eq!(file(&files, "empty added").old_path, None);
    assert_eq!(
        file(&files, "empty deleted").status,
        DiffFileStatus::Deleted
    );
    assert_eq!(file(&files, "empty deleted").new_path, None);
    assert_eq!(file(&files, "deleted").status, DiffFileStatus::Deleted);
    assert_eq!(file(&files, "mode only").hunks, []);
    assert_eq!(
        file(&files, "nested b/name").old_path.as_deref(),
        Some("nested b/name")
    );
    assert!(file(&files, "binary added").binary);
    assert!(
        file(&files, "no newline").hunks[0]
            .lines
            .iter()
            .all(|line| line.no_newline)
    );
    let mut binary_args = args.to_vec();
    binary_args.insert(1, "--binary");
    let binary_patch = git(repo, &binary_args);
    let files = parse(&binary_patch);
    assert!(
        file(&files, "binary added")
            .metadata
            .iter()
            .any(|line| line == "GIT binary patch")
    );
}
