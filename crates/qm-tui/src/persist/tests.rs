use super::*;
use crate::settings::{Field, FieldFor};
use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// A fresh data directory under the system's temporary folder.
struct Home(PathBuf);

impl Home {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "qm-ui-state-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn dir(&self) -> DataDir {
        DataDir::new(&self.0)
    }

    fn file(&self) -> PathBuf {
        self.0.join(FILE)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn png() -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&[7; 64]);
    bytes
}

fn crew() -> AgentRef {
    serde_json::from_str(r#"{"agent":"crewmate","task":"T-42"}"#).unwrap()
}

/// Opens a saver on `home`, applying what it holds to a new app.
fn open(home: &Home) -> (App, Persister) {
    let mut app = App::new();
    let persist = Persister::open(&home.dir(), &mut app);
    (app, persist)
}

#[test]
fn the_shared_state_survives_a_restart() {
    let home = Home::new();
    let (mut app, mut persist) = open(&home);
    app.left_width = 40;
    app.right_width = 50;
    app.scroll_from_bottom = 12;
    app.view.projects_scroll = 3;
    app.view.crew_scroll = 4;
    app.view.task_scroll = 5;
    app.view.diff_scroll = 7;
    app.view.settings.scroll = 8;
    app.view.settings.tab = Tab::Routing;
    app.view.settings.routing_row = 1;
    app.view.selected_crew = Some(crew());
    app.view.selected_project = Some(2);
    app.view.tool_groups.insert(ToolGroupKey {
        agent: crew(),
        first: 9,
    });
    app.open_popup(Popup::Help);
    app.open_popup(Popup::Settings);
    app.view.help_scroll = 6;
    app.focus = Pane::Settings;
    app.draft = Draft::new("half a thought and more");
    app.draft.cursor = Some(4);
    app.draft.anchor = Some(9);
    app.view.session = Some(SessionUi::default());
    app.view
        .session
        .as_mut()
        .unwrap()
        .drafts
        .insert(crew(), Draft::new("tell the crew"));
    persist.flush(&app);
    drop(persist);

    let (back, _persist) = open(&home);
    assert_eq!((back.left_width, back.right_width), (40, 50));
    assert_eq!(back.scroll_from_bottom, 12);
    assert_eq!(back.view.projects_scroll, 3);
    assert_eq!(back.view.crew_scroll, 4);
    assert_eq!(back.view.task_scroll, 5);
    assert_eq!(back.view.help_scroll, 6);
    assert_eq!(back.view.diff_scroll, 7);
    assert_eq!(back.view.settings.scroll, 8);
    assert_eq!(back.view.settings.tab, Tab::Routing);
    assert_eq!(back.view.settings.routing_row, 1);
    assert_eq!(back.view.selected_crew, Some(crew()));
    assert_eq!(back.view.selected_project, Some(2));
    assert_eq!(back.view.tool_groups.len(), 1);
    assert_eq!(
        back.view.popups.iter().map(|o| o.popup).collect::<Vec<_>>(),
        [Popup::Help, Popup::Settings]
    );
    assert_eq!(back.focus, Pane::Settings);
    assert_eq!(back.draft.text, "half a thought and more");
    assert_eq!(back.draft.selection(), Some((4, 9)));
    let sessions = &back.view.session.as_ref().unwrap().drafts;
    assert_eq!(sessions.get(&crew()).unwrap().text, "tell the crew");
}

#[test]
fn the_file_is_private_versioned_and_written_without_leftovers() {
    let home = Home::new();
    let (app, mut persist) = open(&home);
    persist.flush(&app);
    let mode = fs::metadata(home.file()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let text = fs::read_to_string(home.file()).unwrap();
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["version"], VERSION);
    assert!(!home.0.join("ui.json.tmp").exists());
}

#[test]
fn changes_are_written_a_second_after_the_last_one() {
    let home = Home::new();
    let (mut app, mut persist) = open(&home);
    let start = Instant::now();
    // Nothing changed since loading: nothing is written, however long it takes.
    persist.tick(&app, start);
    persist.tick(&app, start + DEBOUNCE * 3);
    app.draft = Draft::new("a");
    persist.tick(&app, start + DEBOUNCE * 4);
    app.draft = Draft::new("ab");
    persist.tick(&app, start + DEBOUNCE * 4 + Duration::from_millis(700));
    // 1.2 s after the first change, but only half a second after the last.
    persist.tick(&app, start + DEBOUNCE * 4 + Duration::from_millis(1200));
    drop(persist);
    assert!(!home.file().exists());

    let (mut app, mut persist) = open(&home);
    app.draft = Draft::new("abc");
    persist.tick(&app, start);
    persist.tick(&app, start + DEBOUNCE + Duration::from_millis(10));
    drop(persist);
    assert!(fs::read_to_string(home.file()).unwrap().contains("abc"));
}

#[test]
fn loading_never_fails() {
    for text in [
        "",
        "not json {",
        "[]",
        "null",
        r#"{"version": 99, "left_width": "wide", "popups": 3, "scroll": []}"#,
        r#"{"popups":[{"popup":"Nope","anchor":"Centre"}],"focus":"Sideways"}"#,
    ] {
        let home = Home::new();
        fs::write(home.file(), text).unwrap();
        let (app, _persist) = open(&home);
        assert!(app.view.popups.is_empty(), "{text}");
        assert_eq!(app.focus, Pane::Input, "{text}");
        assert!(app.draft.is_empty(), "{text}");
    }
}

#[test]
fn one_bad_value_spoils_only_itself() {
    let home = Home::new();
    fs::write(
        home.file(),
        r#"{"left_width": "wide", "right_width": 9999, "bridge_draft": {"text": "kept", "cursor": "x"},
            "scroll": {"crew": 5, "task": -1, "help": 99999999999},
            "popups": [{"popup":"Help","anchor":"Centre"}, 7, {"popup":"Help","anchor":"Centre"}],
            "routing_row": 17, "settings_tab": "Routing"}"#,
    )
    .unwrap();
    let (app, _persist) = open(&home);
    assert_eq!(app.draft.text, "kept");
    assert_eq!(app.draft.cursor, None);
    assert_eq!(app.view.crew_scroll, 5);
    assert_eq!(app.view.task_scroll, 0);
    assert_eq!(app.view.help_scroll, MAX_SCROLL);
    assert_eq!(app.view.popups.len(), 1);
    // A saved selection from a build with more routing rows loads inside this one's.
    assert_eq!(app.view.settings.routing_row, ROUTING_ROWS - 1);
    assert_eq!(app.view.settings.tab, Tab::Routing);
}

#[test]
fn widths_come_from_the_old_file_until_there_is_a_new_one() {
    let home = Home::new();
    fs::write(
        home.0.join(prefs::FILE),
        "left_width = 33\nright_width = 44\n",
    )
    .unwrap();
    let (app, mut persist) = open(&home);
    assert_eq!((app.left_width, app.right_width), (33, 44));
    persist.flush(&app);
    drop(persist);
    // Once ui.json holds the widths, the old file no longer counts.
    fs::write(
        home.0.join(prefs::FILE),
        "left_width = 21\nright_width = 25\n",
    )
    .unwrap();
    let (again, _persist) = open(&home);
    assert_eq!((again.left_width, again.right_width), (33, 44));
}

#[test]
fn screens_that_need_live_state_come_back_as_the_bridge() {
    let home = Home::new();
    fs::write(
        home.file(),
        r#"{"screen": "Diff", "focus": "Session", "popups": []}"#,
    )
    .unwrap();
    let (app, _persist) = open(&home);
    assert_eq!(app.screen, Screen::Bridge);
    assert_eq!(app.focus, Pane::Input);
    let home = Home::new();
    fs::write(home.file(), r#"{"focus": "Settings", "popups": []}"#).unwrap();
    assert_eq!(open(&home).0.focus, Pane::Input);
}

#[test]
fn an_api_key_being_typed_is_never_written() {
    let home = Home::new();
    let (mut app, mut persist) = open(&home);
    app.open_popup(Popup::Settings);
    app.view.settings.field = Some(Field {
        purpose: FieldFor::ApiKey("openai"),
        text: "sk-secret-do-not-write".to_owned(),
        base_url: None,
    });
    app.draft = Draft::new("hello");
    persist.flush(&app);
    let text = fs::read_to_string(home.file()).unwrap();
    assert!(text.contains("hello"));
    assert!(!text.contains("sk-secret"));
}

#[test]
fn a_draft_image_is_kept_for_a_restart_and_removed_once_nothing_names_it() {
    let home = Home::new();
    let (mut app, mut persist) = open(&home);
    let image = PastedImage::new(1, ImageFormat::Png, png());
    app.draft = Draft::new(format!("look {} here", image.placeholder()));
    app.draft.images.push(image);
    // An image whose placeholder is gone from the text is not kept.
    app.draft
        .images
        .push(PastedImage::new(2, ImageFormat::Png, png()));
    persist.flush(&app);
    let folder = home.dir().draft_images();
    let files: Vec<_> = fs::read_dir(&folder).unwrap().flatten().collect();
    assert_eq!(files.len(), 1);
    let mode = fs::metadata(files[0].path()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    drop(persist);

    let (mut back, mut persist) = open(&home);
    assert_eq!(back.draft.images.len(), 1);
    assert_eq!(back.draft.images[0].bytes.as_slice(), png().as_slice());
    assert!(back.draft.text.contains("[Image #1]"));
    // Sending clears the draft; the file goes with the next save.
    back.draft = Draft::default();
    persist.flush(&back);
    assert_eq!(fs::read_dir(&folder).unwrap().count(), 0);
}

#[test]
fn an_image_that_is_gone_or_not_what_it_says_is_dropped_from_the_draft() {
    let home = Home::new();
    let (mut app, mut persist) = open(&home);
    let image = PastedImage::new(1, ImageFormat::Png, png());
    app.draft = Draft::new(format!("a {}", image.placeholder()));
    app.draft.images.push(image);
    persist.flush(&app);
    drop(persist);
    let folder = home.dir().draft_images();
    let file = fs::read_dir(&folder).unwrap().next().unwrap().unwrap();
    fs::write(file.path(), b"not an image").unwrap();
    let (back, _persist) = open(&home);
    assert_eq!(back.draft.images.len(), 0);
    assert!(back.draft.text.starts_with("a "));
    // A token that tries to leave the folder is never opened.
    let text = fs::read_to_string(home.file())
        .unwrap()
        .replace(&file.file_name().to_string_lossy()[..16], "../../../etc");
    fs::write(home.file(), text).unwrap();
    assert_eq!(open(&home).0.draft.images.len(), 0);
}

#[test]
fn the_parked_bridge_draft_and_the_shown_session_draft_both_come_back() {
    let home = Home::new();
    let (mut app, mut persist) = open(&home);
    app.draft = Draft::new("for the quartermaster");
    app.enter_session(crew());
    app.draft = Draft::new("for the crew");
    persist.flush(&app);
    drop(persist);
    let (back, _persist) = open(&home);
    assert_eq!(back.draft.text, "for the quartermaster");
    let sessions = &back.view.session.as_ref().unwrap().drafts;
    assert_eq!(sessions.get(&crew()).unwrap().text, "for the crew");
}
