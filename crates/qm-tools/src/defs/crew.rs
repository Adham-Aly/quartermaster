//! Starting, steering, settling and looking up work: the orchestrators' tools.

use super::{Constraint, Held, LaterStep, RoutingSummary, TaskSummary};
use crate::{Tool, ToolClass};
use jiff::Timestamp;
use qm_types::domain::{Effort, Role, TaskKind, TaskStatus, VerificationLevel};
use qm_types::ids::{CommitSha, CorrelationId, HeldActionId, ModelId, TaskId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

tool! {
    /// Starts a task. The app checks: the project is registered, the kind is valid, `builds_on` is
    /// checked as a fact, your words are copied by the app from your message, every override needs
    /// a turn you started, and the model comes from routing.
    StartCrewmateTool {
        name: "start_crewmate",
        class: Act,
        roles: [Quartermaster],
        args: StartCrewmateArgs,
        output: TaskStarted,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartCrewmateArgs {
    // The folder to work in, absolute or from `~`; its project is found or added. Or name a listed
    // project instead. Neither: your home folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    // A listed project's name, instead of a folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    // Work in the folder itself rather than a worktree, in a git project: only when the user's own
    // words ask for it. A plain folder is always worked in place.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_place: bool,
    pub kind: TaskKind,
    // Conventional-commit style, e.g. "fix(auth): token refresh race".
    pub title: String,
    pub goal: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<Constraint>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tests_required: bool,
    // A finished plan, a reproduction, or the task a review checks; checked as a fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builds_on: Option<TaskId>,
    // One-time instructions from the supervisor, for this task only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instructions: Vec<String>,
    // Overrides from here on: only on a turn you started.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub later_steps: Vec<LaterStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub premium: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<VerificationLevel>,
    // "Try again with a stronger model" for that task's failed work, when you ask for it in your
    // own words: a new task with its own crewmate, one tier up or premium, linked by
    // `tasks.retry_of` and carrying the escalation count forward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stronger_retry_of: Option<TaskId>,
}

/// A secondmate's `start_crewmate`: the same, with no base branch, which the app fills in from the
/// routed request, else the secondmate's standing branch, else the default.
///
/// `correlation` names the routed request the task serves. The app then applies what the stored
/// request carries, all of it authorised on the Quartermaster's turn that routed it: its base
/// branch, your words, and its model, effort, premium and later steps. It sets
/// `tasks.secondmate_request_id`. The secondmate's own override fields still need a turn you
/// started (your direct message to the secondmate); on any other turn they are refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SecondmateStartCrewmateTool;

impl Tool for SecondmateStartCrewmateTool {
    const NAME: &'static str = "start_crewmate";
    const KEY: &'static str = "start_crewmate.secondmate";
    const DESCRIPTION: &'static str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/descriptions/start_crewmate.secondmate.md"
    ))
    .trim_ascii();
    const CLASS: ToolClass = ToolClass::Act;
    const ROLES: &'static [Role] = &[Role::Secondmate];
    type Args = SecondmateStartCrewmateArgs;
    type Output = TaskStarted;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SecondmateStartCrewmateArgs {
    // The folder to work in, absolute or from `~`; or name a listed project instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    // Work in the folder itself rather than a worktree: only when the user's own words ask for it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_place: bool,
    // The routed request this task serves, whose stored fields the app applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<CorrelationId>,
    // Leave it out: the app fills it in. If present it must match, or the call is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    pub kind: TaskKind,
    pub title: String,
    pub goal: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<Constraint>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tests_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builds_on: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instructions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub later_steps: Vec<LaterStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub premium: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<VerificationLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stronger_retry_of: Option<TaskId>,
}

/// A task was started.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStarted {
    pub task: TaskId,
    /// The task's branch, named after its title (`fix/upload-retry-on-slow-tmp`); folder tasks
    /// have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub routing: RoutingSummary,
}

tool! {
    /// Steers a running task; the target must be a direct report, and the message is marked as from
    /// its supervisor.
    MessageTaskTool {
        name: "message_task",
        class: Act,
        roles: [Quartermaster, Secondmate],
        args: MessageTaskArgs,
        output: TaskUpdated,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MessageTaskArgs {
    pub task: TaskId,
    pub text: String,
}

tool! {
    /// Stops a direct report's work and retires it: its worktree and branch are removed. A
    /// plain-folder task's edits stay where they are.
    StopTaskTool {
        name: "stop_task",
        class: Act,
        roles: [Quartermaster, Secondmate],
        args: StopTaskArgs,
        output: TaskUpdated,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StopTaskArgs {
    pub task: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

tool! {
    /// Changes the project label of a direct report's in-place task. Worktree tasks keep their
    /// repository's label.
    SetTaskProjectTool {
        name: "set_task_project",
        class: Act,
        roles: [Quartermaster, Secondmate],
        args: SetTaskProjectArgs,
        output: TaskUpdated,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetTaskProjectArgs {
    pub task: TaskId,
    // A listed project's name, or a folder (absolute, or from ~): one of the two.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
}

tool! {
    /// Closes a direct report's task and retires its worktree and branch. A plain-folder task
    /// keeps its files where they are.
    CloseTaskTool {
        name: "close_task",
        class: Act,
        roles: [Quartermaster, Secondmate],
        args: CloseTaskArgs,
        output: TaskUpdated,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloseTaskArgs {
    pub task: TaskId,
}

/// A task's state after a call that changed it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskUpdated {
    pub task: TaskId,
    pub status: TaskStatus,
}

tool! {
    /// Lands, merges or closes finished work. Answer-only tasks can't land; review must have passed
    /// for `reviewed`; normal autonomy means a held action; only the approved commit is merged.
    SettleTaskTool {
        name: "settle_task",
        class: Act,
        roles: [Quartermaster, Secondmate],
        args: SettleTaskArgs,
        output: SettleOutcome,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SettleTaskArgs {
    pub task: TaskId,
}

/// What settling did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SettleOutcome {
    /// Direct pull request: merged on GitHub at exactly this commit.
    Merged { pr: u64, commit: CommitSha },
    /// Branch only: pushed and marked delivered. With no remote, marked delivered and kept locally.
    Delivered { branch: String, commit: CommitSha },
    /// Build in place: merged into your checkout's current branch.
    Landed { branch: String, commit: CommitSha },
    /// Folder work: its changes are already in the folder; nothing was merged or pushed.
    Done { folder: String },
    /// An answer-only task: its answer is kept and the task closed.
    Closed,
    /// Autonomy is off: waiting for your approval.
    Held {
        held_action: HeldActionId,
        note: String,
    },
}

tool! {
    /// Changes a running task's verification level. The level is an override: the handler calls
    /// `qm_policy::overrides::check_override` with `OverrideKind::VerificationLevel` first, so it
    /// works only on a turn you started. Raising it means the missing validation or review must
    /// happen before the next push or merge, even with a pull request open; lowering it removes
    /// only the review hold (history, failed checks and other holds stay); a level change never
    /// grants permission to merge. Refused for tasks with no level: answer-only, review and
    /// build-in-place tasks.
    SetVerificationTool {
        name: "set_verification",
        class: Act,
        roles: [Quartermaster, Secondmate],
        args: SetVerificationArgs,
        output: VerificationChanged,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetVerificationArgs {
    pub task: TaskId,
    pub level: VerificationLevel,
}

/// A task's verification level changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationChanged {
    pub task: TaskId,
    pub from: VerificationLevel,
    pub to: VerificationLevel,
    /// What the change means for this task now, in a sentence, e.g. "A review must pass before
    /// the next merge."
    pub effect: String,
}

tool! {
    /// Puts a task the app holds to you as one decision; the app attaches the fixed choices.
    PresentHoldTool {
        name: "present_hold",
        class: Act,
        roles: [Quartermaster, Secondmate],
        args: PresentHoldArgs,
        output: Held,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PresentHoldArgs {
    pub task: TaskId,
    // The supervisor's short summary; the app writes the title, reason and choices.
    pub summary: String,
}

tool! {
    /// Marks one task as blocking another; no cycles, both tasks exist.
    LinkTasksTool {
        name: "link_tasks",
        class: Act,
        roles: [Quartermaster, Secondmate],
        args: LinkTasksArgs,
        output: Linked,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkTasksArgs {
    pub blocker: TaskId,
    pub blocked: TaskId,
}

/// A "blocks" relation was recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Linked {
    pub blocker: TaskId,
    pub blocked: TaskId,
}

tool! {
    /// One task's full history. Read only.
    TaskStatusTool {
        name: "task_status",
        class: Read,
        roles: [Quartermaster, Secondmate],
        args: TaskStatusArgs,
        output: TaskDetail,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskStatusArgs {
    pub task: TaskId,
}

/// One task in full.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDetail {
    pub summary: TaskSummary,
    pub goal: String,
    /// Every step since the task started, oldest first.
    pub history: Vec<HistoryEntry>,
    /// An answer-only task's answer, once reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

/// One step in a task's history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub at: Timestamp,
    pub text: String,
}

tool! {
    /// Searches all tasks. Read only.
    FindTasksTool {
        name: "find_tasks",
        class: Read,
        roles: [Quartermaster, Secondmate],
        args: FindTasksArgs,
        output: FoundTasks,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindTasksArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    // Empty means open and active tasks; name closed statuses to search those.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub status: Vec<TaskStatus>,
    // A task ID, branch name or pull request number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<TaskId>,
}

/// Matching tasks, capped like the context block, with how many more matched.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoundTasks {
    pub tasks: Vec<TaskSummary>,
    pub more: u32,
}
