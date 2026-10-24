//! A task given where a crewmate is asked for reaches the handler as the task's crewmate.

use qm_tools::defs::{TaskChanged, Unlinked};
use qm_tools::handler::{Caller, Dispatched, Dispatcher, ToolCx, ToolFailure, ToolHost, TurnCx};
use qm_types::BoxFuture;
use qm_types::clock::SystemClock;
use qm_types::domain::{Role, StartedBy};
use qm_types::error::Refusal;
use qm_types::ids::{AgentId, TaskId, TaskLabelId, TurnId};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

/// T-0017 holds only C-0042; T-0018 holds two crewmates.
#[derive(Debug, Default)]
struct Host {
    resolved: Mutex<Vec<TaskLabelId>>,
    unlinked: Mutex<Vec<(TaskId, TaskId)>>,
    relabelled: Mutex<Vec<(TaskId, Option<TaskLabelId>)>>,
}

fn crewmate(n: u32) -> TaskId {
    TaskId::new(n).unwrap()
}

impl ToolHost for Host {
    fn resolve_task_label<'a>(
        &'a self,
        _caller: &'a Caller,
        task: TaskLabelId,
    ) -> BoxFuture<'a, Result<TaskId, ToolFailure>> {
        Box::pin(async move {
            self.resolved.lock().unwrap().push(task);
            match task.get() {
                17 => Ok(crewmate(42)),
                _ => Err(Refusal::new(format!(
                    "{task} holds C-0001, C-0002; name the crewmate you mean"
                ))
                .into()),
            }
        })
    }

    fn unlink_tasks<'a>(
        &'a self,
        _caller: &'a Caller,
        blocker: TaskId,
        blocked: TaskId,
    ) -> BoxFuture<'a, Result<Unlinked, ToolFailure>> {
        Box::pin(async move {
            self.unlinked.lock().unwrap().push((blocker, blocked));
            Ok(Unlinked { blocker, blocked })
        })
    }

    fn set_task_label<'a>(
        &'a self,
        _caller: &'a Caller,
        crewmate: TaskId,
        task: Option<TaskLabelId>,
    ) -> BoxFuture<'a, Result<TaskChanged, ToolFailure>> {
        Box::pin(async move {
            self.relabelled.lock().unwrap().push((crewmate, task));
            Ok(TaskChanged {
                crewmate,
                task: task.unwrap_or(crewmate.own_label()),
                was: crewmate.own_label(),
                with: Vec::new(),
            })
        })
    }
}

fn dispatch(host: &dyn ToolHost, tool: &str, args: Value) -> Dispatched {
    let caller = Caller {
        agent: AgentId::new(7),
        role: Role::Quartermaster,
        turn: Some(TurnCx {
            turn: TurnId::new(9),
            started_by: StartedBy::AppEvent,
        }),
        task: None,
        secondmate: None,
        plan: None,
    };
    let cx = ToolCx {
        caller: &caller,
        clock: &SystemClock,
        host,
    };
    let dispatcher = Dispatcher::builtin();
    let mut future = pin!(dispatcher.dispatch(&cx, tool, args));
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("test host must complete immediately"),
    }
}

#[test]
fn a_task_holding_one_crewmate_names_that_crewmate() {
    let host = Host::default();
    let done = dispatch(
        &host,
        "unlink_tasks",
        json!({"blocker": "T-17", "blocked": "C-0043"}),
    );
    assert!(!done.result.is_error, "{}", done.result.text);
    assert_eq!(
        *host.unlinked.lock().unwrap(),
        vec![(crewmate(42), crewmate(43))]
    );
    assert_eq!(
        *host.resolved.lock().unwrap(),
        vec![TaskLabelId::new(17).unwrap()]
    );
    // What ran is handed back, so the app's notes name the crewmate.
    assert_eq!(
        done.resolved,
        Some(json!({"blocker": "C-0042", "blocked": "C-0043"}))
    );
}

#[test]
fn an_ambiguous_task_is_refused_before_the_handler_runs() {
    let host = Host::default();
    let done = dispatch(
        &host,
        "unlink_tasks",
        json!({"blocker": "T-18", "blocked": "C-0043"}),
    );
    assert!(done.result.is_error);
    assert!(!done.counted);
    assert!(
        done.result.text.contains("name the crewmate you mean"),
        "{}",
        done.result.text
    );
    assert!(host.unlinked.lock().unwrap().is_empty());
}

#[test]
fn an_argument_that_names_a_task_is_left_as_a_task() {
    let host = Host::default();
    let done = dispatch(
        &host,
        "set_task_label",
        json!({"crewmate": "T-17", "task": "T-18"}),
    );
    assert!(!done.result.is_error, "{}", done.result.text);
    assert_eq!(
        *host.relabelled.lock().unwrap(),
        vec![(crewmate(42), Some(TaskLabelId::new(18).unwrap()))]
    );
    // Only the crewmate argument was resolved; `task` stayed T-0018.
    assert_eq!(
        *host.resolved.lock().unwrap(),
        vec![TaskLabelId::new(17).unwrap()]
    );
}

#[test]
fn a_call_naming_crewmates_never_asks_the_host() {
    let host = Host::default();
    let done = dispatch(
        &host,
        "unlink_tasks",
        json!({"blocker": "C-0042", "blocked": "C-0043"}),
    );
    assert!(!done.result.is_error, "{}", done.result.text);
    assert!(host.resolved.lock().unwrap().is_empty());
    assert_eq!(done.resolved, None);
}
