---
name: add-setting
description: Add or change a user setting stored in config.toml - the schema in qm-types, loading and saving in qm-core config_file, applying it live through the shared config channel, and showing or changing it in Settings. Use when a behaviour should be configurable, or when a setting's default, name or screen changes.
---

# Adding a setting

## Where a setting lives

- **`config.toml`** (`qm_types::config::Config`): the user's preferences that aren't about a
  particular account or detected model: the Quartermaster's model and optional effort override,
  the "always use" pin, tier effort defaults, project search roots, notifications. **Never a
  secret**: credentials go in `auth.json` through `qm_auth`.
  Tier defaults (`[tiers]`, `TierEfforts`) are for model-only pins, explicitly selected
  Quartermaster models without an effort override, and legacy effort migration—not normal
  model+effort entry routing. Quartermaster Auto with no separate effort override uses the
  selected entry's effort, including on fallback.
- **The database**: anything about a particular account or model (placements, the tier entries:
  each a model at an effort; labels, pauses, the preferred account) is a row (skill
  `add-migration`). Saved legacy `None` efforts are frozen to explicit supported efforts from
  the configured tier defaults by `qm_store::models::migrate_legacy_efforts`; existing explicit
  entries stay unchanged. Unknown capabilities wait for a later pass; no-effort models retain
  `None`. New catalogue choices never offer "tier default". See `docs/routing.md`.
- **`ui.json`**: UI-only state (layout, popups, drafts, scrolls), written by the UI host
  (`qm-tui` `persist.rs`), because the UI can't reach the core's config. A new remembered UI field
  is added there (read per field, so an old file still loads); `tui.toml` is only read once.

## Steps

1. **Schema** in `crates/qm-types/src/config.rs`: add a field to a section struct, or a new
   section struct with `#[serde(default)]` and a `Default` impl, plus its field on `Config`. Every
   field must have a default, so a missing or partial file still loads. Never add
   `deny_unknown_fields`. Don't rename a key: an old file's key would become unknown and its value
   ignored.
2. **Loading** needs no change: `qm_core::config_file::load` deserialises through `serde_ignored`
   and returns unknown keys, which `crates/quartermaster/src/main.rs` logs as warnings ("a setting this
   version doesn't know"); they are not refused and not dropped from the user's file by loading.
   `save` writes atomically (temp file, fsync, rename, 0600).
3. **Reading it**: the core holds the live config in a `tokio::sync::watch` channel built in
   `crates/quartermaster/src/wiring.rs`. Read the current value where it's used (`config.borrow()`) rather
   than copying it at startup, so a change applies without a restart. Notifications are the
   example: `NotificationsPart` reads it at each snapshot.
4. **Changing it from Settings** (if the user should change it in the app):
   - a `Command` variant in `crates/qm-types/src/api/mod.rs` and a field on `SettingsView`
     (`#[serde(default)]`), additive only (skill `core-ui-api`);
   - handle the command in `SettingsPart` (`crates/qm-core/src/settings_api.rs`) through
     `save_config(|c| …)`, which saves the file and then publishes the new config; on failure it
     shows an app note in the chat;
   - fill the view field in the part's `view()`;
   - draw and edit it in `crates/qm-tui/src/settings/` (`model.rs` holds the view model,
     `input.rs` sends with `intend(Command::…)`, `other.rs` is the System tab), give it a key and
     list the key in Help.
5. If the user should know about it, mention it in the README.

## Verify

`cargo xtask check`, `cargo build`, then in a throwaway run (skill `run-by-hand`): start with no
`config.toml` (defaults), with a partial one, with an unknown key (one warning in the log), and
change the setting in Settings; check `config.toml` afterwards and that the change applies without
a restart.
