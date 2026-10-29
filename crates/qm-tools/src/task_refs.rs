//! A task given where a crewmate is asked for.
//!
//! Arguments that name a crewmate take its name, `C-0042`. Agents also write a task, `T-0017`:
//! out of habit from when every task was one crewmate, or because the task is what they know. The
//! [`crate::handler::Dispatcher`] finds each such argument from the tool's schema (a string whose
//! pattern is a crewmate's) and asks the host which crewmate the task means, before the arguments
//! are parsed. An argument that names a task (`join_task`, `set_task_label`'s `task`) has a task's
//! pattern and is never touched, and neither is free text that happens to hold a `T-`.

use crate::handler::{Caller, ToolFailure, ToolHost};
use qm_types::ids::TaskLabelId;
use serde_json::Value;

/// The schema pattern of an argument that names a crewmate.
const CREWMATE_PATTERN: &str = "^[CT]-[0-9]+$";

/// Replaces each task given for a crewmate in `args` with the crewmate the host says it means.
/// Fails with the host's refusal when a task holds more than one crewmate.
pub(crate) async fn resolve(
    host: &dyn ToolHost,
    caller: &Caller,
    schema: impl FnOnce() -> Value,
    args: &mut Value,
) -> Result<bool, ToolFailure> {
    if !holds_task_string(args) {
        return Ok(false);
    }
    let mut found = Vec::new();
    find(&schema(), args, String::new(), &mut found);
    let changed = !found.is_empty();
    for (pointer, task) in found {
        let crewmate = host.resolve_task_label(caller, task).await?;
        if let Some(slot) = args.pointer_mut(&pointer) {
            *slot = Value::String(crewmate.to_string());
        }
    }
    Ok(changed)
}

/// Whether any string in `value` looks like a task: most calls have none, and skip the schema.
fn holds_task_string(value: &Value) -> bool {
    match value {
        Value::String(s) => s.starts_with("T-"),
        Value::Array(items) => items.iter().any(holds_task_string),
        Value::Object(map) => map.values().any(holds_task_string),
        _ => false,
    }
}

/// The JSON pointer of every string in `value` that `schema` says names a crewmate and that
/// parses as a task.
fn find(schema: &Value, value: &Value, at: String, out: &mut Vec<(String, TaskLabelId)>) {
    match value {
        Value::String(s) => {
            if names_crewmate(schema)
                && let Ok(task) = s.parse::<TaskLabelId>()
                && !out.iter().any(|(p, _)| *p == at)
            {
                out.push((at, task));
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                for sub in branches(schema).filter_map(|b| b.get("properties")?.get(key)) {
                    find(sub, item, format!("{at}/{}", escape(key)), out);
                }
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                for sub in branches(schema).filter_map(|b| b.get("items")) {
                    find(sub, item, format!("{at}/{i}"), out);
                }
            }
        }
        _ => {}
    }
}

/// Whether `schema`, or one of its alternatives, is a crewmate's name.
fn names_crewmate(schema: &Value) -> bool {
    branches(schema).any(|b| b.get("pattern").and_then(Value::as_str) == Some(CREWMATE_PATTERN))
}

/// `schema` and its alternatives (`anyOf`, `oneOf`, `allOf`), one level deep and recursively.
fn branches(schema: &Value) -> impl Iterator<Item = &Value> {
    let mut all = vec![schema];
    let mut i = 0;
    while let Some(s) = all.get(i).copied() {
        for key in ["anyOf", "oneOf", "allOf"] {
            if let Some(Value::Array(alts)) = s.get(key) {
                all.extend(alts);
            }
        }
        i += 1;
    }
    all.into_iter()
}

/// A JSON pointer token for `key`.
fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}
