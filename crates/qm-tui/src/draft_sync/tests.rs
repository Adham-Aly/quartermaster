use super::*;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// A fresh data directory and scratch folder under the system's temporary folder.
struct Home(PathBuf);

impl Home {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "qm-draft-sync-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn dir(&self) -> DataDir {
        DataDir::with_scratch(&self.0, self.0.join("scratch"))
    }

    fn images(&self) -> PathBuf {
        self.dir().draft_images()
    }

    fn image_files(&self) -> Vec<PathBuf> {
        fs::read_dir(self.images())
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
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

/// The drafts the UI has sent to the core since the last call.
fn sent(app: &mut App) -> Vec<SavedDraft> {
    app.take_commands()
        .into_iter()
        .filter_map(|command| match command {
            Command::SaveDraft { draft } => Some(draft),
            _ => None,
        })
        .collect()
}

/// A draft with one pasted image, as the UI holds it.
fn with_image(text: &str) -> Draft {
    let image = PastedImage::new(1, ImageFormat::Png, png());
    let mut draft = Draft::new(format!("{text} {}", image.placeholder()));
    draft.images.push(image);
    draft
}

/// A new app whose first snapshot brought `saved` back.
fn restored(home: &Home, saved: &[SavedDraft]) -> (App, DraftSync) {
    let mut app = App::new();
    let mut sync = DraftSync::open(&home.dir());
    sync.restore(&mut app, saved);
    (app, sync)
}

#[test]
fn nothing_is_sent_before_the_first_snapshot_has_restored_the_drafts() {
    let home = Home::new();
    let mut app = App::new();
    let mut sync = DraftSync::open(&home.dir());
    app.draft = Draft::new("typed early");
    sync.tick(&mut app);
    assert_eq!(sent(&mut app), Vec::new());
    assert!(!home.images().exists());

    sync.restore(&mut app, &[]);
    sync.tick(&mut app);
    let drafts = sent(&mut app);
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].text, "typed early");
}

#[test]
fn a_draft_is_sent_when_it_changes_and_not_otherwise() {
    let home = Home::new();
    let (mut app, mut sync) = restored(&home, &[]);
    sync.tick(&mut app);
    assert_eq!(sent(&mut app), Vec::new(), "an empty prompt sends nothing");

    app.draft = Draft::new("hello");
    sync.tick(&mut app);
    let drafts = sent(&mut app);
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].target, DraftTarget::Bridge);
    assert_eq!(drafts[0].text, "hello");

    sync.tick(&mut app);
    assert!(
        sent(&mut app).is_empty(),
        "an unchanged draft is not sent again"
    );
}

#[test]
fn the_shown_session_and_the_parked_bridge_draft_are_sent_for_their_own_prompts() {
    let home = Home::new();
    let (mut app, mut sync) = restored(&home, &[]);
    app.draft = Draft::new("for the quartermaster");
    app.enter_session(crew());
    app.draft = Draft::new("for the crew");
    sync.tick(&mut app);

    let mut drafts = sent(&mut app);
    drafts.sort_by_key(|d| format!("{:?}", d.target));
    let texts: Vec<(DraftTarget, String)> =
        drafts.into_iter().map(|d| (d.target, d.text)).collect();
    assert!(texts.contains(&(
        DraftTarget::Session { agent: crew() },
        "for the crew".into()
    )));
    assert!(texts.contains(&(DraftTarget::Bridge, "for the quartermaster".into())));
}

#[test]
fn an_emptied_draft_is_sent_empty_so_the_core_forgets_it() {
    let home = Home::new();
    let (mut app, mut sync) = restored(&home, &[]);
    app.draft = Draft::new("hi");
    sync.tick(&mut app);
    sent(&mut app);

    app.draft = Draft::default();
    sync.tick(&mut app);
    let drafts = sent(&mut app);
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].target, DraftTarget::Bridge);
    assert!(drafts[0].is_empty());
}

#[test]
fn the_first_snapshot_restores_drafts_and_later_ones_change_nothing() {
    let home = Home::new();
    let first = SavedDraft::new(DraftTarget::Bridge, "one");
    let (mut app, mut sync) = restored(&home, &[first]);
    assert_eq!(app.draft.text, "one");

    let later = SavedDraft::new(DraftTarget::Bridge, "two");
    sync.restore(&mut app, &[later]);
    assert_eq!(
        app.draft.text, "one",
        "a later snapshot doesn't replace the draft"
    );
}

#[test]
fn a_restored_draft_does_not_replace_one_typed_since() {
    let home = Home::new();
    let mut app = App::new();
    app.draft = Draft::new("typed first");
    let mut sync = DraftSync::open(&home.dir());
    sync.restore(&mut app, &[SavedDraft::new(DraftTarget::Bridge, "saved")]);
    assert_eq!(app.draft.text, "typed first");
}

#[test]
fn a_restored_session_draft_comes_back_on_its_own_prompt() {
    let home = Home::new();
    let saved = SavedDraft::new(DraftTarget::Session { agent: crew() }, "tell the crew");
    let (app, _sync) = restored(&home, &[saved]);
    let drafts = &app.view.session.as_ref().unwrap().drafts;
    assert_eq!(drafts.get(&crew()).unwrap().text, "tell the crew");
}

#[test]
fn an_image_is_written_privately_once_and_kept_while_a_draft_names_it() {
    let home = Home::new();
    let (mut app, mut sync) = restored(&home, &[]);
    app.draft = with_image("look");
    sync.tick(&mut app);
    let drafts = sent(&mut app);
    assert_eq!(drafts[0].images.len(), 1);
    drop(sync);

    let files = home.image_files();
    assert_eq!(files.len(), 1);
    let mode = fs::metadata(&files[0]).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(fs::read(&files[0]).unwrap(), png());
}

#[test]
fn an_image_is_restored_from_its_file_for_a_restart() {
    let home = Home::new();
    let (mut app, mut sync) = restored(&home, &[]);
    app.draft = with_image("look");
    sync.tick(&mut app);
    let saved = sent(&mut app).remove(0);
    drop(sync);

    let (back, _sync) = restored(&home, &[saved]);
    assert_eq!(back.draft.images.len(), 1);
    assert_eq!(back.draft.images[0].bytes.as_slice(), png().as_slice());
    assert!(back.draft.text.contains("[Image #1]"));
}

#[test]
fn an_image_that_is_gone_or_not_what_it_says_is_dropped_and_the_text_kept() {
    let home = Home::new();
    let (mut app, mut sync) = restored(&home, &[]);
    app.draft = with_image("a");
    sync.tick(&mut app);
    let saved = sent(&mut app).remove(0);
    drop(sync);
    let file = home.image_files().remove(0);

    fs::write(&file, b"not an image").unwrap();
    let (back, _sync) = restored(&home, std::slice::from_ref(&saved));
    assert_eq!(back.draft.images, Vec::new());
    assert!(back.draft.text.starts_with("a "));

    fs::remove_file(&file).unwrap();
    let (back, _sync) = restored(&home, &[saved]);
    assert_eq!(back.draft.images, Vec::new());
    assert!(back.draft.text.starts_with("a "));
}

#[test]
fn an_image_token_that_would_leave_the_folder_is_never_opened() {
    let home = Home::new();
    let image = DraftImage::new(1, ImageFormat::Png, png().len(), "../../../etc/passwd");
    let mut saved = SavedDraft::new(DraftTarget::Bridge, format!("a {}", image_placeholder(1)));
    saved.images = vec![image];
    let (back, _sync) = restored(&home, &[saved]);
    assert_eq!(back.draft.images, Vec::new());
    assert!(back.draft.text.starts_with("a "));
}

#[test]
fn files_no_draft_names_are_removed_once_the_restore_is_done() {
    let home = Home::new();
    let (mut app, mut sync) = restored(&home, &[]);
    app.draft = with_image("look");
    sync.tick(&mut app);
    sent(&mut app);
    drop(sync);
    assert_eq!(home.image_files().len(), 1);

    // A new run restores nothing, so the file is named by no draft and goes at the next tick.
    let (mut app, mut sync) = restored(&home, &[]);
    sync.tick(&mut app);
    drop(sync);
    assert_eq!(home.image_files(), Vec::<PathBuf>::new());
}

#[test]
fn an_image_is_removed_once_the_draft_no_longer_names_it() {
    let home = Home::new();
    let (mut app, mut sync) = restored(&home, &[]);
    app.draft = with_image("look");
    sync.tick(&mut app);
    let saved = sent(&mut app).remove(0);
    drop(sync);
    assert_eq!(home.image_files().len(), 1);

    let (mut app, mut sync) = restored(&home, &[saved]);
    assert_eq!(
        app.draft.images.len(),
        1,
        "the restored draft holds its image"
    );
    app.draft = Draft::default();
    sync.tick(&mut app);
    drop(sync);
    assert_eq!(home.image_files(), Vec::<PathBuf>::new());
}
