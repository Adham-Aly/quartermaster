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

use crate::domain::{
    Autonomy, DeliveryMode, Effort, KindRow, MessageKind, MessageSource, Provider, Tier,
};
use crate::ids::{AccountId, HeldActionId, MessageId, ModelId, SecondmateId, TaskId, TaskLabelId};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio::sync::mpsc;

mod actions;
mod context;
mod diff;
mod draft;
mod image;
pub use actions::{ActionItem, ActionKey, ActionKind, ActionOpen};
pub use context::{
    AgentContextView, CompactingView, CompactionNote, ContextBasisView, ContextOccupancy,
    NoteOutcome, PointView, TurnTokens, tokens_short,
};
pub use diff::{DiffFile, DiffFileStatus, DiffHunk, DiffLine, DiffLineKind};
pub use draft::{DraftImage, DraftTarget, SavedDraft};
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
    /// Open a plan task's page (`plan.html`) in your browser, on the Mac the app runs on.
    OpenPlan {
        task: TaskId,
    },
    /// Open only this task's completed static report in the browser on this Mac. Once it opens,
    /// the report's card leaves Actions.
    OpenReport {
        task: TaskId,
    },
    /// Take a finished report's card out of Actions without opening it. The task pane still
    /// opens the report.
    DismissReport {
        task: TaskId,
    },
    /// Your Approve on a plan's card: the plan is final and a build may start on it.
    ApprovePlan {
        task: TaskId,
    },
    /// Your Go ahead on a review's card (normal autonomy): its supervisor may now act on the
    /// findings as you asked.
    ApproveReview {
        task: TaskId,
    },
    /// Open the Diff screen for an approval card's held action, or close it with `None`: the core
    /// answers in the next snapshot's [`Snapshot::diff`].
    ShowDiff {
        #[serde(default)]
        held: Option<HeldActionId>,
    },
    // ---- Settings and connecting an account. ----
    /// Refresh provider-reported quotas for connected accounts; results arrive in settings snapshots.
    RefreshQuotas,
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
    /// An account's soft limit (shown as "Soft limit"; stored as `reserve_at`): the percent of
    /// its largest quota pool USED (1 to 99) past which routing serves its tier from accounts
    /// under theirs first; `None` clears it.
    SetReserveMark {
        account: AccountId,
        #[serde(default)]
        percent: Option<u8>,
    },
    /// Mark one account preferred for crew work, or none.
    SetPreferredAccount {
        #[serde(default)]
        account: Option<AccountId>,
    },
    /// Put a model + effort combination in tier `to`: moved out of `from`, or added when `from`
    /// is `None`. An effort of `None` is for models without effort controls or unresolved legacy
    /// tier-default entries. One model may sit in a tier at different efforts. A stale source or
    /// duplicate destination is refused without removing the source, not merged; moving an
    /// existing entry to its current tier is a no-op.
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
    /// Legacy API to change a tier entry's effort from `current` to `effort`; `None` retains
    /// tier-default semantics for effort-capable models. The current board adds and moves fixed
    /// combinations instead of offering a separate placement-effort picker.
    SetPlacementEffort {
        model: ModelId,
        tier: Tier,
        #[serde(default)]
        current: Option<Effort>,
        #[serde(default)]
        effort: Option<Effort>,
    },
    /// Legacy API to persist listing metadata for the former NOT IN USE list. Retained for
    /// compatibility; the current fixed catalogue includes every model regardless of this flag.
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
    /// The Quartermaster's own effort override; `None` uses the selected combination's effort
    /// with Auto model, or a named model's tier default. Explicit overrides are capped to the
    /// model's supported levels. Saved to `config.toml` and applied at its next idle moment;
    /// secondmates follow it too.
    SetQuartermasterEffort {
        #[serde(default)]
        effort: Option<Effort>,
    },
    /// Set the autocompact window, in tokens, for every agent; the core clamps and rounds it to a
    /// step. Saved to `config.toml`; each agent applies it at its next idle moment.
    SetAutocompactWindow {
        tokens: u32,
    },
    /// Route a Task kinds row to `tier`; `None` puts it back to the app's default. Saved to
    /// `config.toml` (every row, once you change any) and used by the next task routed.
    SetKindTier {
        row: KindRow,
        #[serde(default)]
        tier: Option<Tier>,
    },
    /// Put every Task kinds row back to the app's default: `[routing.kinds]` leaves
    /// `config.toml`. The UI asks you first.
    ResetKindTiers,
    /// Set a registered project's autonomy to normal or auto (from its menu in the Projects pane, or in
    /// Settings). Only you send this: no agent tool reaches it.
    SetProjectAutonomy {
        project: String,
        autonomy: Autonomy,
    },
    /// Retired with the personal workspace: the core answers with a note that work runs in the
    /// folder the user names.
    SetWorkspaceFolder {
        folder: String,
    },
    // ---- Drafts. ----
    /// Your unsent draft for one prompt, as it now stands. An empty one forgets the draft. The core
    /// keeps it for the server's run and gives it back in the snapshot; it is never written to disk.
    SaveDraft {
        draft: SavedDraft,
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
    /// Setup instruction while no model level is configured; sending to the seat is blocked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_setup_notice: Option<String>,
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
    /// The Quartermaster is in a turn and hasn't written anything in it yet: the conversation
    /// shows the busy indicator where its words will appear.
    #[serde(default)]
    pub thinking: bool,
    /// The app's own updates: the menu's update entry, its popup and the status bar's notice.
    #[serde(default)]
    pub update: UpdateView,
    /// Your unsent drafts, held by the core. The UI restores them from its first snapshot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drafts: Vec<SavedDraft>,
    /// The Actions inbox: every request waiting on you, configuration first, then newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActionItem>,
    /// Each agent's context occupancy, turn usage and compactions, from passive engine readings
    /// held in memory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<AgentContextView>,
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

/// What the Diff screen shows for one held merge or push, from the merge base to the approved
/// head: summary statistics, structured changed files and hunks, and the capped original patch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DiffView {
    /// The held action whose diff this is; the card's keys still answer it from the Diff screen.
    pub held: HeldActionId,
    /// "C-0039 · feat(billing): proration preview".
    pub title: String,
    /// "+214 −37 · 12 files".
    #[serde(default)]
    pub stat: String,
    /// The original capped raw patch (or a read error), retained for older clients and fallback.
    #[serde(default)]
    pub lines: Vec<Styled>,
    /// Structured files from the same patch, in git's order. Empty for old snapshots or read errors.
    #[serde(default)]
    pub files: Vec<DiffFile>,
    /// The patch was cut at the cap, or a text hunk is incomplete.
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
            files: Vec::new(),
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
    /// Its open tasks, drawn beneath it in the pane's tree, oldest first. A task with crewmates in
    /// several projects is listed under each of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<ProjectTask>,
}

impl ProjectRow {
    /// A project with no work running and no open tasks.
    pub fn new(name: impl Into<String>, delivery: DeliveryMode, autonomy: Autonomy) -> Self {
        Self {
            name: name.into(),
            delivery,
            autonomy,
            active: false,
            tasks: Vec::new(),
        }
    }
}

/// An open task beneath its project in the projects pane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProjectTask {
    pub id: TaskLabelId,
    /// What it works on: its oldest open crewmate's title in this project.
    pub title: String,
}

impl ProjectTask {
    pub fn new(id: TaskLabelId, title: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
        }
    }
}

/// A row of the crew tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CrewRow {
    pub agent: AgentRef,
    /// The agent's level under the Quartermaster: 0 for the Quartermaster, 1 for its crewmates and
    /// secondmates, 2 for a secondmate's crewmates. A crewmate with a `task` is drawn one level
    /// deeper, under its task's heading.
    pub depth: u8,
    /// e.g. "C-0042 fix", "S-02 web-app".
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
    /// A crewmate's task: the tree draws a heading for it above the first of its crewmates among
    /// the rows under one supervisor, which the core keeps together. `None` draws the row as is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<CrewTask>,
}

/// The task heading a crewmate's row sits under in the crew tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CrewTask {
    pub id: TaskLabelId,
    /// The heading's words: "T-0042 api-server".
    pub label: String,
}

impl CrewTask {
    pub fn new(id: TaskLabelId, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
        }
    }
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
            task: None,
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
    /// The acknowledgement shown beside your message ("Steered: C-0042"), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledgement: Option<String>,
    /// Dim words after the time in the header ("after C-0043 reported", "reply").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Never set: cards are only in [`Snapshot::actions`], and the chat draws none. Kept so an
    /// older window still reads a newer server's snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<Card>,
    /// Your message waits for the next turn: the header shows "queued".
    #[serde(default)]
    pub queued: bool,
    /// The text is still arriving; only complete lines are shown.
    #[serde(default)]
    pub streaming: bool,
    /// A short note from the app drawn as one dim line, with no header: a plan reply sent from
    /// its page.
    #[serde(default)]
    pub one_line: bool,
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
            one_line: false,
        }
    }

    /// The line drawn as one dim row, with no header.
    #[must_use]
    pub fn with_one_line(mut self) -> Self {
        self.one_line = true;
        self
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
    /// "☰ Plan …": a plan task's page, with its round and what it waits on.
    Plan,
    /// "✎ Review ready": a review's static report, waiting on your go-ahead (normal autonomy).
    Review,
    /// "✓ Report ready": a finished answer task's static report you haven't opened yet.
    Report,
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
    /// A plan card's "Open": sends `Command::OpenPlan`.
    OpenPlan { task: TaskId },
    /// A plan card's "Copy path": copies `path` (the page's file) to your clipboard.
    CopyPlanPath { path: String },
    /// A final plan card's "Approve": sends `Command::ApprovePlan`.
    ApprovePlan { task: TaskId },
    /// A review card's "Open": sends `Command::OpenReport`.
    OpenReport { task: TaskId },
    /// A review card's "Go ahead": sends `Command::ApproveReview`.
    ApproveReview { task: TaskId },
    /// A report card's "Open": sends `Command::OpenReport`, and the card goes once it opens.
    ReadReport { task: TaskId },
    /// A report card's "Dismiss": sends `Command::DismissReport`.
    DismissReport { task: TaskId },
    /// A configuration item's "Open Settings".
    OpenSettings,
    /// A setup item's "Open Setup".
    OpenSetup,
    /// An option-less question's "Reply…": opens the asking agent's chat with the input focused.
    OpenChat { to: AgentRef },
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

/// A request's card, shown in the Actions overlay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Card {
    pub kind: CardKind,
    /// The held action the buttons answer; `None` for a notice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<HeldActionId>,
    /// After the glyph, in the card's colour: "Approval needed", "Decision".
    pub heading: String,
    /// After " · ", in the text colour: "C-0039".
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
    /// The task's kind ("code", "plan", "research"), shown as a chip; empty when unknown.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub rows: Vec<TaskRow>,
    /// Recent tool calls, oldest first, each shown as "▸ Bash cargo test auth".
    #[serde(default)]
    pub activity: Vec<String>,
    /// Still running, so the pane offers "Stop".
    #[serde(default)]
    pub running: bool,
    /// A completed static report is available; offer Open report in the task pane.
    #[serde(default)]
    pub report_available: bool,
}

impl TaskDetail {
    pub fn new(task: TaskId, title: impl Into<String>) -> Self {
        Self {
            task,
            title: title.into(),
            kind: String::new(),
            rows: Vec::new(),
            activity: Vec::new(),
            running: false,
            report_available: false,
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
    /// The empty input's text ("message C-0042 directly…"), or why it takes no text when not
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
    /// The Quartermaster's own effort override (`config.toml`'s `[quartermaster] effort`);
    /// `None` uses Auto's selected combination or a named model's tier default. Explicit
    /// overrides are capped to the model's supported levels.
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
    /// Retired with the personal workspace; always empty now.
    #[serde(default)]
    pub workspace_folder: String,
    /// Why the Settings screens can't show or change anything this run, in one plain sentence
    /// ("auth.json can't be read: …; nothing was changed"), when the core couldn't start its
    /// settings part. The screens show it instead of an empty or loading tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    /// Configured tier defaults (`config.toml`'s `[tiers]`), retained for model-only choices and
    /// unresolved legacy entries. Normal catalogue placements carry their own effort; the board
    /// does not offer a separate "Tier default" control.
    #[serde(default)]
    pub tier_efforts: crate::config::TierEfforts,
    /// How many tokens of conversation every agent keeps before it compacts it (Settings ▸
    /// Routing ▸ Autocompact).
    #[serde(default)]
    pub autocompact_window: crate::config::AutocompactWindow,
    /// Settings ▸ Routing ▸ Task kinds: every row, in `KindRow::ALL` order.
    #[serde(default)]
    pub kind_tiers: Vec<KindTierRow>,
}

/// One Task kinds row: the tier it routes to and the app's default for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct KindTierRow {
    pub row: KindRow,
    pub tier: Tier,
    pub default: Tier,
    /// The tier has no model in it: its work runs on this tier instead (routing's fallback
    /// order), or waits when `None`.
    #[serde(default)]
    pub empty: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runs_on: Option<Tier>,
}

impl KindTierRow {
    pub fn new(row: KindRow, tier: Tier, default: Tier) -> Self {
        Self {
            row,
            tier,
            default,
            empty: false,
            runs_on: None,
        }
    }

    /// The row routes somewhere other than the app's default.
    pub fn changed(&self) -> bool {
        self.tier != self.default
    }
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
// Independent display flags in an additive wire contract, not mutually exclusive states.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent account display flags"
)]
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
    /// Display-ready provider quota windows, balances or allowances; separate from session usage.
    #[serde(default)]
    pub quota_lines: Vec<String>,
    /// Why the latest quota check failed; does not imply that the login is broken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_problem: Option<String>,
    /// A provider quota check is in progress; previous results may still be shown.
    #[serde(default)]
    pub quota_refreshing: bool,
    /// Display-ready time of the last successful quota check; `None` means no successful reading yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_checked_at: Option<String>,
    /// Why this account has no provider quota check ("not offered by this provider"); shown as
    /// a neutral note, never as a problem. `None` when a check exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_note: Option<String>,
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
    /// Your soft limit, in percent USED of its largest quota pool (1 to 99): past it, routing
    /// serves its tier from your other accounts first. `None`: no soft limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_at: Option<u8>,
    /// What routing reads for it ("7-day window 60% used · past its soft limit: others serve
    /// first"); `None` before the core has filled it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<String>,
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
            quota_lines: Vec::new(),
            quota_problem: None,
            quota_refreshing: false,
            quota_checked_at: None,
            quota_note: None,
            follows_cli: false,
            provider: None,
            api_key: false,
            problem: None,
            reserve_at: None,
            routing: None,
        }
    }
}

/// One tier entry: the model in a tier at an effort. A model may have several entries in one tier,
/// each at a different effort.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelPlacement {
    pub tier: Tier,
    /// The combination's effort; `None` means no effort for models without effort controls,
    /// or a tier default for unresolved legacy entries.
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
    /// Where its combinations sit on the board; empty means no tier placements.
    #[serde(default)]
    pub placements: Vec<ModelPlacement>,
    /// The effort levels this model supports, lowest first (a set).
    #[serde(default)]
    pub supported: Vec<Effort>,
    /// Newly found on an account and not yet placed.
    #[serde(default)]
    pub new: bool,
    /// Legacy listing metadata for the former NOT IN USE list, retained for compatibility.
    /// Does not control visibility: the current catalogue includes all models and supported efforts.
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

#[cfg(test)]
mod quota_tests {
    use super::*;

    #[test]
    fn legacy_accounts_default_quota_fields_and_new_fields_round_trip() {
        let account = SettingsAccount::new(AccountId::new(1), "Work");
        let mut legacy = serde_json::to_value(&account).unwrap();
        for field in [
            "quota_lines",
            "quota_problem",
            "quota_refreshing",
            "quota_checked_at",
            "quota_note",
        ] {
            legacy.as_object_mut().unwrap().remove(field);
        }
        assert_eq!(
            serde_json::from_value::<SettingsAccount>(legacy).unwrap(),
            account
        );
        let mut populated = account;
        populated.quota_lines = vec!["5-hour window: 41% used".into()];
        populated.quota_problem = Some("temporarily unavailable".into());
        populated.quota_refreshing = true;
        populated.quota_checked_at = Some("today 12:04".into());
        populated.quota_note = Some("not offered by this provider".into());
        let encoded = serde_json::to_value(&populated).unwrap();
        assert_eq!(
            serde_json::from_value::<SettingsAccount>(encoded).unwrap(),
            populated
        );
        let encoded = serde_json::to_value(Command::RefreshQuotas).unwrap();
        assert_eq!(
            serde_json::from_value::<Command>(encoded).unwrap(),
            Command::RefreshQuotas
        );
    }
}
