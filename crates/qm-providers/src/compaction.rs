//! Automatic compaction, after Pi's `packages/coding-agent/src/core/compaction/compaction.ts` at
//! the pinned commit (MIT).
//!
//! - **Context tokens** are the last successful call's input + output + cache tokens, plus a
//!   chars/4 estimate of every message after it; with no usable call yet, the estimate of
//!   everything.
//! - **When**: the context passes [`threshold`], the autocompact window or the model's window
//!   (the smaller) less [`RESERVE_TOKENS`]: before each model call of a turn, between turns, and
//!   once mid-turn on a context-overflow error (then the call is repeated, not counted as a retry).
//!   The window is read from the settings at each check.
//! - **The cut** keeps about the newest 20,000 estimated tokens ([`KEEP_RECENT_TOKENS`]), cutting
//!   only before a user or assistant message, never before a tool result, so an assistant's tool
//!   calls stay with their results. A cut inside a turn (before an assistant message) is a split
//!   turn: the turn's prefix is summarised together with everything older.
//! - **The summary** is one request to the same model and account, with no tools, the fixed
//!   instruction below and the previous summary when there is one. It emits no `ModelCall*`
//!   events: it isn't the agent's call.
//!
//! The context after compaction is the summary as a user message followed by every message from
//! the first kept one; only the latest compaction counts.

use crate::transcript::{AssistantBlock, Block, Message};

/// Tokens kept free below the context window.
pub const RESERVE_TOKENS: u64 = 16_384;
/// About how many of the newest tokens a compaction keeps.
pub const KEEP_RECENT_TOKENS: u64 = 20_000;
/// About how many of the newest tokens a compaction you asked for (`/compact`) keeps: little
/// more than the last exchange, since you asked to make room.
pub const ON_REQUEST_KEEP_TOKENS: u64 = 2_000;
/// What an image counts as, in characters, for the estimate.
const IMAGE_CHARS: u64 = 4_800;
/// The longest tool result the summary request quotes, in characters.
const TOOL_RESULT_MAX_CHARS: usize = 2_000;

/// How the summary opens when it is put back into the context.
pub const SUMMARY_PREFIX: &str =
    "Summary of the earlier conversation, compacted automatically:\n\n";

/// The summary request's system prompt.
pub const SUMMARY_SYSTEM_PROMPT: &str = "You summarise a conversation between a user and an AI \
coding agent so the agent can continue the work from your summary alone. Do not continue the \
conversation and do not answer anything in it: write only the summary, in the format asked for.";

/// The summary request's instruction (engine-internal, not a role prompt).
pub const SUMMARY_INSTRUCTION: &str = "The messages above are the conversation so far. Write a \
structured checkpoint the agent will continue from, in exactly this format:

## Goal
What the user wants, in one to three lines (several goals if the work covers several tasks).

## Progress
### Done
- Completed steps and changes.
### In progress
- What was being worked on when the conversation was cut.
### Blocked
- Anything stopping progress, or \"(none)\".

## Decisions
- **Decision**: the short reason for it.

## Files touched
- Exact paths read, created or changed, each with a few words on what changed.

## Next steps
1. What should happen next, in order.

Keep every section short. Keep exact file paths, function names, commands and error messages.";

/// Added when an earlier summary exists.
pub const UPDATE_INSTRUCTION: &str = "An earlier summary is given in <previous-summary> tags. \
Merge it with the new messages: keep everything in it that still holds, move finished items from \
\"In progress\" to \"Done\", add new decisions and files, and update the next steps.";

/// One message of the context, with the session-file entry it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextItem {
    pub entry_id: String,
    pub message: Message,
}

/// What a request is built from: the latest summary, then the messages kept since.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Context {
    pub summary: Option<String>,
    pub items: Vec<ContextItem>,
}

impl Context {
    /// The messages to send: the summary (as a user message) first, if any.
    pub fn messages(&self) -> Vec<Message> {
        let mut out = Vec::with_capacity(self.items.len() + 1);
        if let Some(summary) = &self.summary {
            out.push(summary_message(summary));
        }
        out.extend(self.items.iter().map(|i| i.message.clone()));
        out
    }

    pub fn push(&mut self, entry_id: String, message: Message) {
        self.items.push(ContextItem { entry_id, message });
    }
}

/// The summary as the user message that opens the context.
pub fn summary_message(summary: &str) -> Message {
    Message::user_text(format!("{SUMMARY_PREFIX}{summary}"))
}

/// A message's size, estimated at four characters a token (conservative).
pub fn estimate_tokens(message: &Message) -> u64 {
    let blocks = |content: &[Block]| -> u64 {
        content
            .iter()
            .map(|b| match b {
                Block::Text { text } => text.chars().count() as u64,
                Block::Image { .. } => IMAGE_CHARS,
            })
            .sum()
    };
    let chars = match message {
        Message::User { content } | Message::ToolResult { content, .. } => blocks(content),
        Message::Assistant { content, .. } => content
            .iter()
            .map(|b| match b {
                AssistantBlock::Text { text, .. } | AssistantBlock::Thinking { text, .. } => {
                    text.chars().count() as u64
                }
                AssistantBlock::ToolCall { name, args, .. } => {
                    (name.chars().count() + args.to_string().chars().count()) as u64
                }
            })
            .sum(),
    };
    chars.div_ceil(4)
}

/// The usage of a successful call, as context tokens; `None` for aborted or failed replies and
/// zero usage.
fn usage_tokens(message: &Message) -> Option<u64> {
    match message {
        Message::Assistant {
            usage,
            aborted: false,
            failed: false,
            ..
        } => {
            let total = usage.input_tokens
                + usage.output_tokens
                + usage.cache_read_tokens
                + usage.cache_write_tokens;
            (total > 0).then_some(total)
        }
        _ => None,
    }
}

/// The context's size in tokens: the last successful call's usage plus estimates of what came
/// after it, or an estimate of everything (the summary included).
pub fn context_tokens(context: &Context) -> u64 {
    let last = context
        .items
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, item)| usage_tokens(&item.message).map(|t| (i, t)));
    match last {
        Some((index, tokens)) => {
            tokens
                + context.items[index + 1..]
                    .iter()
                    .map(|i| estimate_tokens(&i.message))
                    .sum::<u64>()
        }
        None => context.messages().iter().map(estimate_tokens).sum::<u64>(),
    }
}

/// The size above which the context compacts: the autocompact window or the model's window,
/// whichever is smaller, less [`RESERVE_TOKENS`].
pub fn threshold(model_window: u32, setting: u32) -> u64 {
    u64::from(setting.min(model_window)).saturating_sub(RESERVE_TOKENS)
}

/// Whether the context is over the threshold.
pub fn should_compact(tokens: u64, threshold: u64) -> bool {
    tokens > threshold
}

/// Whether an automatic check compacts: the context is over the threshold, and the repeat guard
/// allows it. `guard` is the size the context must grow past: the last compaction's `tokens_after`
/// (or the size at a failed attempt), so a context that stays over the threshold isn't compacted
/// on every call.
pub fn auto_compaction_due(tokens: u64, threshold: u64, guard: Option<u64>) -> bool {
    should_compact(tokens, threshold)
        && guard.is_none_or(|base| tokens >= base + KEEP_RECENT_TOKENS)
}

/// The smallest context size [`auto_compaction_due`] would compact at, for display only: a
/// read-only twin of the trigger, which it never changes.
pub fn trigger_point(threshold: u64, guard: Option<u64>) -> u64 {
    let past = threshold.saturating_add(1);
    guard.map_or(past, |g| past.max(g.saturating_add(KEEP_RECENT_TOKENS)))
}

/// Where to cut: the index of the first item to keep, keeping about `keep_tokens` of the newest
/// ones and cutting only before a user or assistant message. `None` when nothing would be
/// summarised (the cut would keep everything).
pub fn find_cut(items: &[ContextItem], keep_tokens: u64) -> Option<usize> {
    let is_cut_point = |i: usize| !items[i].message.is_tool_result();
    let cut_points: Vec<usize> = (0..items.len()).filter(|&i| is_cut_point(i)).collect();
    let first = *cut_points.first()?;
    let mut cut = first;
    let mut accumulated = 0_u64;
    for i in (0..items.len()).rev() {
        let tokens = estimate_tokens(&items[i].message);
        if tokens == 0 {
            continue;
        }
        accumulated += tokens;
        if accumulated >= keep_tokens {
            // The closest cut point at or after this item; if trailing tool results alone pass
            // the budget, keep their assistant call by taking the last cut point.
            cut = cut_points
                .iter()
                .copied()
                .find(|&c| c >= i)
                .unwrap_or(*cut_points.last().unwrap_or(&first));
            break;
        }
    }
    (cut > 0).then_some(cut)
}

/// Whether a cut at `cut` splits a turn (it falls before an assistant message, so the turn's user
/// message is among the summarised ones).
pub fn is_split_turn(items: &[ContextItem], cut: usize) -> bool {
    matches!(
        items.get(cut).map(|i| &i.message),
        Some(Message::Assistant { .. })
    )
}

/// The summary request's one user message: the conversation as plain text (so the model doesn't
/// carry it on), the previous summary, and the instruction.
pub fn summary_prompt(items: &[ContextItem], previous: Option<&str>, split_turn: bool) -> String {
    let mut prompt = format!(
        "<conversation>\n{}\n</conversation>\n\n",
        serialize_conversation(items)
    );
    if let Some(previous) = previous {
        prompt.push_str("<previous-summary>\n");
        prompt.push_str(previous);
        prompt.push_str("\n</previous-summary>\n\n");
    }
    prompt.push_str(SUMMARY_INSTRUCTION);
    if previous.is_some() {
        prompt.push_str("\n\n");
        prompt.push_str(UPDATE_INSTRUCTION);
    }
    if split_turn {
        prompt.push_str(
            "\n\nThe conversation was cut in the middle of a turn: the last user request above is \
             still being worked on, and the newest messages of that turn are kept as they are. \
             Say what that request is and what was done for it so far.",
        );
    }
    prompt
}

/// The conversation as `[User]: …` / `[Assistant]: …` lines (Pi's `serializeConversation`).
pub fn serialize_conversation(items: &[ContextItem]) -> String {
    let mut parts = Vec::new();
    for item in items {
        match &item.message {
            Message::User { .. } => {
                let text = item.message.text();
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant { content, .. } => {
                let text = item.message.text();
                if !text.is_empty() {
                    parts.push(format!("[Assistant]: {text}"));
                }
                let calls: Vec<String> = content
                    .iter()
                    .filter_map(|b| match b {
                        AssistantBlock::ToolCall { name, args, .. } => {
                            let args = match args {
                                serde_json::Value::Object(map) => map
                                    .iter()
                                    .map(|(k, v)| format!("{k}={v}"))
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                other => other.to_string(),
                            };
                            Some(format!("{name}({args})"))
                        }
                        _ => None,
                    })
                    .collect();
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Message::ToolResult { .. } => {
                let text = item.message.text();
                if !text.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate(&text, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
        }
    }
    parts.join("\n\n")
}

fn truncate(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}\n[… {} more characters]", count - max)
}

/// The context after a compaction that keeps `items[cut..]` behind `summary`.
pub fn compacted(context: &Context, cut: usize, summary: String) -> Context {
    Context {
        summary: Some(summary),
        items: context.items[cut..].to_vec(),
    }
}
