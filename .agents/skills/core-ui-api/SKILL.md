---
name: core-ui-api
description: Add a command the UI sends to the core, or data the core shows in the UI - the qm_types::api Command, Snapshot and view types, an ApiPart in qm-core registered in the binary's wiring, and the TUI side that sends and draws it. Use when a screen needs new data or a new action, or when adding a screen backed by core state.
---

# The core ⇄ UI API

`qm-tui` depends on `qm-types` only. It sends `qm_types::api::Command`s and renders
`Update::Snapshot(Box<Snapshot>)` (plus `ConfirmQuit` and `Closing`), over the two unbounded
channels from `qm_types::api::link()`. Everything is serde data, so the link could become a
socket later. The core never blocks on the UI; the UI drains with `try_recv` in its render loop.

## 1. The types: `crates/qm-types/src/api/mod.rs`

- Add a `Command` variant, `Snapshot` field or view type. **Additive only:** enums and view
  structs are `#[non_exhaustive]`; new fields are `#[serde(default)]`; other crates build views
  with constructors (`ChatLine::new(…)`, `CrewRow::new(…)`, `Snapshot::default()`) and then set
  fields, never with struct literals. Never rename, remove or change the meaning of a field.
- Data is display-ready: the core decides the words and tones (`Tone`, `Styled`), the UI lays them
  out. The UI doesn't compute rules.

## 2. The core: an `ApiPart` (`crates/qm-core/src/api.rs`)

```rust
pub trait ApiPart: Send + Sync + fmt::Debug {
    fn handle<'a>(&'a self, command: &'a Command) -> Option<BoxFuture<'a, Handled>>;
    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()>;
}
```

`qm_core::api::serve` handles the seat's own commands, then asks each part in `ApiDeps.parts`;
the first `Some` wins. `handle` returns `Handled::Changed` (send a fresh snapshot), `Unchanged`,
`Quit` or `ConfirmQuit { working }`. `fill` adds the part's facts to each snapshot. Put a feature's
part in its own module (`SettingsPart`, `TasksPart`, `SessionsPart`, `NotificationsPart` are
examples) and register it in `start_core` in `crates/quartermaster/src/wiring.rs`; order matters only where a
part reads what earlier parts filled (`NotificationsPart` is last). A change the UI should see
from a background task signals the conversation (`ChatLog::changed()`), which triggers a snapshot.
A refusal the user should see goes in the chat as an app note.

## 3. The UI: `crates/qm-tui/src/`

- Send with `App::command(Command::…)` (`app.rs`), or `intend` on the Settings screens.
- `App::on_update` stores the snapshot; drawing reads it. UI-only state (focus, selection, scroll,
  overlays) lives in `view.rs`, written only by input code (`input/`), read by drawing.
- A new full screen: a `Screen` variant in `router.rs`, its label (and its place in
  `Screen::TABS` if it is a top-bar tab) and an arm in `draw`. Follow skill `tui-conventions`.
- For layout work without a running core, `crates/qm-tui/src/sample.rs` holds fixed snapshots
  shown with `QM_DEV_SAMPLE` (skill `run-by-hand`); add a sample for a new screen.

## Verify

`cargo xtask check`, `cargo build`, then drive the new command in a throwaway run and watch the
screen update (skill `run-by-hand`).
