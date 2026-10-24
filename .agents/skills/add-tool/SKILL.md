---
name: add-tool
description: Add or change a tool an agent can call (Quartermaster, secondmate or crewmate tool, or a provider-engine file tool) - its argument types, description, registry entry, handler, ToolHost method, prompt mention, and how it reaches each engine. Use when adding a tool, changing a tool's arguments or roles, or wiring a tool to a new core service.
---

# Adding a tool

Every tool is defined once in `qm-tools` and runs the same on both engines: a call from `claude`
(through our MCP server) or from the provider engine comes back to the core, which dispatches it
to the handler. Rules are enforced in the handler or the core, never in a prompt.

## 1. The type: `crates/qm-tools/src/defs/<group>.rs`

Use the `tool!` macro (`defs/mod.rs`):

```rust
tool! {
    /// What the tool does (for Rust readers).
    FooTool { name: "foo", class: Act, roles: [Quartermaster, Secondmate], args: FooArgs, output: Ack }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FooArgs {
    // Comment fields with `//`, never `///`: a doc comment becomes a nested schema description.
    pub task: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}
```

- `class`: `Speak` (reply-like; the only class allowed in the action rule's retry), `Act`
  (counts as an effective action), `Read` (never counts).
- `deny_unknown_fields` is required: a field the model may not set comes back as a refusal naming
  it. Use IDs from `qm_types::ids` (they serialise as `C-0042`, `T-0017`, `S-02`). An argument
  typed `TaskId` (a crewmate) also takes a task holding one crewmate (`qm-tools` `task_refs.rs`).
- A second variant of a name for another role (like `start_crewmate.secondmate`) implements
  `Tool` by hand with its own `KEY` (see `defs/crew.rs`).
- Provider-engine-only tools (files, shell, `agent`) use `EngineScope::ProviderOnly`
  (`defs/files.rs`); the engine runs them itself. Add the name to `PROVIDER_FILE_TOOLS` in
  `crates/qm-engine/src/scripted/session.rs` too.

## 2. The description: `crates/qm-tools/descriptions/<key>.md`

It is the only guidance the model gets: what the tool is for, when to call it, every field, and
what a refusal means. Keep it to 1,900 characters at most, and put no `description` keyword below
the schema root (that is why argument fields use `//` comments): both engines then see the same
guidance, all in one place.

## 3. Register it: `crates/qm-tools/src/registry.rs`

Append `ToolDef::of::<defs::FooTool>()` to `TOOLS`; never reorder existing entries. `for_role` decides who is offered
what; `descriptors(role)` is what goes into a session's `SessionSpec.tools`.

## 4. The handler: `crates/qm-tools/src/handlers/<group>.rs`

```rust
impl Handle for FooTool {
    fn handle<'a>(cx: &'a ToolCx<'a>, args: FooArgs) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move { cx.host.foo(cx.caller, args.task).await })
    }
}
```

Bind it in `HANDLERS` (`handlers/mod.rs`): `bind::<crate::defs::FooTool>()`. A registered tool
without a binding answers "isn't available in this build yet".

- Check what you can without the core (lengths, times on `cx.clock`), and refuse with
  `Refusal::new("one plain sentence")`: say what to do instead.
- Overrides call `qm_policy::overrides::check_override` first and need a user-started turn.
  Model/effort/premium use `check_choice_override` instead: an earlier `choice_message` is allowed,
  but the core must verify its actual user authorship and conversation (or checked delegation)
  through `qm_store::user_message_sources`, never trust the tool's citation by itself. This is an
  actual user-message numeric ID, not a turn/task/correlation ID. The prompt must still require
  matching the user's requested fields and scope; source/ownership checks do not parse that intent.
- Deferred starts (`schedule_crewmate`, role-specific secondmate variant, `cancel_crewmate_start`,
  `list_crewmate_starts`) live in `defs/crew.rs` and `handlers/deferred.rs`. Reuse nested normal
  start args and `prepare_start` / `prepare_secondmate_start`; don't copy bulky schemas or bypass
  shape, authority or correlation rules. `StartRequest` is serialized for persistence. No child
  task, worker or worktree exists until its `StartTrigger` fires; `schedule` is only a wake.

## 5. The core service

- Add a method to `ToolHost` (`crates/qm-tools/src/handler.rs`) **with a default body** returning
  `Err(ToolFailure::not_available("foo"))`; `qm-tools` never depends on `qm-core`.
- Implement it on `CoreToolHost` (`crates/qm-core/src/tool_host.rs`). For a role-limited tool
  call `offered_to(caller, "foo")?` first. Reach services through the host's fields; signal the
  conversation after any change the user can see so the UI refreshes.
- Crewmates have helper agents on both engines, and a helper may call the crewmate's tools. While
  any helper runs, every crewmate tool that isn't `Read` class is refused in-band ("wait for your
  helpers to finish, then call … yourself"): `dispatch` in `crates/qm-core/src/tasks/runner.rs`
  does it through `crate::jobs::helpers_running`. So give a crewmate tool `Read` only if it
  truly changes nothing.
- Secondmates act only within their scope and chain of command; follow the existing secondmate
  handlers (`handlers/secondmates.rs`, `qm-core` `secondmates/`).

## 6. Engines and prompts

- Claude: our tools reach `claude` as `mcp__quartermaster__<name>`, allowed by
  `--allowedTools=mcp__quartermaster__*`, and the init check expects exactly the session's
  descriptors, so a new tool needs no engine change. Claude's *built-in* tools are a different
  list: skill `claude-engine`.
- Provider engine: `SessionSpec.tools` carries it; nothing else to do.
- If the model needs to know when to reach for it, add a line to the role's prompt in `prompts/`
  (skill `prompts`).

## Verify

`cargo xtask check` and `cargo build`, then call the tool in a real run (skill `run-by-hand`;
the scripted engine can make the call without a model).
