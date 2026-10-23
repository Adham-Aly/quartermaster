//! The app's closed sets of values, and who started a turn.
//!
//! Every enum here is a plain set of variants with one **wire name** each (`snake_case`). The wire
//! name is used everywhere the value leaves Rust: JSON (tool arguments, events, the core API), the
//! database (a `TEXT` column with a `CHECK` listing the same names) and `config.toml`. Each enum
//! has `ALL` (declaration order, which is also its `Ord` order where order means something),
//! `as_str()`, `Display` and `FromStr` over the wire name, and a JSON Schema that is a bare
//! `{"type":"string","enum":[…]}`, with no per-variant descriptions.
//!
//! Adding a variant changes a contract and needs the matching `CHECK` in a new migration;
//! `WakeKind` is the exception: it grows as wakes are added.

use crate::error::UnknownVariant;
use crate::ids::MessageId;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident ($kind:literal) {
            $( $(#[$vmeta:meta])* $variant:ident = $wire:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name {
            $( $(#[$vmeta])* $variant ),+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The wire name.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = UnknownVariant;

            fn from_str(s: &str) -> Result<Self, UnknownVariant> {
                match s {
                    $($wire => Ok(Self::$variant),)+
                    _ => Err(UnknownVariant {
                        kind: $kind,
                        value: s.to_owned(),
                        expected: [$($wire),+].join(", "),
                    }),
                }
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = Cow::<'de, str>::deserialize(d)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed(stringify!($name))
            }

            fn inline_schema() -> bool {
                true
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({ "type": "string", "enum": [$($wire),+] })
            }
        }

        #[cfg(feature = "rusqlite")]
        impl rusqlite::types::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(rusqlite::types::ToSqlOutput::from(self.as_str()))
            }
        }

        #[cfg(feature = "rusqlite")]
        impl rusqlite::types::FromSql for $name {
            fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
                v.as_str()?
                    .parse()
                    .map_err(|e| rusqlite::types::FromSqlError::Other(Box::new(e)))
            }
        }
    };
}

string_enum! {
    /// Who an agent is.
    pub enum Role ("role") {
        Quartermaster = "quartermaster",
        Secondmate = "secondmate",
        Crewmate = "crewmate",
    }
}

string_enum! {
    /// Which engine runs an agent. It follows the account: a Claude subscription detected through
    /// `claude` runs on the Claude engine, every other account on the provider engine. `Scripted`
    /// plays back scripted responses for the developer seat.
    pub enum EngineKind ("engine") {
        Claude = "claude",
        Provider = "provider",
        Scripted = "scripted",
    }
}

string_enum! {
    /// Who serves an account. `Claude` is a subscription through your `claude`; `Anthropic` is an
    /// Anthropic API key on the provider engine.
    pub enum Provider ("provider") {
        Claude = "claude",
        ChatGpt = "chatgpt",
        Copilot = "copilot",
        Xai = "xai",
        Kimi = "kimi",
        OpenRouter = "openrouter",
        Gemini = "gemini",
        Anthropic = "anthropic",
        OpenAi = "openai",
        Custom = "custom",
        Scripted = "scripted",
    }
}

impl Provider {
    /// The engine this provider's accounts run on.
    pub const fn engine(self) -> EngineKind {
        match self {
            Self::Claude => EngineKind::Claude,
            Self::Scripted => EngineKind::Scripted,
            _ => EngineKind::Provider,
        }
    }
}

string_enum! {
    /// Subscriptions come before pay-per-token keys when ranking.
    pub enum AccountKind ("account kind") {
        Subscription = "subscription",
        ApiKey = "api_key",
    }
}

string_enum! {
    /// The kind of a task.
    pub enum TaskKind ("task kind") {
        Build = "build",
        Fix = "fix",
        Plan = "plan",
        Research = "research",
        Lookup = "lookup",
        Teach = "teach",
        Review = "review",
        Unclear = "unclear",
        Reproduce = "reproduce",
    }
}

impl TaskKind {
    /// Whether the kind's work is usually code: build, fix and unclear. Guidance only (the brief's
    /// wording, the readiness probe, one writer per folder): what a task delivered is decided by
    /// what its crewmate did (`qm_policy::delivery::deliverable`), never by its kind.
    pub const fn can_change_code(self) -> bool {
        matches!(self, Self::Build | Self::Fix | Self::Unclear)
    }

    /// The kinds whose work is usually an answer. Guidance only, like [`Self::can_change_code`].
    pub const fn usually_answers(self) -> bool {
        !self.can_change_code()
    }
}

string_enum! {
    /// Model tiers, weakest first. Premium is never picked automatically.
    pub enum Tier ("tier") {
        Low = "low",
        Medium = "medium",
        High = "high",
        Premium = "premium",
    }
}

string_enum! {
    /// The one effort ladder, lowest first. On Claude the wire names are passed as
    /// `--effort=<name>`; the provider engine maps them through the catalog.
    pub enum Effort ("effort") {
        Low = "low",
        Medium = "medium",
        High = "high",
        ExtraHigh = "xhigh",
        Max = "max",
    }
}

impl Effort {
    /// One step lower on the ladder, saturating at `Low` (quick verification).
    #[must_use]
    pub const fn step_down(self) -> Self {
        match self {
            Self::Low | Self::Medium => Self::Low,
            Self::High => Self::Medium,
            Self::ExtraHigh => Self::High,
            Self::Max => Self::ExtraHigh,
        }
    }

    /// The label the TUI shows.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::ExtraHigh => "Extra high",
            Self::Max => "Max",
        }
    }
}

string_enum! {
    /// Verification levels are tiers of process, not of test depth.
    pub enum VerificationLevel ("verification level") {
        Quick = "quick",
        Standard = "standard",
        Reviewed = "reviewed",
    }
}

impl VerificationLevel {
    /// The label the TUI shows: Quick, Default, Careful.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Quick => "Quick",
            Self::Standard => "Default",
            Self::Reviewed => "Careful",
        }
    }
}

string_enum! {
    /// Where a task's verification level came from: your words, the project's setting, or the
    /// `standard` default.
    pub enum VerificationSource ("verification source") {
        User = "user",
        Project = "project",
        Default = "default",
    }
}

string_enum! {
    /// How a project's finished work is delivered. `Folder` is the personal workspace's plain
    /// folder: crewmates work in it directly, with no git, so their edits are final.
    pub enum DeliveryMode ("delivery mode") {
        DirectPr = "direct_pr",
        BranchOnly = "branch_only",
        BuildInPlace = "build_in_place",
        Folder = "folder",
    }
}

string_enum! {
    /// A project's autonomy, `normal` by default; only you change it. Normal: you approve plans
    /// in their page, and pull requests, pushes and merges on cards. Auto: the crew decides and
    /// goes ahead, and asks you only when it can't go on.
    pub enum Autonomy ("autonomy") {
        Normal = "normal",
        Auto = "auto",
    }
}

string_enum! {
    /// How a plan task plans, chosen by the app from the project's autonomy when the planner's
    /// session starts; never by the planner. Interactive: the planner questions you through the
    /// plan's page. Internal: the plan is for the crew, and the planner decides open points itself.
    pub enum PlanMode ("plan mode") {
        Interactive = "interactive",
        Internal = "internal",
    }
}

impl PlanMode {
    /// The plan mode a project's autonomy gives.
    #[must_use]
    pub const fn for_autonomy(autonomy: Autonomy) -> Self {
        match autonomy {
            Autonomy::Normal => Self::Interactive,
            Autonomy::Auto => Self::Internal,
        }
    }
}

string_enum! {
    /// A project's merge method for pull requests.
    pub enum MergeMethod ("merge method") {
        Merge = "merge",
        Squash = "squash",
        Rebase = "rebase",
    }
}

string_enum! {
    /// How a project joined the list. `Workspace` is the built-in personal workspace.
    pub enum ProjectOrigin ("project origin") {
        Linked = "linked",
        Cloned = "cloned",
        Created = "created",
        Workspace = "workspace",
    }
}

string_enum! {
    /// What `propose_project` proposes.
    pub enum ProjectAction ("project action") {
        Link = "link",
        Clone = "clone",
        Create = "create",
    }
}

string_enum! {
    /// A task's place in its life. The side exits `failed`, `stopped` and `discarded` still pass
    /// through `closed`, so the worktree is always retired the same way.
    pub enum TaskStatus ("task status") {
        Queued = "queued",
        Preparing = "preparing",
        Running = "running",
        Waiting = "waiting",
        Held = "held",
        Stuck = "stuck",
        Reported = "reported",
        InReview = "in_review",
        Settling = "settling",
        Landed = "landed",
        Closed = "closed",
        Failed = "failed",
        Stopped = "stopped",
        Discarded = "discarded",
    }
}

string_enum! {
    /// How a closed task ended. Answer-only tasks end `answered`.
    pub enum TaskOutcome ("task outcome") {
        Landed = "landed",
        Answered = "answered",
        Failed = "failed",
        Stopped = "stopped",
        Discarded = "discarded",
    }
}

string_enum! {
    /// An agent's state, kept on its row with its last sign of life.
    pub enum AgentStatus ("agent status") {
        Starting = "starting",
        Idle = "idle",
        Running = "running",
        Waiting = "waiting",
        Stopped = "stopped",
        Crashed = "crashed",
        Retired = "retired",
    }
}

string_enum! {
    /// A turn's state. Turns still `running` when the app last stopped are marked `interrupted` at
    /// startup.
    pub enum TurnStatus ("turn status") {
        Running = "running",
        Completed = "completed",
        Interrupted = "interrupted",
        Failed = "failed",
    }
}

string_enum! {
    /// Who a chat line is from: you (unmarked, authoritative), the agent's supervisor (marked), the
    /// app (marked), or the agent itself.
    pub enum MessageSource ("message source") {
        You = "you",
        Supervisor = "supervisor",
        App = "app",
        Agent = "agent",
    }
}

string_enum! {
    /// What a chat line is.
    pub enum MessageKind ("message kind") {
        Message = "message",
        Reply = "reply",
        Question = "question",
        Acknowledgement = "acknowledgement",
        AppNote = "app_note",
    }
}

string_enum! {
    /// What a crewmate's report says.
    pub enum ReportOutcome ("report outcome") {
        Done = "done",
        Blocked = "blocked",
        NeedsDecision = "needs_decision",
    }
}

string_enum! {
    /// Where a report came from: a `report` call (a clear report) or a turn's final text kept as
    /// the report when it ended without one.
    pub enum ReportSource ("report source") {
        ReportCall = "report_call",
        FinalText = "final_text",
    }
}

string_enum! {
    /// A review's verdict.
    pub enum ReviewVerdict ("review verdict") {
        Pass = "pass",
        Findings = "findings",
        Blocked = "blocked",
    }
}

string_enum! {
    /// A review finding's severity. Info-only findings count as a pass.
    pub enum Severity ("severity") {
        Error = "error",
        Warning = "warning",
        Info = "info",
    }
}

string_enum! {
    /// What a review finding asks for.
    pub enum FindingAction ("finding action") {
        AutoFix = "auto_fix",
        AskUser = "ask_user",
        NoOp = "no_op",
    }
}

string_enum! {
    /// A review's risk rating: recorded, never blocking a merge.
    pub enum RiskLevel ("risk level") {
        Low = "low",
        Medium = "medium",
        High = "high",
    }
}

string_enum! {
    /// What a failing check is, declared with `ci_diagnosis` before any repair.
    pub enum CiDiagnosis ("CI diagnosis") {
        OwnRegression = "own_regression",
        SuspectedFlake = "suspected_flake",
        FailingOnMain = "failing_on_main",
    }
}

string_enum! {
    /// What `github_read` reads.
    pub enum GithubReadKind ("GitHub read") {
        Issue = "issue",
        PullRequest = "pull_request",
        Comments = "comments",
        File = "file",
    }
}

string_enum! {
    /// What `github_write` does; held for your approval outside the crewmate's own project or in an
    /// answer-only task.
    pub enum GithubWriteAction ("GitHub write") {
        Comment = "comment",
        AddLabels = "add_labels",
        RemoveLabels = "remove_labels",
        OpenIssue = "open_issue",
    }
}

string_enum! {
    /// The kinds of held action: everything that waits for your yes. This set may grow (lines added
    /// only): `held_actions.kind` has no `CHECK` listing it, so adding a kind needs no migration,
    /// and `#[non_exhaustive]` keeps other crates' `match`es working.
    #[non_exhaustive]
    pub enum HeldKind ("held action kind") {
        Merge = "merge",
        GithubWrite = "github_write",
        AddProject = "add_project",
        Secondmate = "secondmate",
        Decision = "decision",
        /// A branch-only push to the remote, held under normal autonomy.
        Push = "push",
        /// A release tag the app creates and pushes: always held, under every autonomy, and
        /// answered only by you.
        ReleaseTag = "release_tag",
    }
}

string_enum! {
    /// Where a proposed release tag stands. `release_launches.status` has a `CHECK` listing these.
    /// Only `Proposed` may become `Pushing`; once `Pushing`, the app never sends the tag again.
    pub enum ReleaseLaunchStatus ("release launch status") {
        /// Waiting for your approval on its card.
        Proposed = "proposed",
        /// Approved; the tag push started. If the app stops here, the next start checks GitHub.
        Pushing = "pushing",
        /// Sent, but the remote didn't yet show the tag at the approved commit.
        Unconfirmed = "unconfirmed",
        /// The remote shows the tag pointing at the approved commit.
        Published = "published",
        /// The tag already existed on the remote at another object; nothing was overwritten.
        Collision = "collision",
        /// The remote refused the push.
        Rejected = "rejected",
        /// The push couldn't be made or the remote confirmed it never arrived.
        NotPublished = "not_published",
        /// The card stopped applying before it was answered.
        Superseded = "superseded",
        /// You rejected the card.
        Declined = "declined",
    }
}

impl ReleaseLaunchStatus {
    /// Holds the tag name for its repository: no second launch of the same tag may start.
    #[must_use]
    pub const fn holds_tag(self) -> bool {
        matches!(
            self,
            Self::Proposed | Self::Pushing | Self::Unconfirmed | Self::Published
        )
    }

    /// Nothing more will happen to it.
    #[must_use]
    pub const fn is_final(self) -> bool {
        !matches!(self, Self::Proposed | Self::Pushing | Self::Unconfirmed)
    }
}

string_enum! {
    /// The kind of git tag to create.
    pub enum TagKind ("tag kind") {
        /// A tag object with a tagger and message, pointing at the commit.
        Annotated = "annotated",
        /// A ref pointing straight at the commit.
        Lightweight = "lightweight",
    }
}

string_enum! {
    /// A held action's state. `Superseded` means it no longer applies (for example a held merge
    /// whose branch moved).
    pub enum HeldStatus ("held action status") {
        Pending = "pending",
        Approved = "approved",
        Rejected = "rejected",
        Answered = "answered",
        Superseded = "superseded",
    }
}

string_enum! {
    /// What a wake is about. This set grows as wakes are added (at the end). `wakes.kind` has no
    /// `CHECK`, so adding a kind needs no migration; Rust validates
    /// it when a wake is read. `#[non_exhaustive]` so an added kind never breaks another crate's
    /// `match`.
    #[non_exhaustive]
    pub enum WakeKind ("wake kind") {
        /// A crewmate reported (`report`), or its final text was kept as its report.
        Reported = "reported",
        /// A crewmate asked its supervisor for a decision (`ask_decision`).
        DecisionAsked = "decision_asked",
        /// A crewmate filed a follow-up proposal (`file_followup`).
        FollowupFiled = "followup_filed",
        /// A task this agent waits on finished (`declare_wait`).
        WaitOver = "wait_over",
        /// Check results on a crewmate's head commit.
        Checks = "checks",
        /// A crewmate's background job exited.
        JobExited = "job_exited",
        /// A watchdog step.
        Watchdog = "watchdog",
        /// You answered a held action; the outcome for the supervisor.
        HeldAnswered = "held_answered",
        /// A merge or delivery became a fact.
        Settled = "settled",
        /// You stepped into one of this supervisor's workers.
        Intervention = "intervention",
        /// A schedule came due (fired late if the app was closed).
        ScheduleDue = "schedule_due",
        /// The app restarted while this agent's work was in flight.
        Restarted = "restarted",
        /// A secondmate's parent-channel update.
        ParentReport = "parent_report",
        /// A request routed into a secondmate's scope.
        Routed = "routed",
        /// A message from the supervisor for a crewmate (`message_task`).
        Steer = "steer",
        /// The app ended a task as failed (setup failed, the worktree or session couldn't be made).
        TaskFailed = "task_failed",
        /// A running task was linked behind another (`link_tasks`).
        BlockedBy = "blocked_by",
        /// The crewmate's account came back after its usage limit.
        AccountReset = "account_reset",
        /// A secondmate's charter changed; the new charter text.
        CharterChanged = "charter_changed",
        /// Settling a task failed and nothing landed; it's back to reported.
        SettleFailed = "settle_failed",
        /// The app held a task for your decision; the supervisor adds its view.
        TaskHeld = "task_held",
        /// Your turn changed a running task's verification level.
        LevelChanged = "level_changed",
        /// The builder fixed its review findings: re-review its new head.
        ReReview = "re_review",
        /// The independent review passed: push and open the pull request.
        ReviewPassed = "review_passed",
        /// A pushed release tag's outcome became known after the push (published, collision or
        /// not published), read back from GitHub.
        ReleaseObserved = "release_observed",
    }
}

string_enum! {
    /// A request routed to a secondmate: open until its answer comes back on the parent channel.
    pub enum RequestStatus ("request status") {
        Open = "open",
        Answered = "answered",
        Closed = "closed",
    }
}

string_enum! {
    /// A schedule's state.
    pub enum ScheduleStatus ("schedule status") {
        Pending = "pending",
        Fired = "fired",
        Cancelled = "cancelled",
    }
}

string_enum! {
    /// Who a recorded process group belongs to.
    pub enum ProcessOwnerKind ("process owner") {
        /// An agent's engine process, such as one `claude`.
        Agent = "agent",
        /// A crewmate's background shell job.
        Job = "job",
        /// A project's setup command in a fresh worktree.
        Setup = "setup",
        /// Anything else the app runs and supervises.
        App = "app",
    }
}

/// Who started a turn.
///
/// A turn is started either by your own message or by an app event (a wake, a schedule, a
/// restart). Every typed override is accepted only on a turn you started; the check is one
/// function in `qm-policy`. For a crewmate's engine turns, a direct message from you is `User`
/// and everything else (its brief, a steer, a job finishing) is `AppEvent`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum StartedBy {
    /// Your message, by its row in `messages`.
    User { message: MessageId },
    /// A wake, a schedule or a restart.
    AppEvent,
}

impl StartedBy {
    pub const fn is_user(self) -> bool {
        matches!(self, Self::User { .. })
    }

    /// The message that started the turn, if you did.
    pub const fn message(self) -> Option<MessageId> {
        match self {
            Self::User { message } => Some(message),
            Self::AppEvent => None,
        }
    }
}
