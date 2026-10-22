//! How a tool call runs.
//!
//! Every call from either engine comes back to the core, which dispatches it here: the
//! [`Dispatcher`] finds the tool for the caller's role, parses the arguments into the tool's type
//! (unknown fields are refused), runs the bound handler, and renders the result. A handler is a
//! [`Handle`] impl for a tool's marker type, bound with [`bind`] in [`crate::handlers::HANDLERS`].
//!
//! A handler gets a [`ToolCx`]: who is calling (role, agent, task, and for orchestrators the turn
//! with its [`StartedBy`], which the override check in `qm-policy` needs), the clock, and the
//! [`ToolHost`]: the core's services, reached through a trait so this crate never depends on the
//! core. Each handler checks its rules with `qm-policy` and refuses with a plain reason
//! ([`ToolFailure::Refused`]) when one says no.

use crate::defs::{
    Ack, Constraint, DeliveryCheck, FileFollowupArgs, FindTasksArgs, FoundTasks, LaterStep, Linked,
    ReportArgs, TaskDetail, TaskStarted, TaskUpdated,
};
use crate::spec::{Tool, ToolDef};
use qm_engine::ToolResult;
use qm_types::BoxFuture;
use qm_types::clock::Clock;
use qm_types::domain::{Effort, PlanMode, TaskKind, VerificationLevel};
use qm_types::domain::{MessageKind, Role, StartedBy};
use qm_types::error::Refusal;
use qm_types::ids::{AgentId, MessageId, SecondmateId, StandingInstructionId, TaskId, TurnId};
use qm_types::ids::{CorrelationId, ModelId};
use std::fmt;

/// The core's services, as tool handlers need them.
///
/// Implemented by `qm-core` (`crates/qm-core/src/tool_host.rs`); handlers are tested against fake
/// hosts.
///
/// **Additive by rule:** every method has a default body that fails with
/// [`ToolFailure::not_available`], so adding one never breaks the core's host; the core overrides
/// each method it implements. A method without a default body is a breaking change. A method
/// looks like this:
///
/// ```text
/// fn task_detail<'a>(&'a self, task: TaskId) -> BoxFuture<'a, Result<TaskDetail, ToolFailure>> {
///     Box::pin(async { Err(ToolFailure::not_available("task_detail")) })
/// }
/// ```
pub trait ToolHost: Send + Sync {
    /// Shows a line in the chat from `reply` or `ask_user`: a row in the caller's conversation, on
    /// its turn, replying to the message that started the turn.
    fn show_message<'a>(
        &'a self,
        caller: &'a Caller,
        message: NewMessage,
    ) -> BoxFuture<'a, Result<MessageId, ToolFailure>> {
        let _ = (caller, message);
        Box::pin(async { Err(ToolFailure::not_available("show_message")) })
    }

    /// Puts an acknowledgement beside your message; the newest one replying to a message is the
    /// one shown, so a later call replaces an earlier one.
    fn acknowledge<'a>(
        &'a self,
        caller: &'a Caller,
        your_message: MessageId,
        text: &'a str,
    ) -> BoxFuture<'a, Result<MessageId, ToolFailure>> {
        let _ = (caller, your_message, text);
        Box::pin(async { Err(ToolFailure::not_available("acknowledge")) })
    }

    /// Keeps a standing instruction from your message on the caller's turn.
    fn save_instruction<'a>(
        &'a self,
        caller: &'a Caller,
        your_message: MessageId,
        text: &'a str,
    ) -> BoxFuture<'a, Result<StandingInstructionId, ToolFailure>> {
        let _ = (caller, your_message, text);
        Box::pin(async { Err(ToolFailure::not_available("save_instruction")) })
    }

    /// Drops a standing instruction; refused when it doesn't exist or was already dropped.
    fn drop_instruction<'a>(
        &'a self,
        caller: &'a Caller,
        instruction: StandingInstructionId,
    ) -> BoxFuture<'a, Result<(), ToolFailure>> {
        let _ = (caller, instruction);
        Box::pin(async { Err(ToolFailure::not_available("drop_instruction")) })
    }

    /// Starts a task and its crewmate (`start_crewmate`, both variants). Returns once the base is
    /// settled and delivery checked; the rest runs in the background.
    fn start_crewmate<'a>(
        &'a self,
        caller: &'a Caller,
        request: StartRequest,
    ) -> BoxFuture<'a, Result<TaskStarted, ToolFailure>> {
        let _ = (caller, request);
        Box::pin(async { Err(ToolFailure::not_available("start_crewmate")) })
    }

    /// Steers a direct report's task with a message marked as from its supervisor.
    fn message_task<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
        text: String,
    ) -> BoxFuture<'a, Result<TaskUpdated, ToolFailure>> {
        let _ = (caller, task, text);
        Box::pin(async { Err(ToolFailure::not_available("message_task")) })
    }

    /// Stops a direct report's task and retires it; its uncommitted changes and unmerged commits go too.
    fn stop_task<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
        reason: Option<String>,
    ) -> BoxFuture<'a, Result<TaskUpdated, ToolFailure>> {
        let _ = (caller, task, reason);
        Box::pin(async { Err(ToolFailure::not_available("stop_task")) })
    }

    /// Closes a direct report's finished task and retires its worktree.
    fn close_task<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
    ) -> BoxFuture<'a, Result<TaskUpdated, ToolFailure>> {
        let _ = (caller, task);
        Box::pin(async { Err(ToolFailure::not_available("close_task")) })
    }

    /// Records that `blocker` blocks `blocked`.
    fn link_tasks<'a>(
        &'a self,
        caller: &'a Caller,
        blocker: TaskId,
        blocked: TaskId,
    ) -> BoxFuture<'a, Result<Linked, ToolFailure>> {
        let _ = (caller, blocker, blocked);
        Box::pin(async { Err(ToolFailure::not_available("link_tasks")) })
    }

    /// One task's full history (`task_status`).
    fn task_detail<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
    ) -> BoxFuture<'a, Result<TaskDetail, ToolFailure>> {
        let _ = (caller, task);
        Box::pin(async { Err(ToolFailure::not_available("task_detail")) })
    }

    /// Searches tasks (`find_tasks`).
    fn find_tasks<'a>(
        &'a self,
        caller: &'a Caller,
        query: FindTasksArgs,
    ) -> BoxFuture<'a, Result<FoundTasks, ToolFailure>> {
        let _ = (caller, query);
        Box::pin(async { Err(ToolFailure::not_available("find_tasks")) })
    }

    /// Records a crewmate's report and wakes its supervisor.
    fn record_report<'a>(
        &'a self,
        caller: &'a Caller,
        report: ReportArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        let _ = (caller, report);
        Box::pin(async { Err(ToolFailure::not_available("record_report")) })
    }

    /// `plan_check`: checks the caller's `plan.md` under the live plan mode and, under normal
    /// autonomy, renders its page; one line, or every problem.
    fn plan_check<'a>(&'a self, caller: &'a Caller) -> BoxFuture<'a, Result<String, ToolFailure>> {
        let _ = caller;
        Box::pin(async { Err(ToolFailure::not_available("plan_check")) })
    }

    /// `approve_plan`: the user approved a plan task's plan (the handler has checked the turn).
    fn approve_plan<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
        take_recommended: bool,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        let _ = (caller, task, take_recommended);
        Box::pin(async { Err(ToolFailure::not_available("approve_plan")) })
    }

    /// A crewmate asks its supervisor for a decision.
    fn ask_decision<'a>(
        &'a self,
        caller: &'a Caller,
        question: String,
        options: Vec<String>,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        let _ = (caller, question, options);
        Box::pin(async { Err(ToolFailure::not_available("ask_decision")) })
    }

    /// A crewmate declares a wait on another task or on something outside.
    fn declare_wait<'a>(
        &'a self,
        caller: &'a Caller,
        wait: DeclaredWait,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        let _ = (caller, wait);
        Box::pin(async { Err(ToolFailure::not_available("declare_wait")) })
    }

    /// A crewmate proposes a follow-up task to its supervisor.
    fn file_followup<'a>(
        &'a self,
        caller: &'a Caller,
        followup: FileFollowupArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        let _ = (caller, followup);
        Box::pin(async { Err(ToolFailure::not_available("file_followup")) })
    }

    /// What the crewmate can do to deliver, from the app's own checks.
    fn delivery_check<'a>(
        &'a self,
        caller: &'a Caller,
    ) -> BoxFuture<'a, Result<DeliveryCheck, ToolFailure>> {
        let _ = caller;
        Box::pin(async { Err(ToolFailure::not_available("delivery_check")) })
    }

    /// `start_secondmate`: stores the proposal as a held action for your approval; nothing is
    /// created before that.
    fn propose_secondmate<'a>(
        &'a self,
        caller: &'a Caller,
        proposal: SecondmateProposal,
    ) -> BoxFuture<'a, Result<crate::defs::Held, ToolFailure>> {
        let _ = (caller, proposal);
        Box::pin(async { Err(ToolFailure::not_available("propose_secondmate")) })
    }

    /// `route_to_secondmate`: a request into one of the caller's own secondmates' scope, with a
    /// fresh correlation ID stamped by the app.
    fn route_to_secondmate<'a>(
        &'a self,
        caller: &'a Caller,
        request: RouteRequest,
    ) -> BoxFuture<'a, Result<crate::defs::Routed, ToolFailure>> {
        let _ = (caller, request);
        Box::pin(async { Err(ToolFailure::not_available("route_to_secondmate")) })
    }

    /// `configure_secondmate`: changes a secondmate's charter; the standing branch only on your
    /// turns, affecting only crewmates started afterwards.
    fn configure_secondmate<'a>(
        &'a self,
        caller: &'a Caller,
        change: CharterChange,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        let _ = (caller, change);
        Box::pin(async { Err(ToolFailure::not_available("configure_secondmate")) })
    }

    /// `close_secondmate`: retires one of your secondmates once it has no open tasks.
    fn close_secondmate<'a>(
        &'a self,
        caller: &'a Caller,
        secondmate: SecondmateId,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        let _ = (caller, secondmate);
        Box::pin(async { Err(ToolFailure::not_available("close_secondmate")) })
    }

    /// `parent_report`: a secondmate's judgment on one of its requests; the app adds its own facts
    /// snapshot and wakes the Quartermaster.
    fn parent_report<'a>(
        &'a self,
        caller: &'a Caller,
        judgment: ParentJudgment,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        let _ = (caller, judgment);
        Box::pin(async { Err(ToolFailure::not_available("parent_report")) })
    }

    /// `find_project_folder`: folders named `name` (any case) that contain a `.git`, up to three
    /// levels under the search roots in settings, best first.
    fn find_project_folder<'a>(
        &'a self,
        caller: &'a Caller,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, ToolFailure>> {
        let _ = (caller, name);
        Box::pin(async { Err(ToolFailure::not_available("find_project_folder")) })
    }

    /// `propose_project`: stores the proposal as a held action with its location filled in, for you
    /// to approve; nothing is linked, cloned or created before that.
    fn propose_project<'a>(
        &'a self,
        caller: &'a Caller,
        proposal: &'a crate::defs::ProposeProjectArgs,
    ) -> BoxFuture<'a, Result<crate::defs::Held, ToolFailure>> {
        let _ = (caller, proposal);
        Box::pin(async { Err(ToolFailure::not_available("propose_project")) })
    }

    /// `configure_project`: changes a registered project's settings; autonomy and the verification
    /// level aren't among them.
    fn configure_project<'a>(
        &'a self,
        caller: &'a Caller,
        change: &'a crate::defs::ConfigureProjectArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        let _ = (caller, change);
        Box::pin(async { Err(ToolFailure::not_available("configure_project")) })
    }

    /// `remove_project`: unregisters a project; its files are never touched.
    fn remove_project<'a>(
        &'a self,
        caller: &'a Caller,
        project: &'a str,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        let _ = (caller, project);
        Box::pin(async { Err(ToolFailure::not_available("remove_project")) })
    }

    /// `pr_open`: pushes the caller's branch and opens or updates its pull request, after the gate,
    /// the body, names and repair checks.
    fn open_pull_request<'a>(
        &'a self,
        caller: &'a Caller,
        title: String,
        body: String,
    ) -> BoxFuture<'a, Result<crate::defs::PrOpened, ToolFailure>> {
        let _ = (caller, title, body);
        Box::pin(async { Err(ToolFailure::not_available("pr_open")) })
    }

    /// `pr_status`: the caller's pull request as GitHub reports it now.
    fn pull_request_status<'a>(
        &'a self,
        caller: &'a Caller,
    ) -> BoxFuture<'a, Result<crate::defs::PrStatus, ToolFailure>> {
        let _ = caller;
        Box::pin(async { Err(ToolFailure::not_available("pr_status")) })
    }

    /// `ci_results`: the checks on the caller's pushed head, with log excerpts.
    fn ci_results<'a>(
        &'a self,
        caller: &'a Caller,
        check: Option<String>,
    ) -> BoxFuture<'a, Result<crate::defs::CiReport, ToolFailure>> {
        let _ = (caller, check);
        Box::pin(async { Err(ToolFailure::not_available("ci_results")) })
    }

    /// `github_read`: an issue, a pull request, its comments or a file.
    fn github_read<'a>(
        &'a self,
        caller: &'a Caller,
        args: crate::defs::GithubReadArgs,
    ) -> BoxFuture<'a, Result<crate::defs::GithubContent, ToolFailure>> {
        let _ = (caller, args);
        Box::pin(async { Err(ToolFailure::not_available("github_read")) })
    }

    /// `github_write`: written at once in the crewmate's own project, else held.
    fn github_write<'a>(
        &'a self,
        caller: &'a Caller,
        args: crate::defs::GithubWriteArgs,
    ) -> BoxFuture<'a, Result<crate::defs::GithubWriteOutcome, ToolFailure>> {
        let _ = (caller, args);
        Box::pin(async { Err(ToolFailure::not_available("github_write")) })
    }

    /// `settle_task`: lands a direct report's finished work under its project's delivery mode and
    /// autonomy, or holds it for your approval.
    fn settle_task<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
    ) -> BoxFuture<'a, Result<crate::defs::SettleOutcome, ToolFailure>> {
        let _ = (caller, task);
        Box::pin(async { Err(ToolFailure::not_available("settle_task")) })
    }

    /// `ci_diagnosis`: declares what a failing check on the caller's pushed head is.
    fn declare_diagnosis<'a>(
        &'a self,
        caller: &'a Caller,
        args: crate::defs::CiDiagnosisArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        let _ = (caller, args);
        Box::pin(async { Err(ToolFailure::not_available("declare_diagnosis")) })
    }

    /// `ci_rerun`: reruns one failing check, once per task, after a flake diagnosis with evidence.
    fn rerun_check<'a>(
        &'a self,
        caller: &'a Caller,
        check: String,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        let _ = (caller, check);
        Box::pin(async { Err(ToolFailure::not_available("rerun_check")) })
    }

    /// `present_hold`: the supervisor's summary on a held or stuck task's decision card.
    fn present_hold<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
        summary: String,
    ) -> BoxFuture<'a, Result<crate::defs::Held, ToolFailure>> {
        let _ = (caller, task, summary);
        Box::pin(async { Err(ToolFailure::not_available("present_hold")) })
    }

    /// `set_verification`: a running task's level, after the handler's override check.
    fn set_verification<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
        level: VerificationLevel,
    ) -> BoxFuture<'a, Result<crate::defs::VerificationChanged, ToolFailure>> {
        let _ = (caller, task, level);
        Box::pin(async { Err(ToolFailure::not_available("set_verification")) })
    }

    /// `schedule`: a wake for the caller at `due`, already checked to be in the future; stored in
    /// UTC with the user's time zone.
    fn add_schedule<'a>(
        &'a self,
        caller: &'a Caller,
        due: jiff::Timestamp,
        what: String,
    ) -> BoxFuture<'a, Result<crate::defs::Scheduled, ToolFailure>> {
        let _ = (caller, due, what);
        Box::pin(async { Err(ToolFailure::not_available("schedule")) })
    }

    /// `cancel_schedule`: cancels one of the caller's pending schedules.
    fn cancel_schedule<'a>(
        &'a self,
        caller: &'a Caller,
        schedule: qm_types::ids::ScheduleId,
    ) -> BoxFuture<'a, Result<(), ToolFailure>> {
        let _ = (caller, schedule);
        Box::pin(async { Err(ToolFailure::not_available("cancel_schedule")) })
    }
}

/// Everything `start_crewmate` carries, for both the Quartermaster's and a secondmate's variant. A
/// secondmate's variant sets `correlation`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartRequest {
    pub project: String,
    pub kind: TaskKind,
    pub title: String,
    pub goal: String,
    pub constraints: Vec<Constraint>,
    pub tests_required: bool,
    pub builds_on: Option<TaskId>,
    pub instructions: Vec<String>,
    pub later_steps: Vec<LaterStep>,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    pub premium: bool,
    pub base_branch: Option<String>,
    pub verification: Option<VerificationLevel>,
    pub stronger_retry_of: Option<TaskId>,
    pub correlation: Option<CorrelationId>,
}

/// A proposed secondmate, after the handler's checks (`start_secondmate`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecondmateProposal {
    pub name: String,
    pub scope: String,
    pub projects: Vec<String>,
    pub base_branch: Option<String>,
}

/// A request to route, after the handler's checks (`route_to_secondmate`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteRequest {
    pub secondmate: SecondmateId,
    pub request: String,
    pub instructions: Vec<String>,
    pub base_branch: Option<String>,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    pub premium: bool,
    pub later_steps: Vec<LaterStep>,
}

/// A change to a secondmate's charter (`configure_secondmate`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CharterChange {
    pub secondmate: SecondmateId,
    pub scope: Option<String>,
    pub projects: Option<Vec<String>>,
    /// `Some(None)` clears the standing branch.
    pub standing_branch: Option<Option<String>>,
}

/// A secondmate's judgment on the parent channel, after the handler's checks (`parent_report`).
/// There is no facts field: the app writes the facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParentJudgment {
    pub correlation: CorrelationId,
    pub summary: String,
    pub recommendation: Option<String>,
    pub needs_user: Option<String>,
}

/// What a crewmate waits on (`declare_wait`): exactly one of the two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeclaredWait {
    /// Another task, until it finishes.
    Task(TaskId),
    /// Something outside the app, until the crewmate's next turn starts.
    External(String),
}

/// A line for the chat from `reply` or `ask_user`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMessage {
    /// `Reply` or `Question`.
    pub kind: MessageKind,
    pub text: String,
    /// A question's one-click answers.
    pub options: Vec<String>,
}

/// Who is calling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caller {
    pub agent: AgentId,
    pub role: Role,
    /// The turn the call belongs to, with who started it. Always present for orchestrators, whose
    /// overrides depend on it; a crewmate's call may carry its turn too, but no crewmate tool takes
    /// an override.
    pub turn: Option<TurnCx>,
    /// A crewmate's task.
    pub task: Option<TaskId>,
    /// The secondmate making the call, or the secondmate a crewmate works under.
    pub secondmate: Option<SecondmateId>,
    /// The plan mode a plan task's crewmate's session was started in, which picks its tool
    /// variants; `None` for every other caller.
    pub plan: Option<PlanMode>,
}

/// The orchestrator turn a call belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnCx {
    pub turn: TurnId,
    /// Overrides are accepted only on turns you started.
    pub started_by: StartedBy,
}

/// Everything a handler gets besides its arguments.
pub struct ToolCx<'a> {
    pub caller: &'a Caller,
    pub clock: &'a dyn Clock,
    pub host: &'a dyn ToolHost,
}

impl fmt::Debug for ToolCx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolCx")
            .field("caller", self.caller)
            .finish_non_exhaustive()
    }
}

/// Why a call didn't succeed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolFailure {
    /// A rule said no; the reason is written for the agent.
    Refused(Refusal),
    /// Something went wrong (GitHub unreachable, a git command failed), in a plain sentence.
    Failed(String),
}

impl ToolFailure {
    /// What a [`ToolHost`] method returns until the core implements it.
    pub fn not_available(service: &str) -> Self {
        Self::Failed(format!("{service} isn't available in this build yet"))
    }

    /// The text the model gets back.
    pub fn render(&self) -> String {
        match self {
            Self::Refused(r) => format!("refused: {}", r.reason),
            Self::Failed(m) => format!("error: {m}"),
        }
    }
}

impl From<Refusal> for ToolFailure {
    fn from(r: Refusal) -> Self {
        Self::Refused(r)
    }
}

/// Runs one tool.
pub trait Handle: Tool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: Self::Args,
    ) -> BoxFuture<'a, Result<Self::Output, ToolFailure>>;
}

type ErasedCall = for<'a> fn(&'a ToolCx<'a>, serde_json::Value) -> BoxFuture<'a, ToolResult>;

/// A handler bound to its tool.
#[derive(Clone, Copy)]
pub struct Binding {
    /// The tool's [`Tool::KEY`].
    pub key: &'static str,
    call: ErasedCall,
}

impl fmt::Debug for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Binding")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// Binds a tool's handler, for [`crate::handlers::HANDLERS`].
pub const fn bind<T: Handle>() -> Binding {
    Binding {
        key: T::KEY,
        call: call_erased::<T>,
    }
}

fn call_erased<'a, T: Handle>(
    cx: &'a ToolCx<'a>,
    args: serde_json::Value,
) -> BoxFuture<'a, ToolResult> {
    Box::pin(async move {
        let args: T::Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => {
                let r = Refusal::new(format!("invalid arguments for {}: {e}", T::NAME));
                return ToolResult::error(ToolFailure::Refused(r).render());
            }
        };
        match T::handle(cx, args).await {
            Ok(out) => ToolResult::ok(T::render(&out)),
            Err(failure) => ToolResult::error(failure.render()),
        }
    })
}

/// The outcome of one dispatched call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dispatched {
    pub result: ToolResult,
    /// A successful call of a counting tool: an effective action.
    pub counted: bool,
}

/// Dispatches tool calls to their handlers.
#[derive(Clone, Copy, Debug)]
pub struct Dispatcher {
    defs: &'static [ToolDef],
    handlers: &'static [Binding],
}

impl Dispatcher {
    /// The registry's tools with the bound handlers.
    pub fn builtin() -> Self {
        Self::new(crate::registry::TOOLS, crate::handlers::HANDLERS)
    }

    pub const fn new(defs: &'static [ToolDef], handlers: &'static [Binding]) -> Self {
        Self { defs, handlers }
    }

    pub fn defs(&self) -> &'static [ToolDef] {
        self.defs
    }

    /// Runs one call by the caller in `cx`.
    pub async fn dispatch(
        &self,
        cx: &ToolCx<'_>,
        name: &str,
        args: serde_json::Value,
    ) -> Dispatched {
        let role = cx.caller.role;
        let plan = cx.caller.plan;
        let Some(def) = self
            .defs
            .iter()
            .find(|d| d.name == name && d.offered(role, plan))
        else {
            if plan.is_some()
                && self
                    .defs
                    .iter()
                    .any(|d| d.name == name && d.roles.contains(&role))
            {
                return failed(format!("there is no tool named {name} for this task"));
            }
            return failed(format!("there is no tool named {name} for a {role}"));
        };
        let Some(binding) = self.handlers.iter().find(|b| b.key == def.key) else {
            return Dispatched {
                result: ToolResult::error(ToolFailure::not_available(name).render()),
                counted: false,
            };
        };
        let result = (binding.call)(cx, args).await;
        let counted = def.class.counts() && !result.is_error;
        Dispatched { result, counted }
    }
}

fn failed(message: String) -> Dispatched {
    Dispatched {
        result: ToolResult::error(ToolFailure::Failed(message).render()),
        counted: false,
    }
}
