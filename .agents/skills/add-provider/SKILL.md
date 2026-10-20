---
name: add-provider
description: Add a model provider (a new subscription login or API-key vendor) or a new wire adapter to the provider engine in qm-providers and qm-auth - the Provider enum, the accounts migration, adapter choice, login and refresh, credential header, catalog and starter tiers, redaction. Use when connecting a new vendor, an OpenAI-compatible service, or a new provider API format.
---

# Adding a provider or a wire adapter

Claude subscriptions run on the Claude engine; every other account runs on the provider engine
(`qm_providers::ProviderEngine`), which picks a wire adapter per provider and credential. Grep an
existing provider (`Provider::Kimi`, `Provider::Xai`) to find every place one is named; the
compiler finds most `match` arms once the variant exists.

## 1. The provider

- `Provider` in `crates/qm-types/src/domain.rs` (`string_enum!`): add the variant with its
  snake_case wire name. `Provider::engine()` maps it to `EngineKind::Provider`.
- **Migration:** `accounts.provider` has a `CHECK` listing every wire name
  (`crates/qm-store/migrations/0001_initial.sql`). SQLite can't alter a `CHECK`, so add a migration
  that rebuilds `accounts` with the new name (skill `add-migration`). Without it, inserting the
  account fails.
- Names: `qm_auth::login::display_name`, `waiting_for` in `crates/qm-core/src/settings_api.rs`,
  and a proper name for `doc_markdown` in `clippy.toml` if docs mention it.

## 2. The adapter: `crates/qm-providers/src/adapters/`

Prefer an existing adapter:

| API | Adapter | Serves today |
|---|---|---|
| OpenAI Responses | `responses::ResponsesAdapter` (`platform`, `codex`, `xai`) | ChatGPT, OpenAI keys, xAI |
| OpenAI Chat Completions + compat flags | `chat_completions::ChatCompletionsAdapter` | OpenRouter, Kimi, Copilot, custom endpoints |
| Anthropic Messages | `anthropic::AnthropicAdapter` | Anthropic API keys |
| Gemini `streamGenerateContent` | `gemini::GeminiAdapter` | Gemini keys |

Then add the arm to `adapter_for` and the provider to `provider_supported` (`adapters/mod.rs`).
An OpenAI-compatible service also gets its base URL in `ChatCompletionsAdapter::default_base_url`
and its quirks as compat flags (`CompatFlag`) in `catalog/corrections.toml`.

A **new adapter** is one file implementing `WireAdapter`, plus a `WireApi` variant (its serde name
is recorded on every assistant message, so never rename one) and an arm in `adapter_for`. The
module docs of `adapters/mod.rs` are the contract:

- pure translation: no I/O, no sleeping, never a secret. The engine adds the credential, using
  `credential_header` (`Bearer`, or a named header such as `x-api-key`);
- `build_request` builds the body with `serde_json::json!`, never adding a field the login rejects;
- `parser` reports deltas only; a stream that ends without its terminal event is an error;
  unknown event types are logged at `debug` and ignored;
- keep opaque replay data (signatures, encrypted reasoning) as JSON strings in `AssistantBlock`
  signatures; `replay::prepare` replays it only to the same provider, API and model;
- `classify_error` names errors by HTTP status and the provider's code and message, never by a
  header or request content;
- `normalize_tool_call_id`, `can_send_output_cap`, `usage_windows` as the provider needs.

## 3. The credential: `crates/qm-auth/src/`

- **API key:** add it to `api_key.rs`: an endpoint in `ApiKeyEndpoints` and the one cheap, free
  request that proves the key (usually listing models). 401/403 is `Rejected`; nothing is saved
  unless the check passes.
- **Subscription login:** a module of its own (`xai.rs`, `kimi.rs`, `copilot.rs` use the device
  code in `device_code.rs`; `chatgpt.rs`, `openrouter.rs` use a browser callback). Add it to
  `SUBSCRIPTION_LOGINS` and `LoginEndpoints` in `login.rs`, its refresh to `tokens.rs`
  (`TokenKeeper` does every refresh), and how its identity is read to `identity.rs` so reconnecting
  replaces the account in place.
- How the engine classifies the credential: `split` in `crates/qm-providers/src/engine.rs`.
- Credentials live only in `auth.json` through `qm_auth`; `Secret` is never logged or serialised.
  If the provider's tokens have a recognisable prefix, add it to `SECRET_PREFIXES` in
  `crates/quartermaster/src/logging.rs`.

The Settings screens are data-driven from the core, so a new login appears there with no UI code.

## 4. Models

Map the provider in `crates/qm-providers/catalog/refresh.py`, add its defaults to
`corrections.toml`, regenerate the snapshot, and draft its starter tiers in
`crates/qm-providers/src/catalog/starter.rs` (skill `model-catalog`). A new starter set is a
draft (`approved: false`) until the user approves it.

## Provider-engine behaviour to keep

- Crewmates' `agent` tool (`crates/qm-providers/src/helpers/`) runs a fresh child on the same
  account and model in the worktree, with the same credential-free environment and only
  read/edit/write/grep/find/ls/bash plus `job_output` and `job_kill`: no nesting, none of our tools.
  At most `MAX_CONCURRENT_HELPERS` (4) at once; a child stops after 100 model calls or a 30-minute
  job wait; it is cancelled with the crewmate; its usage is summed into the turn; a child in flight
  during a crash is lost and the call returns an error on resume.
- Signal job ends and shutdowns with `watch::Sender::send_replace`, never `send`: `send` with no
  subscriber drops the value, which once hung `bash`, `job_kill` and shutdown. The same holds in
  `qm-core` tasks.
- The ChatGPT login listens on the callback port it actually bound (1455).

## Verify

`cargo xtask check`, `cargo build`, then connect the account in a throwaway run (`,` opens
Settings; skill `run-by-hand`) and run one turn on its cheapest model. Never paste the key or
token anywhere while doing it.
