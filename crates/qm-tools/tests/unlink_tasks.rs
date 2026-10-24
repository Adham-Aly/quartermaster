use qm_tools::defs::Unlinked;
use qm_tools::handler::{Caller, Dispatched, Dispatcher, ToolCx, ToolFailure, ToolHost, TurnCx};
use qm_tools::{EngineScope, ToolClass, registry};
use qm_types::BoxFuture;
use qm_types::clock::SystemClock;
use qm_types::domain::{EngineKind, PlanMode, Role, StartedBy};
use qm_types::error::Refusal;
use qm_types::ids::{AgentId, SecondmateId, TaskId, TurnId};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

#[derive(Debug, Default)]
struct RecordingHost {
    calls: Mutex<Vec<(Caller, TaskId, TaskId)>>,
    failure: Option<ToolFailure>,
}

impl ToolHost for RecordingHost {
    fn unlink_tasks<'a>(
        &'a self,
        caller: &'a Caller,
        blocker: TaskId,
        blocked: TaskId,
    ) -> BoxFuture<'a, Result<Unlinked, ToolFailure>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push((*caller, blocker, blocked));
            if let Some(failure) = &self.failure {
                return Err(failure.clone());
            }
            Ok(Unlinked { blocker, blocked })
        })
    }
}

fn caller(role: Role) -> Caller {
    Caller {
        agent: AgentId::new(7),
        role,
        turn: Some(TurnCx {
            turn: TurnId::new(9),
            started_by: StartedBy::AppEvent,
        }),
        task: (role == Role::Crewmate).then(|| TaskId::new(44).unwrap()),
        secondmate: (role == Role::Secondmate).then(|| SecondmateId::new(3).unwrap()),
        plan: None,
    }
}

fn dispatch(host: &dyn ToolHost, caller: &Caller, args: Value) -> Dispatched {
    let cx = ToolCx {
        caller,
        clock: &SystemClock,
        host,
    };
    let dispatcher = Dispatcher::builtin();
    let mut future = pin!(dispatcher.dispatch(&cx, "unlink_tasks", args));
    // These test hosts never suspend, so no runtime or extra test dependency is needed.
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("test host must complete immediately"),
    }
}

#[test]
fn unlink_tasks_is_an_action_for_supervisors_on_both_engines() {
    for role in [Role::Quartermaster, Role::Secondmate] {
        let def = registry::find(role, None, "unlink_tasks").expect("supervisor tool");
        assert_eq!(def.class, ToolClass::Act);
        assert_eq!(def.scope, EngineScope::Both);
        assert!(registry::descriptors(role, None).contains(&def.descriptor()));
        for engine in [
            EngineKind::Claude,
            EngineKind::Provider,
            EngineKind::Scripted,
        ] {
            assert!(registry::for_role(role, engine, None).any(|d| d.key == "unlink_tasks"));
        }
    }
    for plan in [None, Some(PlanMode::Interactive), Some(PlanMode::Internal)] {
        assert!(registry::find(Role::Crewmate, plan, "unlink_tasks").is_none());
        assert!(
            registry::descriptors(Role::Crewmate, plan)
                .iter()
                .all(|d| d.name != "unlink_tasks")
        );
        for engine in [
            EngineKind::Claude,
            EngineKind::Provider,
            EngineKind::Scripted,
        ] {
            assert!(
                registry::for_role(Role::Crewmate, engine, plan).all(|d| d.key != "unlink_tasks")
            );
        }
    }
}

#[test]
fn unlink_tasks_schema_requires_only_directed_task_ids() {
    let def = registry::find(Role::Quartermaster, None, "unlink_tasks").unwrap();
    assert_eq!(
        def.input_schema(),
        json!({
            "type": "object",
            "properties": {
                "blocker": {"type": "string", "pattern": "^[CT]-[0-9]+$"},
                "blocked": {"type": "string", "pattern": "^[CT]-[0-9]+$"}
            },
            "required": ["blocker", "blocked"],
            "additionalProperties": false
        })
    );
}

#[test]
fn unlink_tasks_dispatch_forwards_caller_and_directed_endpoints() {
    let host = RecordingHost::default();
    for role in [Role::Quartermaster, Role::Secondmate] {
        let caller = caller(role);
        let result = dispatch(
            &host,
            &caller,
            json!({"blocker": "T-42", "blocked": "T-43"}),
        );
        assert!(!result.result.is_error, "{}", result.result.text);
        assert!(result.counted);
        assert_eq!(
            serde_json::from_str::<Unlinked>(&result.result.text).unwrap(),
            Unlinked {
                blocker: TaskId::new(42).unwrap(),
                blocked: TaskId::new(43).unwrap(),
            }
        );
        assert_eq!(
            host.calls.lock().unwrap().last(),
            Some(&(caller, TaskId::new(42).unwrap(), TaskId::new(43).unwrap()))
        );
    }
    assert_eq!(host.calls.lock().unwrap().len(), 2);
}

#[test]
fn unlink_tasks_refuses_bad_arguments_before_calling_host() {
    let host = RecordingHost::default();
    let caller = caller(Role::Quartermaster);
    for (args, reason) in [
        (
            json!({"blocker": "T-42", "blocked": "T-43", "force": true}),
            "unknown field `force`",
        ),
        (json!({"blocker": "T-42"}), "missing field `blocked`"),
        (json!({"blocked": "T-43"}), "missing field `blocker`"),
        (
            json!({"blocker": "SM-02", "blocked": "T-43"}),
            "must start with C-",
        ),
        (
            json!({"blocker": "T-42", "blocked": "T-0"}),
            "IDs start at 1",
        ),
        (
            json!({"blocker": "T-42", "blocked": "T-42"}),
            "two different tasks",
        ),
    ] {
        let result = dispatch(&host, &caller, args);
        assert!(result.result.is_error);
        assert!(!result.counted);
        assert!(result.result.text.starts_with("refused:"));
        assert!(
            result.result.text.contains(reason),
            "{}",
            result.result.text
        );
    }
    assert!(host.calls.lock().unwrap().is_empty());
}

#[test]
fn unlink_tasks_dispatch_refuses_crewmates() {
    let host = RecordingHost::default();
    let result = dispatch(
        &host,
        &caller(Role::Crewmate),
        json!({"blocker": "T-42", "blocked": "T-43"}),
    );
    assert!(result.result.is_error);
    assert!(!result.counted);
    assert!(result.result.text.contains("no tool named unlink_tasks"));
    assert!(host.calls.lock().unwrap().is_empty());
}

#[test]
fn unlink_tasks_dispatch_preserves_host_refusals() {
    let host = RecordingHost {
        failure: Some(Refusal::new("both tasks must be your direct reports").into()),
        ..RecordingHost::default()
    };
    let result = dispatch(
        &host,
        &caller(Role::Secondmate),
        json!({"blocker": "T-42", "blocked": "T-43"}),
    );
    assert!(result.result.is_error);
    assert!(!result.counted);
    assert_eq!(
        result.result.text,
        "refused: both tasks must be your direct reports"
    );
    assert_eq!(host.calls.lock().unwrap().len(), 1);
}

#[test]
fn unlink_tasks_default_host_remains_additive() {
    struct DefaultHost;
    impl ToolHost for DefaultHost {}

    let result = dispatch(
        &DefaultHost,
        &caller(Role::Quartermaster),
        json!({"blocker": "T-42", "blocked": "T-43"}),
    );
    assert!(result.result.is_error);
    assert!(!result.counted);
    assert_eq!(
        result.result.text,
        "error: unlink_tasks isn't available in this build yet"
    );
}
