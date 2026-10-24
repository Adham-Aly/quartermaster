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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn found(schema: &Value, args: &Value) -> Vec<(String, String)> {
        let mut out = Vec::new();
        find(schema, args, String::new(), &mut out);
        out.into_iter().map(|(p, t)| (p, t.to_string())).collect()
    }

    #[test]
    fn only_arguments_that_name_a_crewmate_are_found() {
        let schema = json!({
            "type": "object",
            "properties": {
                "crewmate": {"type": "string", "pattern": CREWMATE_PATTERN},
                "task": {"type": ["string", "null"], "pattern": "^T-[0-9]+$"},
                "text": {"type": "string"},
                "trigger": {"oneOf": [
                    {"type": "object", "properties": {"task": {"type": "string", "pattern": CREWMATE_PATTERN}}},
                    {"type": "object", "properties": {"due": {"type": "string"}}}
                ]},
                "tasks": {"type": "array", "items": {"type": "string", "pattern": CREWMATE_PATTERN}}
            }
        });
        let args = json!({
            "crewmate": "T-17",
            "task": "T-0018",
            "text": "T-0019 is broken",
            "trigger": {"task": "T-0020"},
            "tasks": ["C-0021", "T-0022"]
        });
        assert_eq!(
            found(&schema, &args),
            vec![
                ("/crewmate".to_owned(), "T-0017".to_owned()),
                ("/tasks/1".to_owned(), "T-0022".to_owned()),
                ("/trigger/task".to_owned(), "T-0020".to_owned()),
            ]
        );
    }

    #[test]
    fn a_call_without_a_task_skips_the_schema() {
        assert!(!holds_task_string(
            &json!({"task": "C-0042", "text": "fine"})
        ));
        assert!(holds_task_string(&json!({"tasks": [{"task": "T-1"}]})));
    }

    #[test]
    fn every_tool_argument_that_takes_a_task_keeps_a_tasks_pattern() {
        for def in crate::registry::TOOLS {
            let schema = def.input_schema();
            if let Some(join) = schema.pointer("/properties/join_task") {
                assert!(!names_crewmate(join), "{}: join_task", def.key);
            }
        }
        let set = crate::registry::TOOLS
            .iter()
            .find(|d| d.name == "set_task_label")
            .expect("set_task_label")
            .input_schema();
        assert!(!names_crewmate(&set["properties"]["task"]));
        assert!(names_crewmate(&set["properties"]["crewmate"]));
    }
}
