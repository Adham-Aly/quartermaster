//! Layout refinements found while reviewing the real approval Diff.

use super::*;

#[test]
fn single_child_folders_compact_without_consuming_leaf_indentation_or_clicks() {
    let mut file = DiffFile::new(
        Some("old.rs".into()),
        Some("src/a/b/c/d/e/f/module.rs".into()),
    );
    file.status = DiffFileStatus::Copied;
    let mut app = with_files(vec![file]);
    let entries = tree::entries(&app.diff_view().unwrap().files);
    assert_eq!(entries.len(), 2);
    assert_eq!(
        (&*entries[0].label, entries[0].depth, entries[0].file),
        ("src/a/b/c/d/e/f/", 0, None)
    );
    assert_eq!(
        (&*entries[1].label, entries[1].depth, entries[1].file),
        ("module.rs", 1, Some(0))
    );
    assert!(
        entries
            .iter()
            .all(|entry| entry.status == DiffFileStatus::Copied)
    );
    let buffer = draw(&mut app, 60, 16);
    let leaf = region(&app, DiffHit::File(0));
    assert_eq!(text(&buffer, leaf.x + 4, leaf.y, 9), "module.rs");
    assert_eq!(text(&buffer, leaf.right() - 2, leaf.y, 1), "C");
    click(&mut app, DiffHit::File(0));
    assert!(app.view.diff.tree_focus);
    assert_eq!(app.diff_file_order(), [0]);
}

#[test]
fn branching_paths_beyond_four_levels_keep_their_actual_indentation() {
    let paths = (0..8)
        .map(|depth| format!("root/{}peer.rs", "d/".repeat(depth)))
        .chain(std::iter::once(format!("root/{}leaf.rs", "d/".repeat(8))));
    let files = paths.map(|path| DiffFile::new(None, Some(path))).collect();
    let mut app = with_files(files);
    let entries = tree::entries(&app.diff_view().unwrap().files);
    let leaf = entries
        .iter()
        .find(|entry| entry.label == "leaf.rs")
        .unwrap();
    assert!(leaf.depth > 4);
    let index = leaf.file.unwrap();
    let buffer = draw(&mut app, 150, 45);
    let row = region(&app, DiffHit::File(index));
    let x = row.x + 2 + u16::try_from(leaf.depth * 2).unwrap();
    assert_eq!(text(&buffer, x, row.y, 7), "leaf.rs");
    assert_eq!(buffer[(x - 1, row.y)].symbol(), " ");
}

#[test]
fn hunk_headers_and_file_notices_are_full_width_but_code_keeps_its_divider() {
    let mut code = DiffFile::new(Some("a.rs".into()), Some("a.rs".into()));
    code.hunks = vec![hunk(1, 1, &[(DiffLineKind::Context, "unchanged")])];
    let mut app = with_files(vec![code]);
    let buffer = draw(&mut app, 150, 45);
    let body = region(&app, DiffHit::Body);
    let middle = body.x + body.width / 2;
    assert_eq!(buffer[(middle, body.y)].symbol(), " ");
    assert_eq!(buffer[(middle, body.y + 1)].symbol(), "│");
    for binary in [false, true] {
        let mut file = DiffFile::new(Some("file".into()), Some("file".into()));
        file.binary = binary;
        let mut app = with_files(vec![file]);
        let buffer = draw(&mut app, 150, 45);
        let body = region(&app, DiffHit::Body);
        assert_eq!(buffer[(body.x + body.width / 2, body.y)].symbol(), " ");
    }
}

#[test]
fn batched_file_selection_uses_the_new_files_scroll_and_pan_limits() {
    for (width, height) in [(60, 16), (150, 45)] {
        let short = DiffFile::new(Some("a.rs".into()), Some("a.rs".into()));
        let mut app = with_files(vec![short, long_file()]);
        draw(&mut app, width, height);
        let body = region(&app, DiffHit::Body);
        assert_eq!(app.term.drawn.diff_scroll_max, 0);
        key(&mut app, KeyCode::Char(']'));
        key(&mut app, KeyCode::End);
        key(&mut app, KeyCode::Right);
        assert_eq!(app.view.diff.file, 1);
        assert_eq!(app.view.diff.scroll, 162 - usize::from(body.height));
        assert_eq!(app.view.diff.horizontal, 8);
        // No redraw occurred: even returning to the short file must use its own limits.
        key(&mut app, KeyCode::Char('['));
        key(&mut app, KeyCode::End);
        key(&mut app, KeyCode::Right);
        assert_eq!(
            (
                app.view.diff.file,
                app.view.diff.scroll,
                app.view.diff.horizontal
            ),
            (0, 0, 0)
        );
        // Also test the reverse case with the long file's geometry in the frame map.
        key(&mut app, KeyCode::Char(']'));
        draw(&mut app, width, height);
        key(&mut app, KeyCode::Char('['));
        key(&mut app, KeyCode::End);
        key(&mut app, KeyCode::Right);
        assert_eq!((app.view.diff.scroll, app.view.diff.horizontal), (0, 0));
    }
}
