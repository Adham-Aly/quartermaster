//! End-to-end cell and input regressions for the parsed diff, without a core or filesystem.

use super::*;
use crate::{popups::Popup, sample, theme};
use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use qm_types::api::{
    DiffFile, DiffFileStatus, DiffHunk, DiffLine, DiffLineKind, HeldAnswer, Styled, Tone,
};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Modifier};

fn draw(app: &mut App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    app.after_draw();
    terminal.backend().buffer().clone()
}

fn key(app: &mut App, code: KeyCode) {
    app.on_event(
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
        Instant::now(),
    );
}

fn region(app: &App, hit: DiffHit) -> Rect {
    app.term
        .drawn
        .hits
        .rect_of(Hit::Diff(hit))
        .expect("drawn diff region")
}

fn mouse(app: &mut App, hit: DiffHit, kind: MouseEventKind) {
    let area = region(app, hit);
    app.on_event(
        Event::Mouse(MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }),
        Instant::now(),
    );
}

fn click(app: &mut App, hit: DiffHit) {
    mouse(app, hit, MouseEventKind::Down(MouseButton::Left));
    mouse(app, hit, MouseEventKind::Up(MouseButton::Left));
}

/// Cell-based (not byte-based), including the blank cell after a wide glyph.
fn text(buffer: &Buffer, x: u16, y: u16, width: u16) -> String {
    (x..x + width).map(|x| buffer[(x, y)].symbol()).collect()
}

fn review() -> App {
    let mut app = App::new();
    sample::install_diff(&mut app);
    app.view.sample = false;
    app.take_commands();
    app
}

fn with_files(files: Vec<DiffFile>) -> App {
    let mut app = review();
    let view = app.snapshot.diff.as_mut().unwrap();
    view.files = files;
    view.lines.clear();
    app
}

fn hunk(old: u64, new: u64, lines: &[(DiffLineKind, &str)]) -> DiffHunk {
    let mut result = DiffHunk::new("", old, 0, new, 0);
    for &(kind, text) in lines {
        let mut line = DiffLine::new(kind, text);
        if kind != DiffLineKind::Addition {
            line.old_number = Some(old + result.old_count);
            result.old_count += 1;
        }
        if kind != DiffLineKind::Deletion {
            line.new_number = Some(new + result.new_count);
            result.new_count += 1;
        }
        result.lines.push(line);
    }
    result.header = format!(
        "@@ -{old},{} +{new},{} @@",
        result.old_count, result.new_count
    );
    result
}

fn long_file() -> DiffFile {
    let mut file = DiffFile::new(Some("src/file-00.rs".into()), Some("src/file-00.rs".into()));
    for start in [1000, 2000] {
        let mut hunk = DiffHunk::new(
            format!("@@ -{start},80 +{start},80 @@"),
            start,
            80,
            start,
            80,
        );
        for number in start..start + 80 {
            let mut line = DiffLine::new(
                DiffLineKind::Context,
                format!("line {number} {}", "long code ".repeat(12)),
            );
            line.old_number = Some(number);
            line.new_number = Some(number);
            hunk.lines.push(line);
        }
        file.hunks.push(hunk);
    }
    file
}

#[test]
fn sample_uses_the_approval_action_and_its_counts_and_ranges_are_real() {
    let mut app = App::new();
    sample::install_diff(&mut app);
    assert_eq!(app.screen, Screen::Diff);
    assert_eq!(app.popup(), None);
    assert!(app.view.sample);
    let view = app.diff_view().unwrap();
    assert_eq!(app.view.diff.held, Some(HeldActionId::new(1)));
    assert!(app.view.diff_from.is_some());
    assert_eq!(view.stat, "+16 −7 · 6 files");
    assert_eq!(
        view.files
            .iter()
            .map(|file| file.status)
            .collect::<Vec<_>>(),
        [
            DiffFileStatus::Modified,
            DiffFileStatus::Added,
            DiffFileStatus::Deleted,
            DiffFileStatus::Renamed,
            DiffFileStatus::Added,
            DiffFileStatus::Modified,
        ]
    );
    let mut counts = (0, 0);
    for file in &view.files {
        for hunk in &file.hunks {
            let (mut old, mut new) = (hunk.old_start, hunk.new_start);
            for line in &hunk.lines {
                assert_eq!(
                    line.old_number,
                    (line.kind != DiffLineKind::Addition).then_some(old)
                );
                assert_eq!(
                    line.new_number,
                    (line.kind != DiffLineKind::Deletion).then_some(new)
                );
                old += u64::from(line.old_number.is_some());
                new += u64::from(line.new_number.is_some());
                counts.0 += usize::from(line.kind == DiffLineKind::Addition);
                counts.1 += usize::from(line.kind == DiffLineKind::Deletion);
            }
            assert_eq!(
                (old - hunk.old_start, new - hunk.new_start),
                (hunk.old_count, hunk.new_count)
            );
        }
    }
    assert_eq!(counts, (16, 7));
    assert_eq!(
        view.lines.iter().filter(|l| l.tone == Tone::Ok).count(),
        counts.0
    );
    assert_eq!(
        view.lines.iter().filter(|l| l.tone == Tone::Error).count(),
        counts.1
    );
    assert!(
        view.lines
            .iter()
            .any(|l| l.text == "\\ No newline at end of file")
    );
    let approval = app
        .snapshot
        .actions
        .iter()
        .find(|item| item.card.held == app.view.diff.held)
        .unwrap();
    assert!(
        approval
            .card
            .lines
            .iter()
            .flat_map(|l| &l.spans)
            .any(|s| s.text.contains(&view.stat))
    );
    assert_eq!(
        app.take_commands(),
        [Command::ShowDiff {
            held: Some(HeldActionId::new(1))
        }]
    );
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.popup(), Some(Popup::Action));
    assert_eq!(
        app.view.action_open,
        app.snapshot
            .actions
            .iter()
            .find(|i| i.card.held == Some(HeldActionId::new(1)))
            .map(|i| i.key.clone())
    );
}

#[test]
fn split_layout_and_focus_styles_survive_small_sizes_and_the_bridge_fold_boundary() {
    for light in [false, true] {
        for (w, h, tree_width, code_x, code_width) in [
            (60, 16, 14, 18, 41),
            (100, 30, 23, 27, 72),
            (119, 40, 27, 31, 87),
            (120, 40, 28, 32, 87),
            (150, 45, 34, 38, 111),
        ] {
            let mut app = review();
            app.term.palette = theme::fit_reported(&qm_types::wire::Colors {
                bg: Some(if light { [255; 3] } else { [0; 3] }),
                ..Default::default()
            });
            let buffer = draw(&mut app, w, h);
            let tree = region(&app, DiffHit::Tree);
            let body = region(&app, DiffHit::Body);
            assert_eq!(tree, Rect::new(1, 4, tree_width, h - 6), "{w}×{h}");
            assert_eq!(body, Rect::new(code_x, 6, code_width, h - 8));
            assert_eq!(text(&buffer, 1, 3, 11), " Files · 6 ");
            let status = text(&buffer, 0, h - 1, w);
            assert!(
                status.contains("files") || status.contains("? help"),
                "{w}×{h}: {status}"
            );
            if w == 60 {
                assert!(text(&buffer, 0, 2, w).trim_end().ends_with("D deleted"));
            }
            assert_eq!(text(&buffer, body.x, 5, 7), " Before");
            let after = body.x + body.width / 2 + 1;
            assert_eq!(text(&buffer, after, 5, 6), " After");
            assert_eq!(buffer[(0, 3)].symbol(), "╭");
            assert_eq!(buffer[(w - 1, h - 2)].symbol(), "╯");
            assert_eq!(buffer[(body.x + body.width / 2, 7)].symbol(), "│");
            assert_eq!(buffer[(body.x - 1, 3)].fg, app.term.palette.accent);
            assert_eq!(buffer[(0, 3)].fg, app.term.palette.dim);
            assert_eq!(buffer[(body.x + 1, 5)].fg, app.term.palette.error);
            assert_eq!(buffer[(after + 1, 5)].fg, app.term.palette.ok);
            assert!(buffer[(after + 1, 5)].modifier.contains(Modifier::BOLD));
            assert!(
                app.term
                    .drawn
                    .hits
                    .rect_of(Hit::Pane(crate::hit::Pane::Crew))
                    .is_none()
            );
            key(&mut app, KeyCode::Tab);
            let focused = draw(&mut app, w, h);
            assert_eq!(focused[(0, 3)].fg, app.term.palette.accent);
            assert_eq!(focused[(body.x - 1, 3)].fg, app.term.palette.dim);
        }
    }
}

#[test]
fn unequal_change_runs_leave_empty_sides_without_made_up_gutter_numbers() {
    use DiffLineKind::{Addition as A, Context as C, Deletion as D};
    let mut file = DiffFile::new(Some("src/main.rs".into()), Some("src/main.rs".into()));
    file.hunks = vec![
        hunk(
            10,
            20,
            &[
                (C, "shared start"),
                (D, "remove one"),
                (D, "remove two"),
                (D, "remove three"),
                (A, "insert one"),
                (C, "shared middle"),
                (D, "remove last"),
                (A, "insert two"),
                (A, "insert three"),
                (C, "shared end"),
            ],
        ),
        hunk(100, 200, &[(C, "second hunk")]),
    ];
    let mut app = with_files(vec![file]);
    let buffer = draw(&mut app, 150, 45);
    let body = region(&app, DiffHit::Body);
    let (old, new, y) = (body.x, body.x + body.width / 2 + 1, body.y);
    for (row, left, right) in [
        (1, " 10   ", " 20   "),
        (2, " 11 − ", " 21 + "),
        (3, " 12 − ", "      "),
        (4, " 13 − ", "      "),
        (5, " 14   ", " 22   "),
        (6, " 15 − ", " 23 + "),
        (7, "      ", " 24 + "),
        (8, " 16   ", " 25   "),
        (10, "100   ", "200   "),
    ] {
        assert_eq!(text(&buffer, old, y + row, 6), left);
        assert_eq!(text(&buffer, new, y + row, 6), right);
    }
    assert_eq!(text(&buffer, new, y + 3, 55).trim(), "");
    assert_eq!(text(&buffer, old, y + 7, 55).trim(), "");
    assert_eq!(text(&buffer, old + 6, y + 2, 10), "remove one");
    assert_eq!(text(&buffer, new + 6, y + 2, 10), "insert one");
    assert_eq!(buffer[(old, y + 2)].fg, app.term.palette.error);
    assert_eq!(buffer[(new, y + 2)].fg, app.term.palette.ok);
    assert!(buffer[(old, y + 2)].modifier.contains(Modifier::REVERSED));
    assert!(buffer[(new, y + 2)].modifier.contains(Modifier::REVERSED));
    assert!(!buffer[(new, y + 3)].modifier.contains(Modifier::REVERSED));
    assert_eq!(buffer[(old, y + 1)].fg, app.term.palette.dim);
    assert_eq!(text(&buffer, old + 1, y + 9, 5), "@@ -1");
}

#[test]
fn tree_contains_only_changed_paths_with_leaf_and_aggregate_folder_statuses() {
    let mut app = review();
    let buffer = draw(&mut app, 150, 45);
    let entries = tree::entries(&app.diff_view().unwrap().files);
    let expected = [
        ("assets/", 0, "A"),
        ("billing/", 1, "A"),
        ("preview.png", 2, "A"),
        ("docs/", 0, "R"),
        ("billing/", 1, "R"),
        ("proration.md", 2, "R"),
        ("scripts/", 0, "M"),
        ("check-billing.sh", 1, "M"),
        ("src/", 0, "M"),
        ("billing/", 1, "M"),
        ("legacy.rs", 2, "D"),
        ("proration.rs", 2, "M"),
        ("tests/", 0, "A"),
        ("billing/", 1, "A"),
        ("proration.rs", 2, "A"),
    ];
    assert_eq!(entries.len(), expected.len());
    let area = region(&app, DiffHit::Tree);
    for (row, (entry, &(label, depth, badge))) in entries.iter().zip(&expected).enumerate() {
        assert_eq!(
            (
                &*entry.label,
                entry.depth,
                tree::status_label(entry.status).0
            ),
            (label, depth, badge)
        );
        let y = area.y + u16::try_from(row).unwrap();
        assert_eq!(text(&buffer, area.right() - 2, y, 1), badge);
        let color = match badge {
            "A" => app.term.palette.ok,
            "D" => app.term.palette.error,
            "R" => app.term.palette.blue,
            _ => app.term.palette.accent,
        };
        assert_eq!(buffer[(area.right() - 2, y)].fg, color);
        if let Some(index) = entry.file {
            assert_eq!(region(&app, DiffHit::File(index)).y, y);
        } else {
            assert_eq!(
                app.term.drawn.hits.at(area.x, y),
                Some(Hit::Diff(DiffHit::Tree))
            );
        }
    }
    assert_eq!(entries.iter().filter(|e| e.file.is_some()).count(), 6);
    assert!(
        !entries
            .iter()
            .any(|e| e.label == "preview.md" || e.label == "Cargo.toml")
    );
    key(&mut app, KeyCode::Tab);
    let buffer = draw(&mut app, 150, 45);
    let selected = region(&app, DiffHit::File(0));
    assert_eq!(buffer[(selected.x, selected.y)].bg, app.term.palette.fill);
    assert_eq!(
        buffer[(selected.x + 6, selected.y)].fg,
        app.term.palette.on_fill
    );
}

#[test]
fn keys_clicks_and_wheel_use_the_drawn_panes_and_reveal_the_selected_file() {
    let mut files = vec![long_file()];
    files.extend((1..25).map(|i| {
        let path = format!("src/file-{i:02}.rs");
        DiffFile::new(Some(path.clone()), Some(path))
    }));
    let mut app = with_files(files);
    draw(&mut app, 60, 16);
    mouse(&mut app, DiffHit::Body, MouseEventKind::ScrollDown);
    assert_eq!(app.view.diff.scroll, 3);
    key(&mut app, KeyCode::Right);
    assert_eq!(app.view.diff.horizontal, 8);
    mouse(&mut app, DiffHit::Tree, MouseEventKind::ScrollDown);
    assert_eq!(app.view.diff.tree_scroll, 3);
    assert_eq!((app.view.diff.file, app.view.diff.scroll), (0, 3));
    mouse(&mut app, DiffHit::Tree, MouseEventKind::ScrollUp);
    assert_eq!(app.view.diff.tree_scroll, 0);
    click(&mut app, DiffHit::File(2));
    assert!(app.view.diff.tree_focus);
    assert_eq!(
        (
            app.view.diff.file,
            app.view.diff.scroll,
            app.view.diff.horizontal
        ),
        (2, 0, 0)
    );
    assert!(app.view.pointer_picked);
    key(&mut app, KeyCode::Down);
    assert!(!app.view.pointer_picked);
    key(&mut app, KeyCode::Char(']'));
    key(&mut app, KeyCode::Char('['));
    key(&mut app, KeyCode::Up);
    assert_eq!(app.view.diff.file, 2);
    key(&mut app, KeyCode::End);
    let buffer = draw(&mut app, 60, 16);
    assert_eq!(app.view.diff.file, 24);
    let selected = region(&app, DiffHit::File(24));
    assert!(region(&app, DiffHit::Tree).contains(Position::new(selected.x, selected.y)));
    assert_eq!(text(&buffer, selected.x + 4, selected.y, 7), "file-2…");
    key(&mut app, KeyCode::Char(']'));
    assert_eq!(app.view.diff.file, 24);
    key(&mut app, KeyCode::Home);
    key(&mut app, KeyCode::Char('['));
    assert_eq!(app.view.diff.file, 0);
    key(&mut app, KeyCode::PageDown);
    assert_eq!(app.view.diff.file, 9); // Ten visible tree rows, one row of overlap.
    key(&mut app, KeyCode::PageUp);
    key(&mut app, KeyCode::Enter);
    assert!(!app.view.diff.tree_focus);
    key(&mut app, KeyCode::BackTab);
    assert!(app.view.diff.tree_focus);
    draw(&mut app, 60, 16);
    click(&mut app, DiffHit::Body);
    assert!(!app.view.diff.tree_focus);
    assert_eq!(app.take_commands(), []);
}

#[test]
fn renamed_paths_navigate_in_visible_tree_order_not_source_patch_order() {
    let files = ["z.txt", "a.txt", "middle.txt"]
        .map(|path| DiffFile::new(Some(format!("old/{path}")), Some(path.into())));
    let mut app = with_files(files.into());
    draw(&mut app, 100, 30);
    key(&mut app, KeyCode::Tab);
    key(&mut app, KeyCode::Home);
    assert_eq!(app.view.diff.file, 1);
    key(&mut app, KeyCode::Down);
    assert_eq!(app.view.diff.file, 2);
    key(&mut app, KeyCode::Down);
    assert_eq!(app.view.diff.file, 0);
    key(&mut app, KeyCode::Up);
    assert_eq!(app.view.diff.file, 2);
    key(&mut app, KeyCode::Char('['));
    assert_eq!(app.view.diff.file, 1);
    key(&mut app, KeyCode::Char(']'));
    assert_eq!(app.view.diff.file, 2);
}

#[test]
fn long_hunks_scroll_by_paired_rows_and_clamp_at_each_viewport_size() {
    for (width, height) in [(60, 16), (100, 30), (119, 40), (120, 40), (150, 45)] {
        let mut app = with_files(vec![long_file()]);
        draw(&mut app, width, height);
        let body = region(&app, DiffHit::Body);
        let max = 162 - usize::from(body.height); // Two headers and 160 context rows.
        assert_eq!(app.term.drawn.diff_scroll_max, max);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::PageDown);
        assert_eq!(app.view.diff.scroll, usize::from(body.height));
        key(&mut app, KeyCode::PageUp);
        assert_eq!(app.view.diff.scroll, 1);
        key(&mut app, KeyCode::End);
        key(&mut app, KeyCode::Down);
        mouse(&mut app, DiffHit::Body, MouseEventKind::ScrollDown);
        assert_eq!(app.view.diff.scroll, max);
        let buffer = draw(&mut app, width, height);
        let last = body.bottom() - 1;
        assert_eq!(text(&buffer, body.x, last, 7), "2079   ");
        assert_eq!(text(&buffer, body.x + 7, last, 9), "line 2079");
        mouse(&mut app, DiffHit::Body, MouseEventKind::ScrollUp);
        assert_eq!(app.view.diff.scroll, max - 3);
        key(&mut app, KeyCode::Home);
        key(&mut app, KeyCode::Up);
        assert_eq!(app.view.diff.scroll, 0);
    }
}

#[test]
fn unicode_tabs_controls_and_horizontal_pan_preserve_cell_alignment_and_gutters() {
    let content = format!(
        "\t123界🙂e\u{301} {}\u{1b}[31m\r\n END",
        "payload ".repeat(20)
    );
    let mut file = DiffFile::new(Some("unicode.rs".into()), Some("unicode.rs".into()));
    file.hunks = vec![hunk(1, 1, &[(DiffLineKind::Context, &content)])];
    let mut app = with_files(vec![file]);
    let buffer = draw(&mut app, 100, 30);
    let body = region(&app, DiffHit::Body);
    let (x, y) = (body.x + 6, body.y + 1);
    assert_eq!(text(&buffer, x, y, 7), "    123");
    assert_eq!(buffer[(x + 7, y)].symbol(), "界");
    assert_eq!(buffer[(x + 9, y)].symbol(), "🙂");
    assert_eq!(buffer[(x + 11, y)].symbol(), "e\u{301}");
    let gutter = text(&buffer, body.x, y, 6);
    key(&mut app, KeyCode::Right);
    let panned = draw(&mut app, 100, 30);
    assert_eq!(app.view.diff.horizontal, 8);
    assert_eq!(panned[(x, y)].symbol(), " "); // The viewport cuts the second cell of 界.
    assert_eq!(panned[(x + 1, y)].symbol(), "🙂");
    assert_eq!(panned[(x + 3, y)].symbol(), "e\u{301}");
    assert_eq!(text(&panned, body.x, y, 6), gutter);
    assert_eq!(panned[(body.x + body.width / 2, y)].symbol(), "│");
    assert!(
        app.term
            .drawn
            .text
            .iter()
            .all(|row| !row.text.chars().any(char::is_control))
    );
    for _ in 0..40 {
        key(&mut app, KeyCode::Right);
    }
    assert_eq!(app.view.diff.horizontal, app.term.drawn.diff_horizontal_max);
    let end = draw(&mut app, 100, 30);
    assert!(text(&end, x, y, 30).contains("\\u{1b}[31m\\r\\n END"));
    key(&mut app, KeyCode::Left);
    assert_eq!(
        app.view.diff.horizontal,
        app.term.drawn.diff_horizontal_max - 8
    );
    key(&mut app, KeyCode::Char(']')); // A one-file diff stays put, without clearing pan.
    assert_eq!(app.view.diff.file, 0);
}

#[test]
fn added_deleted_renamed_binary_mode_only_and_no_newline_have_honest_sides() {
    let mut app = review();
    draw(&mut app, 150, 45);
    for index in 1..6 {
        click(&mut app, DiffHit::File(index));
        let buffer = draw(&mut app, 150, 45);
        assert_eq!(app.view.diff.file, index);
        let body = region(&app, DiffHit::Body);
        let after = body.x + body.width / 2 + 1;
        match index {
            1 | 2 => {
                let empty = if index == 1 { body.x } else { after };
                let present = if index == 1 { after } else { body.x };
                assert_eq!(
                    text(&buffer, empty, 5, 18),
                    if index == 1 {
                        " Before · (absent)"
                    } else {
                        " After · (absent) "
                    }
                );
                assert_eq!(text(&buffer, empty, body.y + 1, 55).trim(), "");
                assert_eq!(
                    text(&buffer, present, body.y + 1, 6),
                    if index == 1 { "  1 + " } else { "  1 − " }
                );
            }
            3 => {
                assert_eq!(
                    text(&buffer, body.x, 5, 34),
                    " Before · docs/billing/preview.md "
                );
                assert_eq!(
                    text(&buffer, after, 5, 34),
                    " After · docs/billing/proration.md"
                );
                assert_eq!(
                    text(&buffer, after + 1, body.y + 6, 18),
                    "↳ No newline at EO"
                );
                assert_eq!(buffer[(after + 1, body.y + 6)].fg, app.term.palette.accent);
                assert_eq!(text(&buffer, body.x, body.y + 6, 55).trim(), "");
            }
            4 => {
                assert_eq!(
                    text(&buffer, body.x + 1, body.y, 37),
                    "Binary file changed · no text preview"
                );
                assert_eq!(app.term.drawn.text, []);
            }
            5 => {
                assert_eq!(text(&buffer, body.x + 1, body.y, 15), "old mode 100644");
                assert_eq!(text(&buffer, body.x + 1, body.y + 1, 15), "new mode 100755");
                assert_eq!(
                    text(&buffer, body.x + 1, body.y + 2, 18),
                    "No textual changes"
                );
                assert_eq!(app.term.drawn.text, []);
            }
            _ => unreachable!(),
        }
    }
    let mut renamed = sample::diff().files.remove(3);
    renamed.hunks.clear();
    let mut app = with_files(vec![renamed]);
    let buffer = draw(&mut app, 150, 45);
    let body = region(&app, DiffHit::Body);
    assert_eq!(text(&buffer, body.x + 1, body.y, 18), "No textual changes");
    assert_eq!(app.term.drawn.text, []);
}

#[test]
fn loading_stale_error_empty_and_legacy_views_do_not_show_another_held_diff() {
    for stale in [false, true] {
        let mut app = review();
        if stale {
            app.snapshot.diff.as_mut().unwrap().held = HeldActionId::new(99);
        } else {
            app.snapshot.diff = None;
        }
        let buffer = draw(&mut app, 150, 45);
        let body = region(&app, DiffHit::Body);
        assert_eq!(text(&buffer, 8, 1, 17), "Reading the diff…");
        assert_eq!(text(&buffer, body.x, body.y, 17), "Reading the diff…");
        assert_eq!(text(&buffer, 1, 3, 11), " Files · 0 ");
        key(&mut app, KeyCode::Char('a'));
        key(&mut app, KeyCode::Char('r'));
        assert_eq!(app.screen, Screen::Diff);
        assert_eq!(app.take_commands(), []);
    }
    for (message, tone) in [
        ("Couldn't read the approved head", Tone::Warn),
        ("git read failed", Tone::Error),
        ("+legacy patch", Tone::Ok),
        ("", Tone::Dim),
    ] {
        let mut app = review();
        let mut view = DiffView::new(HeldActionId::new(1), "read result");
        if !message.is_empty() {
            view.lines.push(Styled::new(message, tone));
        }
        app.snapshot.diff = Some(view);
        let buffer = draw(&mut app, 150, 45);
        let body = region(&app, DiffHit::Body);
        let shown = if message.is_empty() {
            "No changes in this diff."
        } else {
            message
        };
        assert_eq!(
            text(
                &buffer,
                body.x,
                body.y,
                u16::try_from(shown.chars().count()).unwrap()
            ),
            shown
        );
        theme::scoped(app.term.palette, || {
            assert_eq!(buffer[(body.x, body.y)].fg, theme::tone(tone).fg.unwrap());
        });
        assert_eq!(app.term.drawn.text, []);
    }
}

#[test]
fn truncated_hunks_warn_without_padding_the_declared_ranges() {
    let mut app = with_files(vec![long_file()]);
    let view = app.snapshot.diff.as_mut().unwrap();
    view.truncated = true;
    view.files[0].hunks.truncate(1);
    view.files[0].hunks[0].lines.truncate(1); // Still declares eighty lines, only one arrived.
    let buffer = draw(&mut app, 100, 30);
    assert_eq!(
        text(&buffer, 1, 2, 35),
        "Partial diff · patch cut/incomplete"
    );
    assert_eq!(buffer[(1, 2)].fg, app.term.palette.accent);
    assert_eq!(app.term.drawn.diff_scroll_max, 0);
    let body = region(&app, DiffHit::Body);
    assert_eq!(text(&buffer, body.x, body.y + 1, 7), "1000   ");
    assert_eq!(text(&buffer, body.x, body.y + 2, body.width / 2).trim(), "");
    key(&mut app, KeyCode::End);
    assert_eq!(app.view.diff.scroll, 0);

    // A patch cut just after a file header does not prove that the file is empty.
    let mut file = DiffFile::new(None, Some("new.rs".into()));
    file.status = DiffFileStatus::Added;
    let mut app = with_files(vec![file]);
    app.snapshot.diff.as_mut().unwrap().truncated = true;
    let buffer = draw(&mut app, 100, 30);
    let body = region(&app, DiffHit::Body);
    assert_eq!(
        text(&buffer, body.x + 1, body.y, 23),
        "No text hunks available"
    );
}

#[test]
fn back_click_and_approval_keys_preserve_the_originating_action_behavior() {
    for (letter, answer) in [('a', HeldAnswer::Approve), ('r', HeldAnswer::Reject)] {
        let mut app = review();
        draw(&mut app, 100, 30);
        key(&mut app, KeyCode::Char(letter));
        assert_eq!(app.screen, Screen::Bridge);
        assert_eq!(app.popup(), None);
        assert_eq!(
            app.take_commands(),
            [
                Command::ShowDiff { held: None },
                Command::AnswerHeld {
                    held: HeldActionId::new(1),
                    answer
                }
            ]
        );
        assert_eq!(app.view.diff, DiffUi::default());
    }
    let mut app = review();
    draw(&mut app, 100, 30);
    let origin = app.view.diff_from.clone();
    click(&mut app, DiffHit::Back);
    assert_eq!(app.screen, Screen::Bridge);
    assert_eq!(app.popup(), Some(Popup::Action));
    assert_eq!(app.view.action_open, origin);
    assert_eq!(app.take_commands(), [Command::ShowDiff { held: None }]);
}
