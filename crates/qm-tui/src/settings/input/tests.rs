use super::*;
use crate::settings::model::{Model, Placement};
use ratatui::{Terminal, backend::TestBackend};

fn board() -> App {
    let mut app = App::new();
    app.open_popup(Popup::Settings);
    assert_eq!(app.take_commands(), [Command::RefreshQuotas]);
    app.view.settings.loaded = true;
    app.view.settings.tab = Tab::ModelLevels;
    let mut opus = Model::new(
        ModelId::new("claude:opus-5-5").unwrap(),
        "Opus 5.5",
        "Claude Max",
    );
    opus.supported = vec![
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::ExtraHigh,
        Effort::Max,
    ];
    opus.placements
        .push(Placement::new(Tier::High, Some(Effort::High)));
    opus.listed = true;
    let mut hidden = Model::new(ModelId::new("api:hidden-id").unwrap(), "Hidden", "Work key");
    hidden.supported = vec![Effort::ExtraHigh];
    hidden.listed = false;
    let mut basic = Model::new(ModelId::new("api:basic").unwrap(), "Basic", "Work key");
    basic.placements.push(Placement::new(Tier::Low, None));
    app.view.settings.model.models = vec![opus, hidden, basic];
    screen(&mut app, 150, 45);
    app
}

fn press(app: &mut App, code: KeyCode) {
    app.on_settings_key(KeyEvent::new(code, KeyModifiers::NONE), Instant::now());
}

fn screen(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn options(app: &App) -> &[(String, PickValue)] {
    &app.view.settings.picker.as_ref().unwrap().options
}

fn choose_value(app: &mut App, value: &PickValue) {
    let index = options(app).iter().position(|(_, v)| v == value).unwrap();
    app.settings_click(SettingsHit::PickerItem(index), false, Instant::now());
}

fn catalogue(app: &mut App) -> Vec<PickOption> {
    press(app, KeyCode::Char('a'));
    options(app).to_vec()
}

fn refresh_models(app: &mut App, models: Vec<Model>) {
    let mut snapshot = app.snapshot.clone();
    snapshot.settings.models = models;
    app.on_update(qm_types::api::Update::Snapshot(Box::new(snapshot)));
}

fn refresh_model(name: &str, supported: Vec<Effort>) -> Model {
    let mut model = Model::new(ModelId::new(name).unwrap(), name, "Work key");
    model.supported = supported;
    model
}

fn choose_label(app: &mut App, label: &str) {
    let index = options(app)
        .iter()
        .position(|(text, _)| text.starts_with(label))
        .unwrap();
    app.settings_click(SettingsHit::PickerItem(index), false, Instant::now());
}

fn picker_before_refresh(before_destination: bool, supported: &[Effort]) -> App {
    let mut app = board();
    refresh_models(
        &mut app,
        ["A", "B"]
            .map(|name| refresh_model(name, supported.to_vec()))
            .to_vec(),
    );
    catalogue(&mut app);
    if !before_destination {
        choose_label(&mut app, "B (");
    }
    app
}

#[test]
fn snapshot_refresh_keeps_selected_model_in_both_picker_stages() {
    for before_destination in [true, false] {
        for order in [vec!["B", "C"], vec!["B", "A"], vec!["C", "A", "B"]] {
            let mut app = picker_before_refresh(before_destination, &[Effort::High]);
            refresh_models(
                &mut app,
                order
                    .iter()
                    .map(|name| refresh_model(name, vec![Effort::High]))
                    .collect(),
            );
            if before_destination {
                choose_label(&mut app, "B (high)");
            }
            choose_value(&mut app, &PickValue::Zone(Zone::Tier(Tier::Premium)));
            assert_eq!(
                app.take_commands(),
                vec![Command::PlaceModel {
                    model: ModelId::new("B").unwrap(),
                    from: None,
                    effort: Some(Effort::High),
                    to: Tier::Premium,
                }],
                "before_destination={before_destination}, order={order:?}"
            );
            assert_eq!(
                app.view.settings.board,
                Some(BoardItem::Placed {
                    model: order.iter().position(|&name| name == "B").unwrap(),
                    tier: Tier::Premium,
                    effort: Some(Effort::High),
                })
            );
        }
    }
}

#[test]
fn snapshot_refresh_disappearing_model_emits_no_add_in_either_picker_stage() {
    for before_destination in [true, false] {
        let mut app = picker_before_refresh(before_destination, &[Effort::High]);
        refresh_models(
            &mut app,
            ["A", "C"]
                .map(|name| refresh_model(name, vec![Effort::High]))
                .to_vec(),
        );
        if before_destination {
            choose_label(&mut app, "B (high)");
        } else {
            choose_value(&mut app, &PickValue::Zone(Zone::Tier(Tier::Premium)));
        }
        assert!(app.view.settings.picker.is_none());
        assert_eq!(app.take_commands(), [] as [Command; 0]);
        assert!(
            app.view
                .settings
                .model
                .models
                .iter()
                .all(|m| m.placements.is_empty())
        );
    }
}

#[test]
fn snapshot_refresh_revalidates_effort_support_in_both_picker_stages() {
    for before_destination in [true, false] {
        for (old, new) in [
            (vec![Effort::High], vec![Effort::Low]),
            (vec![Effort::High], vec![]),
            (vec![], vec![Effort::High]),
        ] {
            let mut app = picker_before_refresh(before_destination, &old);
            refresh_models(
                &mut app,
                vec![refresh_model("B", new), refresh_model("C", old)],
            );
            if before_destination {
                choose_label(&mut app, "B (");
            } else {
                choose_value(&mut app, &PickValue::Zone(Zone::Tier(Tier::Premium)));
            }
            assert!(app.view.settings.picker.is_none());
            assert_eq!(app.take_commands(), [] as [Command; 0]);
            assert!(
                app.view
                    .settings
                    .model
                    .models
                    .iter()
                    .all(|m| m.placements.is_empty())
            );
        }
    }
}

#[test]
fn snapshot_refresh_revalidates_duplicate_destination_for_selected_model() {
    for before_destination in [true, false] {
        let mut app = picker_before_refresh(before_destination, &[Effort::High]);
        let mut selected = refresh_model("B", vec![Effort::High]);
        selected
            .placements
            .push(Placement::new(Tier::Premium, Some(Effort::High)));
        refresh_models(
            &mut app,
            vec![selected, refresh_model("C", vec![Effort::High])],
        );
        if before_destination {
            choose_label(&mut app, "B (high)");
            assert!(
                !options(&app)
                    .iter()
                    .any(|(_, v)| *v == PickValue::Zone(Zone::Tier(Tier::Premium)))
            );
            choose_value(&mut app, &PickValue::Zone(Zone::Tier(Tier::Medium)));
            assert_eq!(
                app.take_commands(),
                vec![Command::PlaceModel {
                    model: ModelId::new("B").unwrap(),
                    from: None,
                    effort: Some(Effort::High),
                    to: Tier::Medium,
                }]
            );
        } else {
            choose_value(&mut app, &PickValue::Zone(Zone::Tier(Tier::Premium)));
            assert_eq!(app.take_commands(), [] as [Command; 0]);
            assert_eq!(app.view.settings.model.models[0].placements.len(), 1);
        }
        assert_eq!(
            app.view.settings.model.models[1].placements,
            [] as [Placement; 0]
        );
    }
}

#[test]
fn catalogue_is_all_models_times_supported_efforts_including_placed_and_unlisted() {
    let mut app = board();
    let entries = catalogue(&mut app);
    assert_eq!(entries.len(), 7);
    for effort in [
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::ExtraHigh,
        Effort::Max,
    ] {
        assert!(entries.iter().any(|(_, v)| *v
            == PickValue::Combo {
                model: ModelId::new("claude:opus-5-5").unwrap(),
                effort: Some(effort)
            }));
    }
    assert!(
        entries
            .iter()
            .any(|(label, v)| label.contains("Hidden (extra high)")
                && *v
                    == PickValue::Combo {
                        model: ModelId::new("api:hidden-id").unwrap(),
                        effort: Some(Effort::ExtraHigh)
                    })
    );
    let no_effort: Vec<_> = entries
        .iter()
        .filter(
            |(_, v)| matches!(v, PickValue::Combo { model, .. } if model.as_str() == "api:basic"),
        )
        .collect();
    assert_eq!(no_effort.len(), 1);
    assert!(no_effort[0].0.contains("Basic (no effort)"));
    assert_eq!(
        no_effort[0].1,
        PickValue::Combo {
            model: ModelId::new("api:basic").unwrap(),
            effort: None
        }
    );
    assert!(!entries.iter().any(|(_, v)| *v
        == PickValue::Combo {
            model: ModelId::new("claude:opus-5-5").unwrap(),
            effort: None
        }));
    assert_eq!(app.take_commands(), [] as [Command; 0]);
}

#[test]
fn search_matches_name_id_account_effort_and_multiple_case_insensitive_words() {
    for (query, expected) in [
        ("OPUS", 5),
        ("claude:opus-5-5", 5),
        ("Claude Max", 5),
        ("MAX", 5),
        ("opus (max)", 1),
        ("opus low", 1),
        ("opus extra HIGH", 1),
        ("api:hidden-id", 1),
        ("Work key", 2),
        ("no effort", 1),
        ("absent", 0),
    ] {
        let mut app = board();
        press(&mut app, KeyCode::Char('/'));
        for c in query.chars() {
            press(&mut app, KeyCode::Char(c));
        }
        let picker = app.view.settings.picker.as_ref().unwrap();
        assert_eq!(picker.visible().len(), expected, "{query}");
        assert_eq!(
            picker.options.len(),
            7,
            "search must not mutate the catalogue"
        );
        assert_eq!(app.take_commands(), [] as [Command; 0]);
    }
}

#[test]
fn pasted_search_keeps_cursor_on_match_and_enter_opens_destination_not_effort() {
    let mut app = board();
    press(&mut app, KeyCode::Char('a'));
    app.settings_paste("Hidden extra HIGH");
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        app.view.settings.picker.as_ref().unwrap().purpose,
        PickFor::AddTo {
            model: ModelId::new("api:hidden-id").unwrap(),
            effort: Some(Effort::ExtraHigh)
        }
    );
    assert_eq!(options(&app).len(), 4);
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        app.take_commands(),
        vec![Command::PlaceModel {
            model: ModelId::new("api:hidden-id").unwrap(),
            from: None,
            effort: Some(Effort::ExtraHigh),
            to: Tier::Premium,
        }]
    );
}

#[test]
fn repeated_add_keeps_catalogue_fixed_and_excludes_each_identical_destination() {
    let mut app = board();
    let original = catalogue(&mut app);
    for tier in [Tier::Premium, Tier::Medium, Tier::Low] {
        choose_value(
            &mut app,
            &PickValue::Combo {
                model: ModelId::new("claude:opus-5-5").unwrap(),
                effort: Some(Effort::High),
            },
        );
        assert!(
            !options(&app)
                .iter()
                .any(|(_, value)| *value == PickValue::Zone(Zone::Tier(Tier::High)))
        );
        choose_value(&mut app, &PickValue::Zone(Zone::Tier(tier)));
        assert_eq!(
            app.take_commands(),
            vec![Command::PlaceModel {
                model: ModelId::new("claude:opus-5-5").unwrap(),
                from: None,
                effort: Some(Effort::High),
                to: tier,
            }]
        );
        let m = &app.view.settings.model.models[0];
        assert!(m.placement(Tier::High, Some(Effort::High)).is_some());
        assert!(m.placement(tier, Some(Effort::High)).is_some());
        assert_eq!(catalogue(&mut app), original);
    }
    choose_value(
        &mut app,
        &PickValue::Combo {
            model: ModelId::new("claude:opus-5-5").unwrap(),
            effort: Some(Effort::High),
        },
    );
    assert_eq!(options(&app), []);
    assert!(screen(&mut app, 100, 30).contains("Already in every tier"));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.take_commands(), [] as [Command; 0]);
}

#[test]
fn a_different_effort_can_be_added_to_the_same_tier_and_none_only_to_no_effort_model() {
    let mut app = board();
    catalogue(&mut app);
    choose_value(
        &mut app,
        &PickValue::Combo {
            model: ModelId::new("claude:opus-5-5").unwrap(),
            effort: Some(Effort::Low),
        },
    );
    assert_eq!(options(&app).len(), 4);
    choose_value(&mut app, &PickValue::Zone(Zone::Tier(Tier::High)));
    assert_eq!(app.view.settings.model.models[0].placements.len(), 2);
    app.take_commands();
    catalogue(&mut app);
    choose_value(
        &mut app,
        &PickValue::Combo {
            model: ModelId::new("api:basic").unwrap(),
            effort: None,
        },
    );
    assert_eq!(options(&app).len(), 3);
    choose_value(&mut app, &PickValue::Zone(Zone::Tier(Tier::Medium)));
    assert_eq!(
        app.take_commands(),
        vec![Command::PlaceModel {
            model: ModelId::new("api:basic").unwrap(),
            from: None,
            effort: None,
            to: Tier::Medium,
        }]
    );
    app.add_combo(ModelId::new("claude:opus-5-5").unwrap(), None, Tier::Low);
    app.add_combo(
        ModelId::new("api:hidden-id").unwrap(),
        Some(Effort::Max),
        Tier::Low,
    );
    assert_eq!(
        app.take_commands(),
        [] as [Command; 0],
        "unsupported additions never emit commands"
    );
}

#[test]
fn stale_destination_choices_cannot_emit_duplicate_adds_or_merge_moves() {
    let mut app = board();
    catalogue(&mut app);
    choose_value(
        &mut app,
        &PickValue::Combo {
            model: ModelId::new("claude:opus-5-5").unwrap(),
            effort: Some(Effort::High),
        },
    );
    // Another snapshot has placed this combination after the destination picker opened.
    app.view.settings.model.models[0]
        .placements
        .push(Placement::new(Tier::Medium, Some(Effort::High)));
    choose_value(&mut app, &PickValue::Zone(Zone::Tier(Tier::Medium)));
    assert_eq!(app.take_commands(), [] as [Command; 0]);
    let source = BoardItem::Placed {
        model: 0,
        tier: Tier::High,
        effort: Some(Effort::High),
    };
    app.view.settings.board = Some(source);
    press(&mut app, KeyCode::Char('m'));
    assert!(
        !options(&app)
            .iter()
            .any(|(_, value)| *value == PickValue::Zone(Zone::Tier(Tier::Medium)))
    );
    app.move_item(source, Zone::Tier(Tier::Medium));
    assert_eq!(app.take_commands(), [] as [Command; 0]);
    assert_eq!(app.view.settings.model.models[0].placements.len(), 2);
    // Even optimistic application of a duplicate move preserves the source.
    app.view.settings.model.apply(&Command::PlaceModel {
        model: ModelId::new("claude:opus-5-5").unwrap(),
        from: Some(Tier::High),
        effort: Some(Effort::High),
        to: Tier::Medium,
    });
    assert_eq!(app.view.settings.model.models[0].placements.len(), 2);
}

#[test]
fn drag_duplicates_are_not_drop_targets_and_valid_move_and_remove_preserve_effort() {
    let mut app = board();
    let source = BoardItem::Placed {
        model: 0,
        tier: Tier::High,
        effort: Some(Effort::High),
    };
    app.view.settings.model.models[0]
        .placements
        .push(Placement::new(Tier::Medium, Some(Effort::High)));
    app.settings_click(SettingsHit::Board(source), false, Instant::now());
    app.settings_drag_to(Some(Hit::Settings(SettingsHit::Zone(Zone::Tier(
        Tier::Medium,
    )))));
    assert_eq!(app.view.settings.drag.unwrap().over, None);
    app.settings_drop(Some(Hit::Settings(SettingsHit::Zone(Zone::Tier(
        Tier::Medium,
    )))));
    assert_eq!(app.take_commands(), [] as [Command; 0]);
    app.settings_click(SettingsHit::Board(source), false, Instant::now());
    app.settings_drop(Some(Hit::Settings(SettingsHit::Zone(Zone::Tier(
        Tier::Low,
    )))));
    assert_eq!(
        app.take_commands(),
        vec![Command::PlaceModel {
            model: ModelId::new("claude:opus-5-5").unwrap(),
            from: Some(Tier::High),
            effort: Some(Effort::High),
            to: Tier::Low,
        }]
    );
    press(&mut app, KeyCode::Char('x'));
    assert_eq!(
        app.take_commands(),
        vec![Command::RemovePlacement {
            model: ModelId::new("claude:opus-5-5").unwrap(),
            tier: Tier::Low,
            effort: Some(Effort::High),
        }]
    );
    assert!(
        app.view.settings.model.models[0]
            .placement(Tier::Medium, Some(Effort::High))
            .is_some()
    );
}

#[test]
fn placed_context_menu_has_no_add_submenu_or_effort_control() {
    for right_click in [false, true] {
        let mut app = board();
        let source = BoardItem::Placed {
            model: 0,
            tier: Tier::High,
            effort: Some(Effort::High),
        };
        if right_click {
            let row = app
                .term
                .drawn
                .hits
                .rect_of(Hit::Settings(SettingsHit::Board(source)))
                .unwrap();
            app.settings_right_click(SettingsHit::Board(source), Position::new(row.x, row.y));
        } else {
            app.view.settings.board = Some(source);
            press(&mut app, KeyCode::Char(' '));
        }
        let menu = app.view.menu.as_ref().expect("placed combination's menu");
        assert_eq!(menu.target, MenuTarget::Model(source));
        assert_eq!(
            menu.items,
            [REMOVE_FROM_HIGH, REMOVE_FROM_ALL, ALWAYS_USE],
            "only removal and pin actions; no add submenu or effort control"
        );
        assert!(app.view.settings.picker.is_none());
        assert_eq!(app.take_commands(), [] as [Command; 0]);
    }
}

#[test]
fn placed_combination_has_no_separate_effort_shortcut() {
    let mut app = board();
    app.view.settings.board = Some(BoardItem::Placed {
        model: 0,
        tier: Tier::High,
        effort: Some(Effort::High),
    });
    press(&mut app, KeyCode::Char('e'));
    assert!(
        app.view.settings.picker.is_none(),
        "no separate board effort picker"
    );
    assert_eq!(app.take_commands(), [] as [Command; 0]);
}

#[test]
fn render_board_and_catalogue_at_supported_sizes_in_light_and_dark_palettes() {
    for (width, height) in [(150, 45), (120, 40), (119, 40), (100, 30)] {
        for light in [false, true] {
            let mut app = board();
            let colors = qm_types::wire::Colors {
                bg: Some(if light { [255; 3] } else { [0; 3] }),
                ..Default::default()
            };
            app.term.palette = crate::theme::fit_reported(&colors);
            let shown = screen(&mut app, width, height);
            assert!(
                shown.contains("Opus 5.5 (high) · Claude Max"),
                "{width}×{height}:\n{shown}"
            );
            assert!(shown.contains("Basic (no effort)"));
            assert!(shown.contains("Add model…"));
            assert!(!shown.contains("Tier default"));
            assert!(!shown.contains("your setting"));
            assert!(!shown.contains("NOT IN USE"));
            let add = app
                .term
                .drawn
                .hits
                .rect_of(Hit::Settings(SettingsHit::Find))
                .unwrap();
            assert!(add.width > 0);
            app.settings_click(SettingsHit::Find, false, Instant::now());
            app.settings_paste("opus extra high");
            let shown = screen(&mut app, width, height);
            assert!(
                shown.contains("Opus 5.5 (extra high)"),
                "{width}×{height}:\n{shown}"
            );
            assert!(shown.contains("find: opus extra high"));
            assert!(
                app.term
                    .drawn
                    .hits
                    .rect_of(Hit::Settings(SettingsHit::PickerItem(3)))
                    .is_some()
            );
            app.settings_paste(" nonexistent");
            assert!(screen(&mut app, width, height).contains("No model matches"));
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.take_commands(), [] as [Command; 0]);
        }
    }
}

#[test]
fn a_removed_final_placement_stays_in_catalogue_and_selection_clears() {
    let mut app = board();
    app.view.settings.board = Some(BoardItem::Placed {
        model: 0,
        tier: Tier::High,
        effort: Some(Effort::High),
    });
    let original = app.picker_options(PickFor::Find).unwrap().0;
    press(&mut app, KeyCode::Delete);
    assert!(app.view.settings.board.is_none());
    assert_eq!(
        app.view.settings.model.models[0].placements,
        [] as [Placement; 0]
    );
    assert_eq!(catalogue(&mut app), original);
}

#[test]
fn keyboard_move_and_remove_only_affect_the_selected_combination() {
    let mut app = board();
    app.view.settings.model.models[0]
        .placements
        .push(Placement::new(Tier::High, Some(Effort::Low)));
    press(&mut app, KeyCode::Home);
    press(&mut app, KeyCode::Char('m'));
    choose_value(&mut app, &PickValue::Zone(Zone::Tier(Tier::Medium)));
    assert_eq!(
        app.take_commands(),
        vec![Command::PlaceModel {
            model: ModelId::new("claude:opus-5-5").unwrap(),
            from: Some(Tier::High),
            effort: Some(Effort::High),
            to: Tier::Medium,
        }]
    );
    assert!(
        app.view.settings.model.models[0]
            .placement(Tier::High, Some(Effort::Low))
            .is_some()
    );
    press(&mut app, KeyCode::Backspace);
    assert_eq!(
        app.take_commands(),
        vec![Command::RemovePlacement {
            model: ModelId::new("claude:opus-5-5").unwrap(),
            tier: Tier::Medium,
            effort: Some(Effort::High),
        }]
    );
    assert_eq!(
        app.view.settings.model.models[0].placements,
        vec![Placement::new(Tier::High, Some(Effort::Low))]
    );
}

#[test]
fn empty_board_still_offers_add_and_empty_catalogue_is_safe() {
    let mut app = board();
    for m in &mut app.view.settings.model.models {
        m.placements.clear();
    }
    press(&mut app, KeyCode::Char('a'));
    assert_eq!(options(&app).len(), 7);
    press(&mut app, KeyCode::Esc);
    app.view.settings.model.models.clear();
    assert!(screen(&mut app, 100, 30).contains("Add model…"));
    press(&mut app, KeyCode::Char('/'));
    assert_eq!(options(&app), []);
    assert!(screen(&mut app, 100, 30).contains("No model matches"));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.take_commands(), [] as [Command; 0]);
}

#[test]
fn quartermaster_auto_effort_label_distinguishes_auto_and_explicit_models() {
    for (model, label) in [
        (QuartermasterModel::Auto, "Auto · selected entry's effort"),
        (
            QuartermasterModel::Model(ModelId::new("claude:opus-5-5").unwrap()),
            "Auto · its model's tier default",
        ),
    ] {
        let mut app = board();
        app.view.settings.tab = Tab::Routing;
        app.view.settings.model.quartermaster = model;
        press(&mut app, KeyCode::Char('e'));
        assert_eq!(
            options(&app)[0],
            (label.to_owned(), PickValue::Effort(None))
        );
        assert_eq!(app.take_commands(), [] as [Command; 0]);
    }
}

#[test]
fn unresolved_placement_uses_a_default_label_not_an_effort_control() {
    let mut app = board();
    app.view.settings.model.models[0].placements[0].effort = None;
    let shown = screen(&mut app, 100, 30);
    assert!(shown.contains("Opus 5.5 (default)"));
    assert!(!shown.contains("Tier default"));
}

#[test]
fn large_catalogue_can_reach_its_last_combination_and_search_does_not_shrink_it() {
    let mut app = board();
    app.view.settings.model = crate::sample::settings();
    app.view.settings.model.login = None;
    screen(&mut app, 100, 30);
    let all = catalogue(&mut app);
    let expected: usize = app
        .view
        .settings
        .model
        .models
        .iter()
        .map(|m| m.supported.len().max(1))
        .sum();
    assert_eq!(all.len(), expected);
    assert!(all.len() > 200);
    press(&mut app, KeyCode::End);
    let last = all.len() - 1;
    assert_eq!(app.view.settings.picker.as_ref().unwrap().cursor, last);
    screen(&mut app, 100, 30);
    assert!(
        app.term
            .drawn
            .hits
            .rect_of(Hit::Settings(SettingsHit::PickerItem(last)))
            .is_some()
    );
    app.settings_paste("not-a-model");
    assert_eq!(
        app.view.settings.picker.as_ref().unwrap().visible(),
        [] as [usize; 0]
    );
    assert_eq!(options(&app), all);
    assert_eq!(app.take_commands(), [] as [Command; 0]);
}
