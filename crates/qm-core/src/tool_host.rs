//! The core's services for tool handlers (`qm_tools::handler::ToolHost`).
//!
//! [`CoreToolHost`] writes what the handlers in `qm-tools` decided, after their checks passed:
//! chat lines from `reply`, `ask_user` and `acknowledge`, and standing instructions. It signals
//! the conversation after every change you can see, so the UI gets a fresh snapshot. `ToolHost`
//! methods this host doesn't implement keep their default bodies.

use crate::api::Conversation;
use crate::decisions::DecisionService;
use crate::delivery::DeliveryService;
use crate::parent_channel::ParentChannel;
use crate::projects::ProjectService;
use crate::secondmates::SecondmateService;
use crate::settling::SettlingService;
use crate::tasks::TaskService;
use crate::turn_io::ChatLog;
use crate::verification::VerificationService;
use qm_store::StoreError;
use qm_store::db::Db;
use qm_store::events::{self, NewEvent};
use qm_store::messages::{self, NewMessage as MessageRow};
use qm_store::standing_instructions;
use qm_tools::defs::{
    Ack, ConfigureProjectArgs, Held, ProposeReleaseTagArgs, ReleaseStatus, ReleaseStatusArgs,
};
use qm_tools::defs::{
    CiReport, GithubContent, GithubReadArgs, GithubWriteArgs, GithubWriteOutcome, PrOpened,
    PrStatus, SettleOutcome,
};
use qm_tools::defs::{
    DeliveryCheck, FileFollowupArgs, FindTasksArgs, FoundTasks, Linked, ReportArgs, TaskDetail,
    TaskStarted, TaskUpdated, Unlinked,
};
use qm_tools::defs::{Routed, TaskChanged};
use qm_tools::handler::ParentJudgment;
use qm_tools::handler::{Caller, NewMessage, ToolFailure, ToolHost};
use qm_tools::handler::{CharterChange, RouteRequest, SecondmateProposal};
use qm_tools::handler::{DeclaredWait, StartRequest};
use qm_types::BoxFuture;
use qm_types::clock::Clock;
use qm_types::domain::{MessageKind, MessageSource, Role};
use qm_types::error::Refusal;
use qm_types::ids::{MessageId, StandingInstructionId};
use qm_types::ids::{SecondmateId, TaskId, TaskLabelId};
use std::fmt;
use std::sync::Arc;

/// A standing instruction was saved.
pub const INSTRUCTION_SAVED: &str = "seat.instruction_saved";
/// A standing instruction was dropped.
pub const INSTRUCTION_DROPPED: &str = "seat.instruction_dropped";

/// The core's [`ToolHost`].
pub struct CoreToolHost {
    db: Db,
    clock: Arc<dyn Clock>,
    conversation: Arc<Conversation>,
    tasks: Option<TaskService>,
    /// The project registry; `None` means "not available".
    projects: Option<ProjectService>,
    /// The secondmates; `None` means "not available".
    secondmates: Option<SecondmateService>,
    /// The parent channel; `None` means "not available".
    parent_channel: Option<ParentChannel>,
    /// Pull requests, checks and the GitHub tools; `None` means "not available".
    delivery: Option<DeliveryService>,
    /// `settle_task`; `None` means "not available".
    settling: Option<SettlingService>,
    /// Verification and decisions; `None` means "not available".
    verification: Option<VerificationService>,
    decisions: Option<DecisionService>,
    /// `schedule` and `cancel_schedule`; unset means "not available".
    schedules: std::sync::OnceLock<crate::schedules::ScheduleService>,
}

impl fmt::Debug for CoreToolHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoreToolHost").finish_non_exhaustive()
    }
}

impl CoreToolHost {
    pub fn new(db: Db, clock: Arc<dyn Clock>, conversation: Arc<Conversation>) -> Self {
        Self {
            db,
            clock,
            conversation,
            tasks: None,
            projects: None,
            secondmates: None,
            parent_channel: None,
            delivery: None,
            settling: None,
            verification: None,
            decisions: None,
            schedules: std::sync::OnceLock::new(),
        }
    }

    /// Puts the schedule service behind `schedule` and `cancel_schedule`; a second call is
    /// ignored.
    pub fn set_schedules(&self, schedules: crate::schedules::ScheduleService) {
        let _ = self.schedules.set(schedules);
    }

    fn schedules(&self, tool: &str) -> Result<&crate::schedules::ScheduleService, ToolFailure> {
        self.schedules
            .get()
            .ok_or_else(|| ToolFailure::not_available(tool))
    }

    /// The host with the verification tools (`ci_diagnosis`, `ci_rerun`, `set_verification`).
    #[must_use]
    pub fn with_verification(mut self, verification: VerificationService) -> Self {
        self.verification = Some(verification);
        self
    }

    /// The host with `present_hold`.
    #[must_use]
    pub fn with_decisions(mut self, decisions: DecisionService) -> Self {
        self.decisions = Some(decisions);
        self
    }

    fn verification(&self, tool: &str) -> Result<&VerificationService, ToolFailure> {
        self.verification
            .as_ref()
            .ok_or_else(|| ToolFailure::not_available(tool))
    }

    fn decisions(&self, tool: &str) -> Result<&DecisionService, ToolFailure> {
        self.decisions
            .as_ref()
            .ok_or_else(|| ToolFailure::not_available(tool))
    }

    /// The host with delivery's tools.
    #[must_use]
    pub fn with_delivery(mut self, delivery: DeliveryService) -> Self {
        self.delivery = Some(delivery);
        self
    }

    fn delivery(&self, tool: &str) -> Result<&DeliveryService, ToolFailure> {
        self.delivery
            .as_ref()
            .ok_or_else(|| ToolFailure::not_available(tool))
    }

    /// The host with `settle_task`.
    #[must_use]
    pub fn with_settling(mut self, settling: SettlingService) -> Self {
        self.settling = Some(settling);
        self
    }

    fn settling(&self, tool: &str) -> Result<&SettlingService, ToolFailure> {
        self.settling
            .as_ref()
            .ok_or_else(|| ToolFailure::not_available(tool))
    }

    /// The same host, with the task service behind the task and crewmate tools.
    #[must_use]
    pub fn with_tasks(mut self, tasks: TaskService) -> Self {
        self.tasks = Some(tasks);
        self
    }

    fn tasks(&self, service: &str) -> Result<&TaskService, ToolFailure> {
        self.tasks
            .as_ref()
            .ok_or_else(|| ToolFailure::not_available(service))
    }

    /// The host with the project registry's tools.
    #[must_use]
    pub fn with_projects(mut self, projects: ProjectService) -> Self {
        self.projects = Some(projects);
        self
    }

    /// The host with the secondmates' tools.
    #[must_use]
    pub fn with_secondmates(mut self, secondmates: SecondmateService) -> Self {
        self.secondmates = Some(secondmates);
        self
    }

    fn secondmates(&self, tool: &str) -> Result<&SecondmateService, ToolFailure> {
        self.secondmates
            .as_ref()
            .ok_or_else(|| ToolFailure::not_available(tool))
    }

    /// The host with `parent_report`.
    #[must_use]
    pub fn with_parent_channel(mut self, parent_channel: ParentChannel) -> Self {
        self.parent_channel = Some(parent_channel);
        self
    }

    fn projects(&self, tool: &str) -> Result<&ProjectService, ToolFailure> {
        self.projects
            .as_ref()
            .ok_or_else(|| ToolFailure::not_available(tool))
    }
}

fn failed(e: &StoreError) -> ToolFailure {
    ToolFailure::Failed(e.to_string())
}

/// Defence in depth for the methods only some roles may reach: the dispatcher offers each role only
/// the tools the registry gives it, and the host refuses a caller the registry doesn't give `tool`,
/// should a call ever reach it another way. The registry stays the one list of who may call what.
fn offered_to(caller: &Caller, tool: &str) -> Result<(), ToolFailure> {
    if qm_tools::registry::find(caller.role, caller.plan, tool).is_some() {
        Ok(())
    } else {
        Err(Refusal::new(format!("{tool} isn't a tool a {} has", caller.role)).into())
    }
}

fn needs_turn(what: &str) -> ToolFailure {
    Refusal::new(format!(
        "{what} needs a turn started by the user's own message"
    ))
    .into()
}

impl ToolHost for CoreToolHost {
    fn show_message<'a>(
        &'a self,
        caller: &'a Caller,
        message: NewMessage,
    ) -> BoxFuture<'a, Result<MessageId, ToolFailure>> {
        Box::pin(async move {
            // On a turn no message of yours started, nobody is in a secondmate's session to answer,
            // so its questions go up the parent channel instead.
            if caller.role == Role::Secondmate
                && message.kind == MessageKind::Question
                && !caller.turn.is_some_and(|t| t.started_by.is_user())
            {
                return Err(Refusal::new(
                    "you're working on a routed request; ask the Quartermaster with \
                     parent_report's needs_user. ask_user reaches the user only when they speak \
                     to you directly",
                )
                .into());
            }
            let agent = caller.agent;
            let turn = caller.turn.map(|t| t.turn);
            let replies_to = caller.turn.and_then(|t| t.started_by.message());
            let at = self.clock.now();
            let kind = message.kind;
            // Checked and stored in one call, so two identical calls can't both be shown.
            let id = self
                .db
                .call(move |conn| {
                    if let Some(turn) = turn
                        && messages::shown_on_turn(conn, agent, turn, message.kind, &message.text)?
                    {
                        return Ok(None);
                    }
                    messages::insert(
                        conn,
                        &MessageRow {
                            agent,
                            source: MessageSource::Agent,
                            kind: message.kind,
                            text: &message.text,
                            options: &message.options,
                            replies_to,
                            turn,
                            at,
                        },
                    )
                    .map(Some)
                })
                .await
                .map_err(|e| failed(&e))?;
            // Some models send the same reply twice in a turn: the user sees it once and the model
            // is told so in band. A different second reply is shown as usual.
            let Some(id) = id else {
                let what = if kind == MessageKind::Question {
                    "question"
                } else {
                    "reply"
                };
                return Err(Refusal::new(format!(
                    "this exact {what} is already shown to the user in this turn, so it wasn't \
                     shown again; don't send it again"
                ))
                .into());
            };
            self.conversation.changed();
            Ok(id)
        })
    }

    fn acknowledge<'a>(
        &'a self,
        caller: &'a Caller,
        your_message: MessageId,
        text: &'a str,
    ) -> BoxFuture<'a, Result<MessageId, ToolFailure>> {
        Box::pin(async move {
            let agent = caller.agent;
            let turn = caller.turn.map(|t| t.turn);
            let text = text.to_owned();
            let at = self.clock.now();
            let id = self
                .db
                .call(move |conn| {
                    messages::insert(
                        conn,
                        &MessageRow {
                            agent,
                            source: MessageSource::Agent,
                            kind: MessageKind::Acknowledgement,
                            text: &text,
                            options: &[],
                            replies_to: Some(your_message),
                            turn,
                            at,
                        },
                    )
                })
                .await
                .map_err(|e| failed(&e))?;
            self.conversation.changed();
            Ok(id)
        })
    }

    fn save_instruction<'a>(
        &'a self,
        caller: &'a Caller,
        your_message: MessageId,
        text: &'a str,
    ) -> BoxFuture<'a, Result<StandingInstructionId, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "save_instruction")?;
            let Some(turn) = caller.turn.map(|t| t.turn) else {
                return Err(needs_turn("saving a standing instruction"));
            };
            let agent = caller.agent;
            let text = text.to_owned();
            let at = self.clock.now();
            self.db
                .call(move |conn| {
                    let tx = conn.transaction()?;
                    let id = standing_instructions::save(&tx, &text, your_message, turn, at)?;
                    events::insert(
                        &tx,
                        &NewEvent {
                            kind: INSTRUCTION_SAVED,
                            agent: Some(agent),
                            task: None,
                            turn: Some(turn),
                            summary: &format!("Standing instruction {id} saved"),
                            payload: serde_json::json!({"instruction": id, "text": text}),
                            at,
                        },
                    )?;
                    tx.commit()?;
                    Ok(id)
                })
                .await
                .map_err(|e| failed(&e))
        })
    }

    fn drop_instruction<'a>(
        &'a self,
        caller: &'a Caller,
        instruction: StandingInstructionId,
    ) -> BoxFuture<'a, Result<(), ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "drop_instruction")?;
            let Some(turn) = caller.turn.map(|t| t.turn) else {
                return Err(needs_turn("dropping a standing instruction"));
            };
            let agent = caller.agent;
            let at = self.clock.now();
            let outcome = self
                .db
                .call(move |conn| {
                    let tx = conn.transaction()?;
                    let Some(row) = standing_instructions::get(&tx, instruction)? else {
                        return Ok(Err(format!("{instruction} doesn't exist")));
                    };
                    if row.dropped_at.is_some() {
                        return Ok(Err(format!("{instruction} was already dropped")));
                    }
                    standing_instructions::drop(&tx, instruction, turn, at)?;
                    events::insert(
                        &tx,
                        &NewEvent {
                            kind: INSTRUCTION_DROPPED,
                            agent: Some(agent),
                            task: None,
                            turn: Some(turn),
                            summary: &format!("Standing instruction {instruction} dropped"),
                            payload: serde_json::json!({"instruction": instruction}),
                            at,
                        },
                    )?;
                    tx.commit()?;
                    Ok(Ok(()))
                })
                .await
                .map_err(|e| failed(&e))?;
            outcome.map_err(|reason| Refusal::new(reason).into())
        })
    }

    fn start_crewmate<'a>(
        &'a self,
        caller: &'a Caller,
        request: StartRequest,
    ) -> BoxFuture<'a, Result<TaskStarted, ToolFailure>> {
        Box::pin(async move {
            self.tasks("start_crewmate")?
                .start_crewmate(caller, request)
                .await
        })
    }

    fn schedule_crewmate<'a>(
        &'a self,
        caller: &'a Caller,
        request: StartRequest,
        trigger: qm_tools::defs::StartTrigger,
    ) -> BoxFuture<'a, Result<qm_tools::defs::DeferredStart, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "schedule_crewmate")?;
            self.tasks("schedule_crewmate")?
                .schedule_crewmate(caller, request, trigger)
                .await
        })
    }

    fn cancel_crewmate_start<'a>(
        &'a self,
        caller: &'a Caller,
        schedule: i64,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "cancel_crewmate_start")?;
            self.tasks("cancel_crewmate_start")?
                .cancel_crewmate_start(caller, schedule)
                .await
        })
    }

    fn list_crewmate_starts<'a>(
        &'a self,
        caller: &'a Caller,
    ) -> BoxFuture<'a, Result<Vec<qm_tools::defs::DeferredStart>, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "list_crewmate_starts")?;
            self.tasks("list_crewmate_starts")?
                .list_crewmate_starts(caller)
                .await
        })
    }

    fn find_project_folder<'a>(
        &'a self,
        caller: &'a Caller,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "find_project_folder")?;
            self.projects("find_project_folder")?
                .find_folder(name)
                .await
        })
    }

    fn message_task<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
        text: String,
    ) -> BoxFuture<'a, Result<TaskUpdated, ToolFailure>> {
        Box::pin(async move {
            self.tasks("message_task")?
                .message_task(caller, task, text)
                .await
        })
    }

    fn stop_task<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
        reason: Option<String>,
    ) -> BoxFuture<'a, Result<TaskUpdated, ToolFailure>> {
        Box::pin(async move {
            self.tasks("stop_task")?
                .stop_task(caller, task, reason)
                .await
        })
    }

    fn set_task_project<'a>(
        &'a self,
        caller: &'a Caller,
        args: qm_tools::defs::SetTaskProjectArgs,
    ) -> BoxFuture<'a, Result<qm_tools::defs::CrewmateMoved, ToolFailure>> {
        Box::pin(async move {
            self.tasks("set_task_project")?
                .set_task_project(caller, args)
                .await
        })
    }

    fn set_task_label<'a>(
        &'a self,
        caller: &'a Caller,
        crewmate: TaskId,
        task: Option<TaskLabelId>,
    ) -> BoxFuture<'a, Result<TaskChanged, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "set_task_label")?;
            self.tasks("set_task_label")?
                .set_task_label(caller, crewmate, task)
                .await
        })
    }

    fn resolve_task_label<'a>(
        &'a self,
        _caller: &'a Caller,
        task: TaskLabelId,
    ) -> BoxFuture<'a, Result<TaskId, ToolFailure>> {
        Box::pin(async move {
            match &self.tasks {
                Some(tasks) => tasks.resolve_task_label(task).await,
                None => Ok(task.legacy_crewmate()),
            }
        })
    }

    fn close_task<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
    ) -> BoxFuture<'a, Result<TaskUpdated, ToolFailure>> {
        Box::pin(async move { self.tasks("close_task")?.close_task(caller, task).await })
    }

    fn link_tasks<'a>(
        &'a self,
        caller: &'a Caller,
        blocker: TaskId,
        blocked: TaskId,
    ) -> BoxFuture<'a, Result<Linked, ToolFailure>> {
        Box::pin(async move {
            self.tasks("link_tasks")?
                .link_tasks(caller, blocker, blocked)
                .await
        })
    }

    fn unlink_tasks<'a>(
        &'a self,
        caller: &'a Caller,
        blocker: TaskId,
        blocked: TaskId,
    ) -> BoxFuture<'a, Result<Unlinked, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "unlink_tasks")?;
            self.tasks("unlink_tasks")?
                .unlink_tasks(caller, blocker, blocked)
                .await
        })
    }

    fn task_detail<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
    ) -> BoxFuture<'a, Result<TaskDetail, ToolFailure>> {
        Box::pin(async move { self.tasks("task_status")?.task_detail(caller, task).await })
    }

    fn find_tasks<'a>(
        &'a self,
        caller: &'a Caller,
        query: FindTasksArgs,
    ) -> BoxFuture<'a, Result<FoundTasks, ToolFailure>> {
        Box::pin(async move { self.tasks("find_tasks")?.find_tasks(caller, query).await })
    }

    fn record_report<'a>(
        &'a self,
        caller: &'a Caller,
        report: ReportArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move { self.tasks("report")?.record_report(caller, report).await })
    }

    fn plan_check<'a>(&'a self, caller: &'a Caller) -> BoxFuture<'a, Result<String, ToolFailure>> {
        Box::pin(async move {
            let svc = self.tasks("plan_check")?;
            crate::tasks::plan::plan_check(svc, caller).await
        })
    }

    fn approve_plan<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
        take_recommended: bool,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "approve_plan")?;
            let svc = self.tasks("approve_plan")?;
            crate::tasks::plan::approve_plan(svc, caller, task, take_recommended).await
        })
    }

    fn approve_review<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "approve_review")?;
            let svc = self.tasks("approve_review")?;
            crate::tasks::review::approve_review(svc, caller, task).await
        })
    }

    fn ask_decision<'a>(
        &'a self,
        caller: &'a Caller,
        question: String,
        options: Vec<String>,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            self.tasks("ask_decision")?
                .ask_decision(caller, question, options)
                .await
        })
    }

    fn declare_wait<'a>(
        &'a self,
        caller: &'a Caller,
        wait: DeclaredWait,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move { self.tasks("declare_wait")?.declare_wait(caller, wait).await })
    }

    fn file_followup<'a>(
        &'a self,
        caller: &'a Caller,
        followup: FileFollowupArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            self.tasks("file_followup")?
                .file_followup(caller, followup)
                .await
        })
    }

    fn delivery_check<'a>(
        &'a self,
        caller: &'a Caller,
    ) -> BoxFuture<'a, Result<DeliveryCheck, ToolFailure>> {
        Box::pin(async move { self.tasks("delivery_check")?.delivery_check(caller).await })
    }

    fn propose_secondmate<'a>(
        &'a self,
        caller: &'a Caller,
        proposal: SecondmateProposal,
    ) -> BoxFuture<'a, Result<Held, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "start_secondmate")?;
            let held = self
                .secondmates("start_secondmate")?
                .propose(caller, proposal)
                .await?;
            self.conversation.changed();
            Ok(held)
        })
    }

    fn route_to_secondmate<'a>(
        &'a self,
        caller: &'a Caller,
        request: RouteRequest,
    ) -> BoxFuture<'a, Result<Routed, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "route_to_secondmate")?;
            self.secondmates("route_to_secondmate")?
                .route(caller, request)
                .await
        })
    }

    fn configure_secondmate<'a>(
        &'a self,
        caller: &'a Caller,
        change: CharterChange,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "configure_secondmate")?;
            self.secondmates("configure_secondmate")?
                .configure(caller, change)
                .await
        })
    }

    fn close_secondmate<'a>(
        &'a self,
        caller: &'a Caller,
        secondmate: SecondmateId,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "close_secondmate")?;
            self.secondmates("close_secondmate")?
                .close(caller, secondmate)
                .await
        })
    }

    fn parent_report<'a>(
        &'a self,
        caller: &'a Caller,
        judgment: ParentJudgment,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            self.parent_channel
                .as_ref()
                .ok_or_else(|| ToolFailure::not_available("parent_report"))?
                .report(caller, judgment)
                .await
        })
    }
    fn clone_repository<'a>(
        &'a self,
        caller: &'a Caller,
        url: &'a str,
        location: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "clone_repository")?;
            self.projects("clone_repository")?
                .clone_repository(url, location)
                .await
        })
    }

    fn configure_project<'a>(
        &'a self,
        caller: &'a Caller,
        change: &'a ConfigureProjectArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "configure_project")?;
            let ack = self
                .projects("configure_project")?
                .configure(caller, change)
                .await?;
            self.conversation.changed();
            Ok(ack)
        })
    }

    fn open_pull_request<'a>(
        &'a self,
        caller: &'a Caller,
        title: String,
        body: String,
    ) -> BoxFuture<'a, Result<PrOpened, ToolFailure>> {
        Box::pin(async move {
            self.delivery("pr_open")?
                .open_pull_request(caller, title, body)
                .await
        })
    }

    fn pull_request_status<'a>(
        &'a self,
        caller: &'a Caller,
    ) -> BoxFuture<'a, Result<PrStatus, ToolFailure>> {
        Box::pin(async move {
            self.delivery("pr_status")?
                .pull_request_status(caller)
                .await
        })
    }

    fn ci_results<'a>(
        &'a self,
        caller: &'a Caller,
        check: Option<String>,
    ) -> BoxFuture<'a, Result<CiReport, ToolFailure>> {
        Box::pin(async move { self.delivery("ci_results")?.ci_results(caller, check).await })
    }

    fn github_read<'a>(
        &'a self,
        caller: &'a Caller,
        args: GithubReadArgs,
    ) -> BoxFuture<'a, Result<GithubContent, ToolFailure>> {
        Box::pin(async move {
            self.delivery("github_read")?
                .github_read(caller, args)
                .await
        })
    }

    fn github_write<'a>(
        &'a self,
        caller: &'a Caller,
        args: GithubWriteArgs,
    ) -> BoxFuture<'a, Result<GithubWriteOutcome, ToolFailure>> {
        Box::pin(async move {
            let outcome = self
                .delivery("github_write")?
                .github_write(caller, args)
                .await?;
            self.conversation.changed();
            Ok(outcome)
        })
    }

    fn propose_release_tag<'a>(
        &'a self,
        caller: &'a Caller,
        args: ProposeReleaseTagArgs,
    ) -> BoxFuture<'a, Result<Held, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "propose_release_tag")?;
            let held = self
                .delivery("propose_release_tag")?
                .propose_release_tag(caller, args)
                .await?;
            self.conversation.changed();
            Ok(held)
        })
    }

    fn release_status<'a>(
        &'a self,
        caller: &'a Caller,
        args: ReleaseStatusArgs,
    ) -> BoxFuture<'a, Result<ReleaseStatus, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "release_status")?;
            let status = self
                .delivery("release_status")?
                .release_status(caller, args.task, args.tag)
                .await?;
            self.conversation.changed();
            Ok(status)
        })
    }

    fn settle_task<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
    ) -> BoxFuture<'a, Result<SettleOutcome, ToolFailure>> {
        Box::pin(async move {
            let outcome = self
                .settling("settle_task")?
                .settle_task(caller, task)
                .await?;
            self.conversation.changed();
            Ok(outcome)
        })
    }

    fn declare_diagnosis<'a>(
        &'a self,
        caller: &'a Caller,
        args: qm_tools::defs::CiDiagnosisArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            self.verification("ci_diagnosis")?
                .declare_diagnosis(caller, args)
                .await
        })
    }

    fn rerun_check<'a>(
        &'a self,
        caller: &'a Caller,
        check: String,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            self.verification("ci_rerun")?
                .rerun_check(caller, check)
                .await
        })
    }

    fn present_hold<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
        summary: String,
    ) -> BoxFuture<'a, Result<Held, ToolFailure>> {
        Box::pin(async move {
            self.decisions("present_hold")?
                .present_hold(caller, task, summary)
                .await
        })
    }

    fn set_verification<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
        level: qm_types::domain::VerificationLevel,
    ) -> BoxFuture<'a, Result<qm_tools::defs::VerificationChanged, ToolFailure>> {
        Box::pin(async move {
            self.verification("set_verification")?
                .set_verification(caller, task, level)
                .await
        })
    }

    fn add_schedule<'a>(
        &'a self,
        caller: &'a Caller,
        due: jiff::Timestamp,
        what: String,
    ) -> BoxFuture<'a, Result<qm_tools::defs::Scheduled, ToolFailure>> {
        Box::pin(async move { self.schedules("schedule")?.add(caller, due, what).await })
    }

    fn find_user_requests<'a>(
        &'a self,
        caller: &'a Caller,
        args: qm_tools::defs::FindUserRequestsArgs,
    ) -> BoxFuture<'a, Result<qm_tools::defs::FoundUserRequests, ToolFailure>> {
        Box::pin(async move {
            offered_to(caller, "find_user_requests")?;
            let agent = caller.agent;
            let sources = self
                .db
                .call(move |conn| {
                    qm_store::user_message_sources::search(
                        conn,
                        agent,
                        args.query.as_deref(),
                        args.before,
                    )
                })
                .await
                .map_err(|e| failed(&e))?;
            Ok(qm_tools::defs::FoundUserRequests {
                requests: sources
                    .into_iter()
                    .map(|source| qm_tools::defs::UserRequest {
                        message: source.id.get(),
                        text: source.text,
                    })
                    .collect(),
            })
        })
    }

    fn cancel_schedule<'a>(
        &'a self,
        caller: &'a Caller,
        schedule: qm_types::ids::ScheduleId,
    ) -> BoxFuture<'a, Result<(), ToolFailure>> {
        Box::pin(async move {
            self.schedules("cancel_schedule")?
                .cancel(caller, schedule)
                .await
        })
    }
}

#[cfg(test)]
#[path = "tool_host_user_requests_tests.rs"]
mod user_requests_tests;
