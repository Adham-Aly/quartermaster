//! Diff-specific contrast and resize regressions.

use super::*;
use ratatui::style::Color;

fn many_files() -> App {
    with_files(
        (0..60)
            .map(|i| {
                let path = format!("src/file-{i:02}.rs");
                DiffFile::new(Some(path.clone()), Some(path))
            })
            .collect(),
    )
}

#[test]
fn shrinking_the_tree_keeps_a_visible_selection_and_remembers_the_adjusted_scroll() {
    let mut app = many_files();
    draw(&mut app, 150, 45);
    app.select_diff_file(30);
    draw(&mut app, 150, 45);
    assert_eq!(app.view.diff.tree_scroll, 0);
    for (width, height) in [(120, 40), (119, 40), (100, 30), (60, 16)] {
        app.on_event(Event::Resize(width, height), Instant::now());
        let buffer = draw(&mut app, width, height);
        let leaf = region(&app, DiffHit::File(30));
        let tree = region(&app, DiffHit::Tree);
        assert!(tree.contains(Position::new(leaf.x, leaf.y)));
        assert!(text(&buffer, leaf.x, leaf.y, leaf.width).contains("file-30.rs"));
        assert_eq!(app.view.diff.file, 30);
        assert_eq!(app.view.diff.scroll, 0);
        assert!(!app.view.diff.tree_focus);
        let scroll = app.view.diff.tree_scroll;
        // The adjustment must persist on the next frame and subsequent wheel input.
        draw(&mut app, width, height);
        assert_eq!(app.view.diff.tree_scroll, scroll);
        region(&app, DiffHit::File(30));
    }
    assert!(app.view.diff.tree_scroll > 0);
    let scroll = app.view.diff.tree_scroll;
    mouse(&mut app, DiffHit::Tree, MouseEventKind::ScrollUp);
    assert_eq!(app.view.diff.tree_scroll, scroll - 3);
    draw(&mut app, 60, 16);
    assert!(
        app.term
            .drawn
            .hits
            .rect_of(Hit::Diff(DiffHit::File(30)))
            .is_none()
    );
}

#[test]
fn shrink_does_not_override_tree_only_wheel_scrolling_even_in_the_same_input_batch() {
    for draw_after_wheel in [false, true] {
        let mut app = many_files();
        draw(&mut app, 150, 45);
        mouse(&mut app, DiffHit::Tree, MouseEventKind::ScrollDown);
        assert_eq!(app.view.diff.tree_scroll, 3);
        if draw_after_wheel {
            draw(&mut app, 150, 45);
        }
        app.on_event(Event::Resize(60, 16), Instant::now());
        draw(&mut app, 60, 16);
        assert_eq!(
            (
                app.view.diff.file,
                app.view.diff.scroll,
                app.view.diff.tree_scroll
            ),
            (0, 0, 3)
        );
        assert!(
            app.term
                .drawn
                .hits
                .rect_of(Hit::Diff(DiffHit::File(0)))
                .is_none()
        );
        draw(&mut app, 60, 16);
        assert_eq!(app.view.diff.tree_scroll, 3);
    }
}

#[test]
fn addition_rows_and_badges_keep_green_without_changing_context_or_deletions() {
    for light in [false, true] {
        for (width, height) in [(60, 16), (100, 30), (119, 40), (120, 40), (150, 45)] {
            let mut file = DiffFile::new(None, Some("new.rs".into()));
            file.status = DiffFileStatus::Added;
            file.hunks = vec![hunk(
                1,
                1,
                &[
                    (DiffLineKind::Deletion, "old"),
                    (DiffLineKind::Addition, "new"),
                    (DiffLineKind::Context, "context"),
                ],
            )];
            let mut app = with_files(vec![file]);
            app.term.palette = theme::fit_reported(&qm_types::wire::Colors {
                bg: Some(if light { [255; 3] } else { [0; 3] }),
                ..Default::default()
            });
            let buffer = draw(&mut app, width, height);
            let body = region(&app, DiffHit::Body);
            let (right, y) = (body.x + body.width / 2 + 1, body.y + 1);
            let addition = app.term.palette.addition;
            assert_eq!(text(&buffer, right + 6, y, 3), "new");
            assert_eq!(buffer[(right + 6, y)].fg, addition.fg.unwrap());
            assert_eq!(
                buffer[(right + 6, y)].bg,
                addition.bg.unwrap_or(Color::Reset)
            );
            if light {
                assert_eq!(addition.bg, Some(Color::Green));
                assert_eq!(buffer[(body.right() - 1, y)].bg, Color::Green);
            } else {
                assert_eq!(addition.fg, Some(Color::Green));
                assert_eq!(addition.bg, None);
            }
            assert_eq!(buffer[(body.x + 6, y)].fg, app.term.palette.error);
            assert_eq!(buffer[(body.x + 6, y)].bg, Color::Reset);
            assert_eq!(buffer[(right + 6, y + 1)].bg, Color::Reset);
            assert_eq!(buffer[(right, body.y)].bg, Color::Reset);
            click(&mut app, DiffHit::File(0));
            let selected = draw(&mut app, width, height);
            let leaf = region(&app, DiffHit::File(0));
            if light {
                assert_eq!(selected[(leaf.right() - 2, leaf.y)].bg, Color::Green);
            }
        }
    }
}
