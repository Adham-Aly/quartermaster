use super::*;
use crate::handler::{Caller, Dispatcher, StartRequest, ToolHost};
use qm_types::clock::SystemClock;
use qm_types::domain::Role;
use qm_types::ids::{AgentId, CorrelationId, SecondmateId, TaskId};
use serde_json::{Value, json};
use std::future::Future;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

#[derive(Default)]
struct Host {
    starts: Mutex<Vec<StartRequest>>,
    scheduled: Mutex<Vec<(StartRequest, StartTrigger)>>,
    cancelled: Mutex<Vec<i64>>,
}

impl ToolHost for Host {
    fn start_crewmate<'a>(
        &'a self,
        _caller: &'a Caller,
        request: StartRequest,
    ) -> BoxFuture<'a, Result<crate::defs::TaskStarted, ToolFailure>> {
        self.starts.lock().unwrap().push(request);
        Box::pin(async { Err(ToolFailure::not_available("test immediate start")) })
    }

    fn schedule_crewmate<'a>(
        &'a self,
        _caller: &'a Caller,
        request: StartRequest,
        trigger: StartTrigger,
    ) -> BoxFuture<'a, Result<DeferredStart, ToolFailure>> {
        self.scheduled
            .lock()
            .unwrap()
            .push((request, trigger.clone()));
        Box::pin(async move {
            Ok(DeferredStart {
                schedule: 42,
                trigger,
                status: "pending".into(),
                task: None,
                note: None,
            })
        })
    }

    fn cancel_crewmate_start<'a>(
        &'a self,
        _caller: &'a Caller,
        schedule: i64,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        self.cancelled.lock().unwrap().push(schedule);
        Box::pin(async {
            Ok(Ack {
                note: "cancelled".into(),
            })
        })
    }

    fn list_crewmate_starts<'a>(
        &'a self,
        _caller: &'a Caller,
    ) -> BoxFuture<'a, Result<Vec<DeferredStart>, ToolFailure>> {
        Box::pin(async { Ok(vec![]) })
    }
}

// These fake host futures are ready immediately; no async runtime or timer is needed.
fn ready<F: Future>(future: F) -> F::Output {
    match std::pin::pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("test host must not wait"),
    }
}

fn caller(role: Role) -> Caller {
    Caller {
        agent: AgentId::new(1),
        role,
        turn: None,
        task: None,
        secondmate: (role == Role::Secondmate).then(|| SecondmateId::new(2).unwrap()),
        plan: None,
    }
}

fn start(role: Role) -> Value {
    let mut args = json!({
        "folder": "  ~/work  ",
        "kind": "research",
        "title": "  docs(test): inspect  ",
        "goal": "inspect",
        "instructions": ["one-time instruction"],
        "effort": "high",
        "premium": true,
        "choice_message": 7
    });
    if role == Role::Secondmate {
        args["correlation"] = json!(CorrelationId::new(193));
        args["base_branch"] = json!("  topic  ");
    }
    args
}

#[test]
fn scheduled_starts_reuse_immediate_conversion_authority_and_correlation() {
    for role in [Role::Quartermaster, Role::Secondmate] {
        let caller = caller(role);
        let host = Host::default();
        let cx = ToolCx {
            caller: &caller,
            clock: &SystemClock,
            host: &host,
        };
        let dispatcher = Dispatcher::builtin();
        ready(dispatcher.dispatch(&cx, "start_crewmate", start(role)));
        for trigger in [
            json!({"when": "commit", "task": TaskId::new(3)}),
            json!({"when": "done", "task": TaskId::new(3)}),
            json!({"when": "landed", "task": TaskId::new(3)}),
            json!({"when": "at", "due": "2099-01-01T00:00:00Z"}),
        ] {
            let out = ready(dispatcher.dispatch(
                &cx,
                "schedule_crewmate",
                json!({
                    "start": start(role), "trigger": trigger
                }),
            ));
            assert!(!out.result.is_error, "{}", out.result.text);
            assert!(out.counted);
            let value: Value = serde_json::from_str(&out.result.text).unwrap();
            assert_eq!(value["schedule"], 42);
            assert_eq!(value["task"], Value::Null, "no placeholder child");
            let scheduled = host.scheduled.lock().unwrap();
            let (request, actual_trigger) = scheduled.last().unwrap();
            assert_eq!(request, &host.starts.lock().unwrap()[0]);
            assert_eq!(serde_json::to_value(actual_trigger).unwrap(), trigger);
            assert_eq!(request.folder.as_deref(), Some("~/work"));
            assert_eq!(request.title, "docs(test): inspect");
            if role == Role::Secondmate {
                assert_eq!(request.base_branch.as_deref(), Some("topic"));
                assert_eq!(request.correlation, CorrelationId::new(193));
            }
            let persisted = serde_json::to_string(request).unwrap();
            assert_eq!(
                serde_json::from_str::<StartRequest>(&persisted).unwrap(),
                *request
            );
        }
        assert_eq!(
            host.starts.lock().unwrap().len(),
            1,
            "scheduling must not call start"
        );
    }
}

#[test]
fn invalid_scheduled_starts_never_reach_host() {
    for role in [Role::Quartermaster, Role::Secondmate] {
        let caller = caller(role);
        let host = Host::default();
        let cx = ToolCx {
            caller: &caller,
            clock: &SystemClock,
            host: &host,
        };
        let valid =
            json!({"start": start(role), "trigger": {"when": "done", "task": TaskId::new(3)}});
        let mut cases = Vec::new();
        let mut bad = valid.clone();
        bad["start"]["goal"] = json!("");
        cases.push(bad);
        let mut bad = valid.clone();
        bad["start"]["title"] = json!("not conventional");
        cases.push(bad);
        let mut bad = valid.clone();
        bad["start"]["in_place"] = json!(true);
        cases.push(bad);
        let mut bad = valid.clone();
        bad["start"]
            .as_object_mut()
            .unwrap()
            .remove("choice_message");
        cases.push(bad);
        let mut bad = valid.clone();
        bad["start"]["unexpected"] = json!(true);
        cases.push(bad);
        let mut bad = valid.clone();
        bad["unexpected"] = json!(true);
        cases.push(bad);
        let mut bad = valid.clone();
        bad["trigger"]["unexpected"] = json!(true);
        cases.push(bad);
        let mut bad = valid.clone();
        bad["trigger"] = json!({"when": "at", "due": "2000-01-01T00:00:00Z"});
        cases.push(bad);
        let mut bad = valid;
        bad["trigger"] = json!({"when": "done"});
        cases.push(bad);
        for args in cases {
            let out = ready(Dispatcher::builtin().dispatch(&cx, "schedule_crewmate", args));
            assert!(out.result.is_error, "{}", out.result.text);
            assert!(!out.counted);
        }
        assert!(host.scheduled.lock().unwrap().is_empty());
        assert!(host.starts.lock().unwrap().is_empty());
    }
}

#[test]
fn cancel_and_list_dispatch_with_numeric_ids_and_correct_counting() {
    for role in [Role::Quartermaster, Role::Secondmate] {
        let caller = caller(role);
        let host = Host::default();
        let cx = ToolCx {
            caller: &caller,
            clock: &SystemClock,
            host: &host,
        };
        let dispatcher = Dispatcher::builtin();
        let out = ready(dispatcher.dispatch(&cx, "cancel_crewmate_start", json!({"schedule": 42})));
        assert!(!out.result.is_error);
        assert!(out.counted);
        assert_eq!(*host.cancelled.lock().unwrap(), [42]);
        let out = ready(dispatcher.dispatch(&cx, "list_crewmate_starts", json!({})));
        assert!(!out.result.is_error);
        assert!(!out.counted);
        assert_eq!(out.result.text, "[]");
        for (tool, args) in [
            ("cancel_crewmate_start", json!({"schedule": "SC-42"})),
            ("list_crewmate_starts", json!({"schedule": 42})),
        ] {
            assert!(ready(dispatcher.dispatch(&cx, tool, args)).result.is_error);
        }
    }
}

#[test]
fn deferred_schema_reuses_start_shapes_without_nested_guidance() {
    use crate::defs::{SecondmateStartCrewmateTool, StartCrewmateTool};
    use crate::spec::input_schema;
    for (scheduled, immediate) in [
        (
            input_schema::<ScheduleCrewmateTool>(),
            input_schema::<StartCrewmateTool>(),
        ),
        (
            input_schema::<SecondmateScheduleCrewmateTool>(),
            input_schema::<SecondmateStartCrewmateTool>(),
        ),
    ] {
        assert_eq!(
            scheduled["properties"]["start"]["properties"],
            immediate["properties"]
        );
        assert_eq!(
            scheduled["properties"]["start"]["required"],
            immediate["required"]
        );
        assert_eq!(scheduled["additionalProperties"], false);
        let text = scheduled.to_string();
        assert!(!text.contains("\"description\""), "{text}");
        assert!(!text.contains("\"$ref\""), "{text}");
        assert_eq!(
            scheduled["properties"]["start"]["properties"]["choice_message"]["type"],
            json!(["integer", "null"])
        );
    }
}
