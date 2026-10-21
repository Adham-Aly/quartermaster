//! The internal API between the core and the terminal UI.
//!
//! The core is a library with no dependency on the UI. The UI sends [`Command`]s and renders the
//! [`Update`]s it receives; both are plain serde data, so the in-process [`link`] here could be
//! kept as it is: the background server holds both ends in one process and its socket carries
//! terminal frames ([`crate::wire`]), not these messages.
//! `qm-tui` depends on `qm-types` only, which enforces the boundary at compile time.
//!
//! Today the link is a pair of unbounded Tokio channels: the core never blocks on a slow UI, and
//! the UI can drain updates without blocking from its render loop ([`CoreClient::try_recv`]).
//!
//! **Additive only.** New screens add variants to [`Command`] and [`Update`] and fields (with
//! defaults) to the view types; nothing existing changes. Both enums are
//! `#[non_exhaustive]`, so an addition never breaks another crate's `match`. The view types carry
//! display-ready facts, never handles into the core, and are `#[non_exhaustive]` too: other crates
//! build them with `new(...)` or `default()` and then set fields, never with a struct literal, so a
//! field added later (with a `#[serde(default)]`) breaks nobody.

use crate::domain::{Autonomy, DeliveryMode, Effort, MessageKind, MessageSource, Provider, Tier};
use crate::ids::{AccountId, HeldActionId, MessageId, ModelId, SecondmateId, TaskId};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio::sync::mpsc;

mod image;
pub use image::{
    ImageBytes, ImageFormat, MAX_IMAGE_BYTES, PastedImage, placeholder as image_placeholder,
};

/// An agent the UI can address: the Quartermaster, or a worker whose session you opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "agent", rename_all = "snake_case")]
pub enum AgentRef {
    Quartermaster,
    Secondmate {
        id: SecondmateId,
    },
    /// A crewmate, by its task.
    Crewmate {
        task: TaskId,
    },
}

/// From the UI to the core.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Command {
    /// Your message. To the Quartermaster it starts (or queues for) a turn; to a worker it goes in
    /// unmarked, as direct intervention. The core neutralises the reserved `<qm:` prefix, so a
    /// marker can only come from the app.
    ///
    /// `images` are the images you pasted with `Ctrl`+`V` whose placeholders are still in `text`;
    /// the core saves them under `attachments/message-<id>/` and adds their paths to the message.
    Send {
        to: AgentRef,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<PastedImage>,
    },
    /// `Esc` on a running turn: stop it and keep the session.
    Interrupt {
        agent: AgentRef,
    },
    /// Your answer on an approval or decision card.
    AnswerHeld {
        held: HeldActionId,
        answer: HeldAnswer,
    },
    /// Ask for a fresh [`Snapshot`].
    Refresh,
    /// You asked to quit; the core answers with [`Update::ConfirmQuit`] if agents are working, or
    /// [`Update::Closing`].
    RequestQuit,
    /// Quit now, confirmed.
    Quit,
    /// Your answer on the notice under one of your messages ("Ask again" or "Leave it"):
    /// `message` is your message, the line the card is drawn under.
    AnswerNotice {
        message: MessageId,
        answer: NoticeAnswer,
    },
    /// Run the setup check again: after an install or login in the setup screen's terminal pane
    /// finishes, and when you come back to the window.
    RecheckSetup,
    /// Your folder for a pending project proposal: the core checks it and updates the held action's
    /// stored payload, which is then what Approve performs.
    ChangeHeldLocation {
        held: HeldActionId,
        location: String,
    },
    /// Open the Diff screen for an approval card's held action, or close it with `None`: the core
    /// answers in the next snapshot's [`Snapshot::diff`].
    ShowDiff {
        #[serde(default)]
        held: Option<HeldActionId>,
    },
    // ---- Settings and connecting an account. ----
    /// Start the in-app login for a subscription (browser or device code); the core answers with
    /// [`SettingsView::login`]. `account` is set when you asked to reconnect that account (its
    /// login stopped working): the login then replaces its credential in place, keeping the
    /// account, unless it signs in as a different identity.
    StartLogin {
        provider: Provider,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<AccountId>,
    },
    /// Use the subscription detected from a CLI login on this Mac (Claude, through `claude`).
    UseDetectedLogin {
        provider: Provider,
    },
    /// Open the login's page in your browser.
    OpenLoginPage {
        url: String,
    },
    /// Stop waiting for the login in progress.
    CancelLogin,
    /// Dismiss a finished login.
    FinishLogin,
    /// Check an API key and store it in Quartermaster's credential store. For a custom
    /// OpenAI-compatible endpoint, `base_url` and `models` say where and what. The key never shows
    /// in `Debug` output ([`SecretText`]). `account` is set when you asked to reconnect that
    /// account: the key then replaces its credential in place.
    AddApiKey {
        provider: Provider,
        key: SecretText,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        models: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<AccountId>,
    },
    /// Your label for an account.
    SetAccountLabel {
        account: AccountId,
        label: String,
    },
    PauseAccount {
        account: AccountId,
    },
    ResumeAccount {
        account: AccountId,
    },
    /// Disconnect an account and forget its credentials (a Claude account is paused instead).
    DisconnectAccount {
        account: AccountId,
    },
    /// Mark one account preferred for crew work, or none.
    SetPreferredAccount {
        #[serde(default)]
        account: Option<AccountId>,
    },
    /// Put a tier entry (the model at `effort`; `None` is the tier default) in tier `to`: moved
    /// out of `from`, or added when `from` is `None`. One model may sit in a tier more than once
    /// at different efforts.
    PlaceModel {
        model: ModelId,
        #[serde(default)]
        from: Option<Tier>,
        #[serde(default)]
        effort: Option<Effort>,
        to: Tier,
    },
    /// Take one tier entry (the model at `effort` in `tier`) out.
    RemovePlacement {
        model: ModelId,
        tier: Tier,
        #[serde(default)]
        effort: Option<Effort>,
    },
    /// Take a model out of every tier.
    TakeOutOfUse {
        model: ModelId,
    },
    /// Change a tier entry's effort from `current` to `effort`; `None` is the tier default.
    SetPlacementEffort {
        model: ModelId,
        tier: Tier,
        #[serde(default)]
        current: Option<Effort>,
        #[serde(default)]
        effort: Option<Effort>,
    },
    /// Show a model found with find under NOT IN USE.
    ListModel {
        model: ModelId,
    },
    /// Turn the pin on or off, with its model and fallback.
    SetPin {
        on: bool,
        #[serde(default)]
        model: Option<ModelId>,
        #[serde(default)]
        fallback: Option<ModelId>,
    },
    /// What the Quartermaster runs on.
    SetQuartermasterModel {
        choice: QuartermasterChoice,
    },
    /// The Quartermaster's own effort level; `None` follows its model's tier. Saved to
    /// `config.toml` and applied at its next idle moment. Secondmates follow it too.
    SetQuartermasterEffort {
        #[serde(default)]
        effort: Option<Effort>,
    },
    /// Turn the bell and desktop notifications on or off; saved to `config.toml` and applied at
    /// once.
    SetNotifications {
        enabled: bool,
    },
    /// Set the autocompact window, in tokens, for every agent; the core clamps and rounds it to a
    /// step. Saved to `config.toml`; each agent applies it at its next idle moment.
    SetAutocompactWindow {
        tokens: u32,
    },
    /// Turn a registered project's autonomy on or off (from its menu in the Projects pane, or in
    /// Settings). Only you send this: no agent tool reaches it.
    SetProjectAutonomy {
        project: String,
        autonomy: Autonomy,
    },
    /// Point the personal workspace at a folder you chose (absolute, or from `~`). It's checked
    /// first, and refused while personal tasks are open. Saved to `config.toml`; no files move.
    SetWorkspaceFolder {
        folder: String,
    },
    // ---- Worker sessions. ----
    /// Open a worker's session (the core then fills [`Snapshot::session`]), or close it with
    /// `None` (back to the Bridge).
    ViewSession {
        #[serde(default)]
        agent: Option<AgentRef>,
    },
    // ---- The Quartermaster's slash commands. ----
    /// A slash command you ran in the Quartermaster's chat. It waits behind a running turn and
    /// your queued messages, in order; the chat says what it did.
    RunSlashCommand {
        slash: SlashCommand,
    },
    // ---- The app's own updates. ----
    /// Look for a newer release now (the core also looks at launch and every few hours).
    CheckForUpdate,
    /// Install the newer release the last check found ([`UpdateView::available`]). Ignored when
    /// there is none, or one is already installing.
    StartUpdate,
}

/// The Quartermaster's slash commands. The UI's command menu lists them (with their names and
/// descriptions, in `qm-tui`); the core runs them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SlashCommand {
    /// `/clear`: end the Quartermaster's session and start a new one with an empty conversation
    /// (no summary of earlier turns).
    Clear,
    /// `/compact`: compact the Quartermaster's conversation now.
    Compact,
}

/// Text that must never be shown or logged, such as an API key on its way to the credential
/// store: `Debug` prints `SecretText(<redacted>)`, there is no `Display`, and reading it takes
/// [`SecretText::expose`]. It serialises as a plain string, so the link (and a future socket)
/// can carry it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretText(String);

impl SecretText {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The text itself: for the credential store, never for a log line.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretText(<redacted>)")
    }
}

/// The two choices on a notice under your message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NoticeAnswer {
    /// Run your message again, as a new turn.
    AskAgain,
    /// Let it be.
    LeaveIt,
}

/// An answer on a card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum HeldAnswer {
    /// Approve an approval card: the app performs exactly the stored payload.
    Approve,
    /// Reject an approval card.
    Reject,
    /// Pick one of a decision card's choices, with your words where the choice takes them ("Retry
    /// with my guidance"). The core accepts only a choice the card stored (`held_actions.choices`)
    /// and refuses any other, so the stored answer says exactly what was chosen.
    Choose {
        choice: HeldChoice,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        words: Option<String>,
    },
}

/// One of a decision card's fixed choices, which the app writes and then carries out exactly. Shown
/// numbered from 1 in the order the card stores them. `#[non_exhaustive]`: a choice added later
/// never breaks another crate's `match`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "choice", rename_all = "snake_case")]
#[non_exhaustive]
pub enum HeldChoice {
    /// "Retry with my guidance": your words go to the crewmate as an unmarked message from you.
    RetryWithGuidance,
    /// "Try a premium model" (`premium`) or "Try a stronger model": a new crewmate at `tier`.
    StrongerModel { tier: Tier, premium: bool },
    /// "Discard the work", keeping a ref.
    Discard,
    /// The stuck notice's "Continue".
    Continue,
    /// The stuck notice's "Drop task".
    DropTask,
}

impl HeldChoice {
    /// The label on the card.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::RetryWithGuidance => "Retry with my guidance",
            Self::StrongerModel { premium: true, .. } => "Try a premium model",
            Self::StrongerModel { premium: false, .. } => "Try a stronger model",
            Self::Discard => "Discard the work",
            Self::Continue => "Continue",
            Self::DropTask => "Drop task",
        }
    }
}

/// From the core to the UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "update", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Update {
    /// Everything the Bridge shows, in full.
    Snapshot(Box<Snapshot>),
    /// Agents are working; ask before quitting ("4 crewmates are working; they'll pick up again
    /// next launch").
    ConfirmQuit { working: u32 },
    /// The core is shutting down; restore the terminal and exit.
    Closing,
}

/// What the Bridge shows, as display-ready rows. `Snapshot::default()` is the empty Bridge.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct Snapshot {
    /// This Mac's name, shown in the top bar.
    pub machine: String,
    /// One badge per account in the top bar ("Claude Max 7d 35%", "ChatGPT Pro ?").
    pub accounts: Vec<AccountBadge>,
    pub projects: Vec<ProjectRow>,
    /// The crew tree: the Quartermaster, its crewmates and secondmates, their crewmates.
    pub crew: Vec<CrewRow>,
    /// The Quartermaster's conversation, oldest first.
    pub conversation: Vec<ChatLine>,
    /// The status bar's counts.
    pub attention: Attention,
    /// What the setup check found missing; non-empty shows the red dot on the menu and in
    /// Settings.
    pub setup_issues: Vec<String>,
    /// Detail for each task the crew tree shows, for the task pane.
    pub tasks: Vec<TaskDetail>,
    /// The Quartermaster is in a turn, so `Esc` interrupts it.
    pub turn_running: bool,
    /// What the Settings screens show.
    pub settings: SettingsView,
    /// The setup check's screen, display-ready.
    pub setup: SetupView,
    /// The worker session you opened, while one is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionView>,
    /// The Diff screen's content, after [`Command::ShowDiff`]; `None` when it's closed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffView>,
    /// How many times the bell has rung since launch: when it rises, the UI writes BEL to the
    /// terminal. Only questions, decisions and approvals raise it, and only while notifications are
    /// on.
    #[serde(default)]
    pub bell: u64,
    /// The Quartermaster is in a turn and hasn't written anything in it yet: the conversation
    /// shows the busy indicator where its words will appear.
    #[serde(default)]
    pub thinking: bool,
    /// The app's own updates: the menu's update entry, its popup and the status bar's notice.
    #[serde(default)]
    pub update: UpdateView,
}

/// The app's own updates, display-ready. `UpdateView::default()` says nothing at all.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct UpdateView {
    /// This copy's version: "0.1.0".
    pub current: String,
    /// The newer release to install, installing or installed: "0.2.0". `None` while this copy is
    /// the latest, or before the first check has answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available: Option<String>,
    pub phase: UpdatePhase,
    /// The popup's sentence on where things stand ("Quartermaster 0.2.0 is available.").
    pub status: Styled,
    /// Lines under it: when the last check ran, why this copy can't update itself, the
    /// installer's last lines after a failure.
    pub detail: Vec<String>,
    /// The short notice beside the menu button ("update 0.2.0 available"). `None` when there's
    /// nothing to tell you, so nothing nags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<Styled>,
    /// The red dot on the menu's update entry (and so on the menu button): an update waits for
    /// you to install it, retry it or reopen the app.
    pub badge: bool,
    /// The popup's button; `None` while checking or installing, and after an install.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub button: Option<UpdateButton>,
    /// The release's page on GitHub, for its notes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_page: Option<String>,
}

/// Where the app's update stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum UpdatePhase {
    /// No check has answered yet (the first is running, or GitHub couldn't be reached).
    #[default]
    Unknown,
    Checking,
    /// This copy is the latest release.
    UpToDate,
    /// A newer release can be installed.
    Available,
    Installing,
    /// The newer release is installed; it runs from the next launch.
    Installed,
    /// The install didn't finish; this copy is unchanged.
    Failed,
}

/// The update popup's button.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UpdateButton {
    /// As shown: "Update to 0.2.0", "Try again", "Check now".
    pub label: String,
    /// The command it sends: [`Command::StartUpdate`] or [`Command::CheckForUpdate`].
    pub command: Command,
}

impl UpdateButton {
    pub fn new(label: impl Into<String>, command: Command) -> Self {
        Self {
            label: label.into(),
            command,
        }
    }
}

/// What the Diff screen shows for one held merge, push or in-place merge: the stat and the patch
/// from the merge base to the approved head, each line toned (`+` ok, `-` error, hunk and file
/// headers accent, the rest text), at most 20,000 lines.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DiffView {
    /// The held action whose diff this is; the card's keys still answer it from the Diff screen.
    pub held: HeldActionId,
    /// "T-0039 · feat(billing): proration preview".
    pub title: String,
    /// "+214 −37 · 12 files".
    #[serde(default)]
    pub stat: String,
    #[serde(default)]
    pub lines: Vec<Styled>,
    /// The patch was longer than the cap and is cut.
    #[serde(default)]
    pub truncated: bool,
}

impl DiffView {
    pub fn new(held: HeldActionId, title: impl Into<String>) -> Self {
        Self {
            held,
            title: title.into(),
            stat: String::new(),
            lines: Vec::new(),
            truncated: false,
        }
    }
}

/// What the setup check found on this Mac, as its screen shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct SetupView {
    /// The check has run at least once since the app started.
    pub checked: bool,
    /// One item per tool the app relies on: `git`, Claude Code, the GitHub CLI.
    pub items: Vec<SetupItem>,
    /// The status bar's left side: "Homebrew 4.6 found at /opt/homebrew"; `None` without
    /// Homebrew.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homebrew: Option<String>,
    /// How many times the check has run since the app started: the UI waits for a higher number
    /// after asking for a re-check.
    pub generation: u64,
}

/// Which tool a setup item is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SetupTool {
    Git,
    Claude,
    Gh,
}

/// A setup item's mark: `✓` (ok), `✗` (missing, error colour) or `!` (needs you, warning
/// colour).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SetupMark {
    #[default]
    Ok,
    Missing,
    Warn,
}

/// One tool on the setup check, display-ready.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SetupItem {
    pub tool: SetupTool,
    pub mark: SetupMark,
    /// "git 2.50.1", "GitHub CLI (gh)", "Claude Code"; bold unless the item is ok.
    pub name: String,
    /// The installed version, if installed: "2.92.0".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// After the name column: "Xcode Command Line Tools" (dim), "not installed" (error), …
    #[serde(default)]
    pub state: Vec<Styled>,
    /// The one-line form for the pane view: "GitHub CLI 2.92.0 · logged in".
    #[serde(default)]
    pub summary: String,
    /// Dim lines under the summary: "scopes: repo, workflow, read:org".
    #[serde(default)]
    pub details: Vec<String>,
    /// What the missing tool switches off, a dim paragraph under the item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    /// Numbered steps without Homebrew: a label and a command to copy.
    #[serde(default)]
    pub steps: Vec<SetupStep>,
    /// A dim line under the steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footnote: Option<String>,
    /// The buttons, left to right; the first `Run` is what `⏎` does.
    #[serde(default)]
    pub actions: Vec<SetupAction>,
    /// Dim words after the buttons: "runs claude auth login in a pane".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// How to fix it, one plain line for Settings ▸ System: "Install it with Homebrew: press
    /// Open, or run brew install gh."; `None` when the item is ok.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl SetupItem {
    pub fn new(tool: SetupTool, mark: SetupMark, name: impl Into<String>) -> Self {
        Self {
            tool,
            mark,
            name: name.into(),
            version: None,
            state: Vec::new(),
            summary: String::new(),
            details: Vec::new(),
            explanation: None,
            steps: Vec::new(),
            footnote: None,
            actions: Vec::new(),
            hint: None,
            fix: None,
        }
    }
}

/// A numbered step: "1. Install Homebrew", its command shown dim, and a Copy button.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SetupStep {
    pub label: String,
    pub command: String,
}

impl SetupStep {
    pub fn new(label: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            command: command.into(),
        }
    }
}

/// A button on a setup item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SetupAction {
    /// As shown: "Install with Homebrew".
    pub label: String,
    pub kind: SetupActionKind,
}

impl SetupAction {
    pub fn new(label: impl Into<String>, kind: SetupActionKind) -> Self {
        Self {
            label: label.into(),
            kind,
        }
    }
}

/// What a setup button does. Nothing runs without your click, and nothing uses `sudo`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SetupActionKind {
    /// Runs `argv` in the setup screen's terminal pane, titled `title` ("brew install gh"); the
    /// check runs again when it exits. `verb` names it in the key hint ("⏎ install").
    Run {
        title: String,
        argv: Vec<String>,
        verb: String,
    },
    /// Copies `text` to the clipboard.
    Copy { text: String },
    /// Not now: close the check; the banner stays while anything is missing.
    Later,
}

/// An account in the top bar.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AccountBadge {
    pub label: String,
    /// The usage window where known, e.g. "7d 35%"; `None` shows as unknown, never as zero.
    #[serde(default)]
    pub usage: Option<String>,
}

impl AccountBadge {
    /// A badge with no usage reading yet.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            usage: None,
        }
    }
}

/// A project in the left pane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProjectRow {
    pub name: String,
    pub delivery: DeliveryMode,
    pub autonomy: Autonomy,
    /// Whether any work is running in it (filled dot).
    #[serde(default)]
    pub active: bool,
}

impl ProjectRow {
    /// A project with no work running.
    pub fn new(name: impl Into<String>, delivery: DeliveryMode, autonomy: Autonomy) -> Self {
        Self {
            name: name.into(),
            delivery,
            autonomy,
            active: false,
        }
    }
}

/// A row of the crew tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CrewRow {
    pub agent: AgentRef,
    /// Indent: 0 for the Quartermaster, 1 for its reports, 2 for a secondmate's crewmates.
    pub depth: u8,
    /// e.g. "T-0042 fix", "SM-02 web-app".
    pub label: String,
    /// e.g. "12m", "idle", "approve", "decide".
    pub status: String,
    /// Waiting on you (shown as a flag).
    #[serde(default)]
    pub needs_you: bool,
    /// Which card the flag stands for: an approval (accent) or a decision (warning colour).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<Waiting>,
    /// The agent is in a turn right now: the row's glyph is the busy indicator.
    #[serde(default)]
    pub busy: bool,
}

impl CrewRow {
    /// A row that isn't waiting on you.
    pub fn new(
        agent: AgentRef,
        depth: u8,
        label: impl Into<String>,
        status: impl Into<String>,
    ) -> Self {
        Self {
            agent,
            depth,
            label: label.into(),
            status: status.into(),
            needs_you: false,
            waiting: None,
            busy: false,
        }
    }
}

/// One line of a conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChatLine {
    pub id: MessageId,
    pub source: MessageSource,
    pub kind: MessageKind,
    pub at: Timestamp,
    pub text: String,
    /// The acknowledgement shown beside your message ("Steered: T-0042"), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledgement: Option<String>,
    /// Dim words after the time in the header ("after T-0043 reported", "reply").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// A card drawn under the message: an approval, a decision or a notice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<Card>,
    /// Your message waits for the next turn: the header shows "queued".
    #[serde(default)]
    pub queued: bool,
    /// The text is still arriving; only complete lines are shown.
    #[serde(default)]
    pub streaming: bool,
}

impl ChatLine {
    /// A line with no acknowledgement beside it.
    pub fn new(
        id: MessageId,
        source: MessageSource,
        kind: MessageKind,
        at: Timestamp,
        text: impl Into<String>,
    ) -> Self {
        Self {
            id,
            source,
            kind,
            at,
            text: text.into(),
            acknowledgement: None,
            context: None,
            card: None,
            queued: false,
            streaming: false,
        }
    }
}

/// The status bar's counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct Attention {
    pub approvals: u32,
    pub decisions: u32,
    pub running: u32,
}

/// Which card a crew row's flag stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Waiting {
    Approval,
    Decision,
}

/// The colour role of a run of text; the UI maps it to the terminal's 16 colours.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Tone {
    #[default]
    Text,
    Dim,
    Strong,
    Accent,
    Ok,
    Warn,
    Error,
}

/// A run of text in one tone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Styled {
    pub text: String,
    #[serde(default)]
    pub tone: Tone,
}

impl Styled {
    pub fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }
}

/// What kind of card: its glyph, heading colour and border.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CardKind {
    /// "⚑ Approval needed": accent glyph, heading and border.
    Approval,
    /// "⚑ Decision": accent glyph, heading and border.
    Decision,
    /// "⚠ …": warning glyph, heading and border (the no-action notice, a stuck notice).
    Notice,
    /// "? …": a question from `ask_user` with one-click answers.
    Question,
}

/// What a card's button does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CardAction {
    /// Answer the card's held action.
    Answer { answer: HeldAnswer },
    /// Show the diff.
    ShowDiff,
    /// The no-action notice's "Ask again".
    AskAgain,
    /// The no-action notice's "Leave it".
    LeaveIt,
    /// A question card's one-click answer: sent to the Quartermaster as your message.
    Reply { text: String },
    /// A project proposal's "Change location": the input box collects the folder, prefilled with
    /// `location`, and sends `Command::ChangeHeldLocation`.
    ChangeLocation { location: String },
}

/// A button on a card and the key that presses it (`a`/`d`/`r` or `1`/`2`/`3`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CardButton {
    /// As shown: "Approve", "1 · Retry with my guidance".
    pub label: String,
    pub key: char,
    pub action: CardAction,
}

impl CardButton {
    pub fn new(label: impl Into<String>, key: char, action: CardAction) -> Self {
        Self {
            label: label.into(),
            key,
            action,
        }
    }
}

/// One body line of a card, word-wrapped to the card, every row indented by `indent` cells.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CardLine {
    #[serde(default)]
    pub indent: u16,
    #[serde(default)]
    pub spans: Vec<Styled>,
}

impl CardLine {
    pub fn new(indent: u16, spans: Vec<Styled>) -> Self {
        Self { indent, spans }
    }
}

/// A card under a message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Card {
    pub kind: CardKind,
    /// The held action the buttons answer; `None` for a notice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<HeldActionId>,
    /// After the glyph, in the card's colour: "Approval needed", "Decision".
    pub heading: String,
    /// After " · ", in the text colour: "T-0039".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// After one space, dim: "· api-server · high · opus-5-5".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default)]
    pub lines: Vec<CardLine>,
    #[serde(default)]
    pub buttons: Vec<CardButton>,
    /// Set once answered: the buttons give way to this outcome ("Approved").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered: Option<String>,
}

impl Card {
    pub fn new(kind: CardKind, heading: impl Into<String>) -> Self {
        Self {
            kind,
            held: None,
            heading: heading.into(),
            title: None,
            note: None,
            lines: Vec::new(),
            buttons: Vec::new(),
            answered: None,
        }
    }
}

/// A row of the task pane: a dim label padded to 7 cells, two spaces, then the value; with no
/// label the value is word-wrapped across the pane (the "why: “…”" row).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TaskRow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub value: Vec<Styled>,
}

impl TaskRow {
    pub fn new(label: Option<&str>, value: Vec<Styled>) -> Self {
        Self {
            label: label.map(str::to_owned),
            value,
        }
    }
}

/// A task as the task pane shows it, display-ready.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TaskDetail {
    pub task: TaskId,
    /// "fix(auth): token refresh race", bold and wrapped.
    pub title: String,
    #[serde(default)]
    pub rows: Vec<TaskRow>,
    /// Recent tool calls, oldest first, each shown as "▸ Bash cargo test auth".
    #[serde(default)]
    pub activity: Vec<String>,
    /// Still running, so the pane offers "Stop".
    #[serde(default)]
    pub running: bool,
}

impl TaskDetail {
    pub fn new(task: TaskId, title: impl Into<String>) -> Self {
        Self {
            task,
            title: title.into(),
            rows: Vec::new(),
            activity: Vec::new(),
            running: false,
        }
    }
}

/// A worker's session as its screen shows it: the header, the session's lines, the status line and
/// whether you can still write to it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionView {
    pub agent: AgentRef,
    /// "fix(auth): token refresh race".
    #[serde(default)]
    pub title: String,
    /// "api-server · sonnet-5-5 · supervisor: Quartermaster".
    #[serde(default)]
    pub note: String,
    /// Oldest first; the last may be the text block still arriving (`streaming`).
    #[serde(default)]
    pub lines: Vec<SessionLine>,
    /// "running 14m · job J3 idle".
    #[serde(default)]
    pub status: String,
    /// The worker is in a turn, so the screen offers Interrupt.
    #[serde(default)]
    pub turn_running: bool,
    /// You can still write to it: false once the task is closed or ending.
    #[serde(default)]
    pub open: bool,
    /// The empty input's text ("message T-0042 directly…"), or why it takes no text when not
    /// `open`.
    #[serde(default)]
    pub placeholder: String,
    /// A secondmate's conversation, oldest first, drawn as the Bridge draws the Quartermaster's.
    /// Empty for a crewmate, whose session is `lines`.
    #[serde(default)]
    pub conversation: Vec<ChatLine>,
    /// The dim line under the header in place of the crewmate banner: a secondmate's charter,
    /// first line. Empty for a crewmate.
    #[serde(default)]
    pub subtitle: String,
    /// A secondmate is in a turn and hasn't written anything in it yet: its conversation shows
    /// the busy indicator. A crewmate's session shows it from `turn_running` and `lines`.
    #[serde(default)]
    pub thinking: bool,
}

impl SessionView {
    /// An empty session view of `agent`; set the rest after.
    pub fn new(agent: AgentRef) -> Self {
        Self {
            agent,
            title: String::new(),
            note: String::new(),
            lines: Vec::new(),
            status: String::new(),
            turn_running: false,
            open: false,
            placeholder: String::new(),
            conversation: Vec::new(),
            subtitle: String::new(),
            thinking: false,
        }
    }
}

/// One line of a worker's session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionLine {
    /// The stored row's ID; the text block still arriving carries `0`.
    pub id: u64,
    pub kind: SessionLineKind,
    pub at: Timestamp,
    pub text: String,
    /// What Expand shows (the whole brief, a wake's full text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The text block still arriving: only its complete lines are shown.
    #[serde(default)]
    pub streaming: bool,
}

impl SessionLine {
    pub fn new(id: u64, kind: SessionLineKind, at: Timestamp, text: impl Into<String>) -> Self {
        Self {
            id,
            kind,
            at,
            text: text.into(),
            detail: None,
            streaming: false,
        }
    }
}

/// Who a session line is from (the user, the supervisor and the app, plus the worker and its
/// tools).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionLineKind {
    /// "[from supervisor]": the brief, the supervisor's messages.
    Supervisor,
    /// "[from app]": a wake from the app.
    App,
    /// "you (direct)": your message to the worker.
    You,
    /// The worker's own text.
    Agent,
    /// A tool it used ("▸ Bash cargo test auth").
    Tool,
}

/// What the Settings screens show: accounts, models and their placements on the tier board, the
/// pin, the Quartermaster's model, the subscriptions you can connect and a login in progress.
/// `SettingsView::default()` is empty.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct SettingsView {
    pub accounts: Vec<SettingsAccount>,
    /// Every model on your accounts, placed or not.
    pub models: Vec<SettingsModel>,
    pub pin: PinSetting,
    pub quartermaster: QuartermasterChoice,
    /// The Quartermaster's own effort level (`config.toml`'s `[quartermaster] effort`); `None`
    /// follows its model's tier. Its model's supported levels still cap it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quartermaster_effort: Option<Effort>,
    /// What the Quartermaster runs on now, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quartermaster_now: Option<QuartermasterNow>,
    /// The subscriptions you can connect, in the order the login screen lists them.
    pub providers: Vec<LoginProvider>,
    /// A login in progress or just finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login: Option<LoginView>,
    /// The bell and desktop notifications are on (Settings ▸ System).
    #[serde(default)]
    pub notifications: bool,
    /// The personal workspace's folder as Settings shows it (`~` for your home folder); empty
    /// until the core has one to show.
    #[serde(default)]
    pub workspace_folder: String,
    /// Why the Settings screens can't show or change anything this run, in one plain sentence
    /// ("auth.json can't be read: …; nothing was changed"), when the core couldn't start its
    /// settings part. The screens show it instead of an empty or loading tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    /// Each tier's default effort as routing uses it (`config.toml`'s `[tiers]`), so the Model
    /// levels screen shows a placement's "Tier default" as it runs. A model's supported efforts
    /// still cap it.
    #[serde(default)]
    pub tier_efforts: crate::config::TierEfforts,
    /// How many tokens of conversation every agent keeps before it compacts it (Settings ▸
    /// Routing ▸ Autocompact).
    #[serde(default)]
    pub autocompact_window: crate::config::AutocompactWindow,
}

/// Whether an account is in use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AccountStatus {
    #[default]
    Active,
    Paused,
}

/// One connected account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SettingsAccount {
    pub id: AccountId,
    /// Your label for it ("Claude Max").
    pub label: String,
    /// How it connects and which engine drives it ("your claude login · Claude engine").
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub status: AccountStatus,
    /// Preferred for crew work.
    #[serde(default)]
    pub preferred: bool,
    /// The usage line ("5-hour window 41% · 7-day 35%, resets Thu 09:00"); `None` is unknown,
    /// never zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<String>,
    /// Follows the `claude` login on this Mac: paused here, never disconnected.
    #[serde(default)]
    pub follows_cli: bool,
    /// Its provider, so the screen can start a login to reconnect it; `None` if unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    /// Connected with an API key (reconnecting asks for a key, not a login).
    #[serde(default)]
    pub api_key: bool,
    /// Why its login stopped working ("the refresh was refused; connect the account again"):
    /// routing skips it until you reconnect it. `None` is working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

impl SettingsAccount {
    /// An active account with no detail, usage or preference yet.
    pub fn new(id: AccountId, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            detail: String::new(),
            status: AccountStatus::Active,
            preferred: false,
            usage: None,
            follows_cli: false,
            provider: None,
            api_key: false,
            problem: None,
        }
    }
}

/// One tier entry: the model in a tier at an effort. A model may have several entries in one tier,
/// each at a different effort.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelPlacement {
    pub tier: Tier,
    /// The entry's effort; `None` follows the tier default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}

impl ModelPlacement {
    pub fn new(tier: Tier, effort: Option<Effort>) -> Self {
        Self { tier, effort }
    }
}

/// One model on your accounts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SettingsModel {
    pub id: ModelId,
    /// The display name ("Opus 5.5").
    pub name: String,
    /// The account it runs on, by label ("Claude Max").
    #[serde(default)]
    pub account: String,
    /// Where it sits on the board; empty is not in use.
    #[serde(default)]
    pub placements: Vec<ModelPlacement>,
    /// The effort levels this model supports, lowest first (a set).
    #[serde(default)]
    pub supported: Vec<Effort>,
    /// Newly found on an account and not yet placed.
    #[serde(default)]
    pub new: bool,
    /// Listed under NOT IN USE; the rest are only reachable through find.
    #[serde(default)]
    pub listed: bool,
}

impl SettingsModel {
    /// A model in no tier, with no supported levels yet.
    pub fn new(id: ModelId, name: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            account: account.into(),
            placements: Vec::new(),
            supported: Vec::new(),
            new: false,
            listed: false,
        }
    }

    /// Whether the model sits in no tier.
    pub fn unused(&self) -> bool {
        self.placements.is_empty()
    }

    /// Whether the model has an entry in `tier`, at any effort.
    pub fn in_tier(&self, tier: Tier) -> bool {
        self.placements.iter().any(|p| p.tier == tier)
    }

    /// Its entries in `tier`, in the order they were made.
    pub fn placements_in(&self, tier: Tier) -> impl Iterator<Item = ModelPlacement> + '_ {
        self.placements
            .iter()
            .copied()
            .filter(move |p| p.tier == tier)
    }

    /// Its entry in `tier` at `effort` (`None` is the tier default), if any.
    pub fn placement(&self, tier: Tier, effort: Option<Effort>) -> Option<ModelPlacement> {
        self.placements_in(tier).find(|p| p.effort == effort)
    }
}

/// The pin: always use one model for all crew work, ignoring tiers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct PinSetting {
    pub on: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Where crew work goes when the pinned model can't take it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<ModelId>,
}

impl PinSetting {
    pub fn new(on: bool, model: Option<ModelId>, fallback: Option<ModelId>) -> Self {
        Self {
            on,
            model,
            fallback,
        }
    }
}

/// What the Quartermaster runs on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum QuartermasterChoice {
    /// The code chooses: high, then medium, premium, low.
    #[default]
    Auto,
    /// One model.
    Model(ModelId),
}

/// The model the Quartermaster runs on now, and its effort.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QuartermasterNow {
    pub model: ModelId,
    pub effort: Effort,
}

impl QuartermasterNow {
    pub fn new(model: ModelId, effort: Effort) -> Self {
        Self { model, effort }
    }
}

/// How a provider logs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LoginMethod {
    /// Opens your browser and waits for the callback.
    Browser,
    /// Shows a code to enter on the provider's page.
    DeviceCode,
    /// Detected from a CLI login on this Mac (Claude).
    Detected,
}

/// A subscription you can connect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LoginProvider {
    pub id: Provider,
    pub name: String,
    pub method: LoginMethod,
    /// Already connected or detected, with what ("logged in · Max").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connected: Option<String>,
}

impl LoginProvider {
    pub fn new(id: Provider, name: impl Into<String>, method: LoginMethod) -> Self {
        Self {
            id,
            name: name.into(),
            method,
            connected: None,
        }
    }
}

/// What a login asks you to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LoginStep {
    /// Asked the core; nothing to show yet.
    Starting,
    /// Your browser opened `url`; the core waits for the callback.
    Browser { url: String },
    /// Enter `code` at `url`.
    DeviceCode { url: String, code: String },
}

/// Where a login stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LoginStatus {
    Waiting,
    /// Connected: `models` found on the account, and whether the starter tiers were filled.
    Connected {
        models: u32,
        starter_tiers: bool,
    },
    /// Failed, in a plain sentence that never carries a secret.
    Failed {
        reason: String,
    },
}

/// A login in progress or just finished.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LoginView {
    pub provider: Provider,
    /// The provider's name ("GitHub Copilot").
    pub name: String,
    /// Who it waits for ("GitHub").
    pub waiting_for: String,
    pub step: LoginStep,
    pub status: LoginStatus,
    /// Seconds the code or link was good for when this snapshot was made; the UI counts down
    /// from when it received it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
    /// The account this login reconnects, when you started it from that account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountId>,
}

impl LoginView {
    /// A login just asked for: starting, waiting.
    pub fn new(
        provider: Provider,
        name: impl Into<String>,
        waiting_for: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            name: name.into(),
            waiting_for: waiting_for.into(),
            step: LoginStep::Starting,
            status: LoginStatus::Waiting,
            expires_in: None,
            account: None,
        }
    }
}

/// The link's other end is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the link between the core and the UI is closed")]
pub struct LinkClosed;

/// Connects a UI to a core in-process.
pub fn link() -> (CoreClient, CoreServer) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (upd_tx, upd_rx) = mpsc::unbounded_channel();
    (
        CoreClient {
            commands: cmd_tx,
            updates: upd_rx,
        },
        CoreServer {
            commands: cmd_rx,
            updates: upd_tx,
        },
    )
}

/// The UI's end of the link.
#[derive(Debug)]
pub struct CoreClient {
    commands: mpsc::UnboundedSender<Command>,
    updates: mpsc::UnboundedReceiver<Update>,
}

impl CoreClient {
    /// Sends a command; fails only if the core is gone.
    pub fn send(&self, command: Command) -> Result<(), LinkClosed> {
        self.commands.send(command).map_err(|_| LinkClosed)
    }

    /// The next update, waiting for it; `None` once the core is gone and every update is read.
    pub async fn recv(&mut self) -> Option<Update> {
        self.updates.recv().await
    }

    /// The next update if one is ready, without waiting.
    pub fn try_recv(&mut self) -> Result<Option<Update>, LinkClosed> {
        match self.updates.try_recv() {
            Ok(u) => Ok(Some(u)),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => Err(LinkClosed),
        }
    }
}

/// The core's end of the link.
#[derive(Debug)]
pub struct CoreServer {
    commands: mpsc::UnboundedReceiver<Command>,
    updates: mpsc::UnboundedSender<Update>,
}

impl CoreServer {
    /// The next command, waiting for it; `None` once the UI is gone.
    pub async fn recv(&mut self) -> Option<Command> {
        self.commands.recv().await
    }

    /// Sends an update; fails only if the UI is gone.
    pub fn send(&self, update: Update) -> Result<(), LinkClosed> {
        self.updates.send(update).map_err(|_| LinkClosed)
    }
}
