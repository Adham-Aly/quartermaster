//! The replay rules, after Pi's `packages/ai/src/api/transform-messages.ts` at the pinned commit
//! (MIT).
//!
//! Applied to the whole context before every request by [`prepare`], whatever the adapter:
//!
//! 1. Assistant messages that were aborted or failed are dropped, with their tool results: they
//!    may hold half a reply, and replaying one makes some APIs refuse the request.
//! 2. Thinking from a different provider, API or model is dropped (its signature or encrypted
//!    reasoning is only valid for the model that wrote it, and readable thinking sent as text
//!    would be taken as part of the reply); same-model thinking keeps its signature; empty
//!    unsigned thinking is dropped.
//! 3. Text from another model loses its signature.
//! 4. Tool calls from another model lose their provider signature (Gemini's function-call
//!    `thoughtSignature`, like foreign thinking), and their IDs are normalised by the adapter
//!    ([`crate::adapters::WireAdapter::normalize_tool_call_id`]); every matching tool result is
//!    renamed with it.
//! 5. A tool call with no result gets an error result "No result provided" before the next
//!    assistant or user message (or at the end).
//! 6. Images go to a model without image input as a short text note.

use crate::adapters::{WireAdapter, WireApi};
use crate::transcript::{AssistantBlock, Block, Message};
use qm_types::domain::Provider;
use qm_types::ids::ModelId;
use std::collections::{HashMap, HashSet};

/// What a user image becomes for a model without image input.
pub const USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
/// What a tool-result image becomes for a model without image input.
pub const TOOL_IMAGE_PLACEHOLDER: &str = "(tool image omitted: model does not support images)";
/// The result synthesised for a tool call that never got one.
pub const NO_RESULT: &str = "No result provided";

/// The model the context is about to be sent to.
#[derive(Clone, Copy, Debug)]
pub struct ReplayTarget<'a> {
    pub provider: Provider,
    pub api: WireApi,
    pub model: &'a ModelId,
    pub image_input: bool,
}

/// The context, ready for `target`.
pub fn prepare(
    messages: &[Message],
    target: ReplayTarget<'_>,
    adapter: &dyn WireAdapter,
) -> Vec<Message> {
    // Rule 1: the tool calls of dropped replies, whose results go too.
    let dropped_calls: HashSet<&str> = messages
        .iter()
        .filter(|m| m.is_unreplayable())
        .flat_map(|m| m.tool_calls().into_iter().map(|(id, _, _)| id))
        .collect();

    let mut renamed: HashMap<String, String> = HashMap::new();
    let mut transformed = Vec::with_capacity(messages.len());
    for msg in messages {
        match msg {
            Message::Assistant { .. } if msg.is_unreplayable() => {}
            Message::ToolResult { call_id, .. } if dropped_calls.contains(call_id.as_str()) => {}
            Message::Assistant {
                content,
                provider,
                api,
                model,
                stop,
                aborted,
                failed,
                usage,
                response_id,
            } => {
                let same_model =
                    *provider == target.provider && *api == target.api && model == target.model;
                let foreign = *provider != target.provider || *api != target.api;
                let content = transform_blocks(content, same_model, foreign, adapter, &mut renamed);
                transformed.push(Message::Assistant {
                    content,
                    provider: *provider,
                    api: *api,
                    model: model.clone(),
                    stop: *stop,
                    aborted: *aborted,
                    failed: *failed,
                    usage: *usage,
                    response_id: response_id.clone(),
                });
            }
            Message::ToolResult {
                call_id,
                name,
                content,
                is_error,
            } => transformed.push(Message::ToolResult {
                call_id: renamed
                    .get(call_id)
                    .cloned()
                    .unwrap_or_else(|| call_id.clone()),
                name: name.clone(),
                content: downgrade(content, target.image_input, TOOL_IMAGE_PLACEHOLDER),
                is_error: *is_error,
            }),
            Message::User { content } => transformed.push(Message::User {
                content: downgrade(content, target.image_input, USER_IMAGE_PLACEHOLDER),
            }),
        }
    }

    synthesize_missing_results(transformed)
}

/// Rules 2 to 4 for one assistant message's blocks.
fn transform_blocks(
    content: &[AssistantBlock],
    same_model: bool,
    foreign: bool,
    adapter: &dyn WireAdapter,
    renamed: &mut HashMap<String, String>,
) -> Vec<AssistantBlock> {
    content
        .iter()
        .filter_map(|block| match block {
            AssistantBlock::Thinking {
                text,
                signature,
                redacted,
            } => {
                let keep =
                    same_model && (*redacted || signature.is_some() || !text.trim().is_empty());
                keep.then(|| block.clone())
            }
            AssistantBlock::Text { text, .. } if !same_model => Some(AssistantBlock::Text {
                text: text.clone(),
                signature: None,
            }),
            AssistantBlock::Text { .. } => Some(block.clone()),
            AssistantBlock::ToolCall { .. } if same_model => Some(block.clone()),
            AssistantBlock::ToolCall { id, name, args, .. } => {
                let normalized = adapter.normalize_tool_call_id(id, foreign);
                if &normalized != id {
                    renamed.insert(id.clone(), normalized.clone());
                }
                Some(AssistantBlock::ToolCall {
                    id: normalized,
                    name: name.clone(),
                    args: args.clone(),
                    provider_signature: None,
                })
            }
        })
        .collect()
}

/// Rule 5: an error result for every call left unanswered, before the next assistant or user
/// message (or at the end).
fn synthesize_missing_results(transformed: Vec<Message>) -> Vec<Message> {
    let mut result = Vec::with_capacity(transformed.len());
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut answered: HashSet<String> = HashSet::new();
    let close = |result: &mut Vec<Message>,
                 pending: &mut Vec<(String, String)>,
                 answered: &mut HashSet<String>| {
        for (id, name) in pending.drain(..) {
            if !answered.contains(&id) {
                result.push(Message::ToolResult {
                    call_id: id,
                    name,
                    content: vec![Block::text(NO_RESULT)],
                    is_error: true,
                });
            }
        }
        answered.clear();
    };
    for msg in transformed {
        match &msg {
            Message::Assistant { .. } => {
                close(&mut result, &mut pending, &mut answered);
                pending = msg
                    .tool_calls()
                    .into_iter()
                    .map(|(id, name, _)| (id.to_owned(), name.to_owned()))
                    .collect();
            }
            Message::ToolResult { call_id, .. } => {
                answered.insert(call_id.clone());
            }
            Message::User { .. } => close(&mut result, &mut pending, &mut answered),
        }
        result.push(msg);
    }
    close(&mut result, &mut pending, &mut answered);
    result
}

/// Rule 6: each run of images becomes one placeholder for a model without image input.
fn downgrade(content: &[Block], image_input: bool, placeholder: &str) -> Vec<Block> {
    if image_input {
        return content.to_vec();
    }
    let mut out = Vec::with_capacity(content.len());
    let mut previous_was_placeholder = false;
    for block in content {
        match block {
            Block::Image { .. } => {
                if !previous_was_placeholder {
                    out.push(Block::text(placeholder));
                }
                previous_was_placeholder = true;
            }
            Block::Text { text } => {
                previous_was_placeholder = text == placeholder;
                out.push(block.clone());
            }
        }
    }
    out
}
