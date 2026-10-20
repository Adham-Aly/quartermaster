//! Pure mappings from `claude`'s stream messages to our event parts.
//!
//! Every field is optional here: a missing number is zero, a missing string is absent, an
//! unknown value maps to the `Unknown`/`Other` case. The session task decides *when* to send what;
//! these functions only decide *what* a message says.

use crate::protocol::{str_at, u64_at};
use jiff::Timestamp;
use qm_engine::{
    CallUsage, ProviderError, ProviderErrorKind, RateLimitInfo, RateLimitStatus, StopReason,
    TurnUsage, UsageReading, UsageWindow,
};
use serde_json::Value;

/// The most characters a `BuiltinToolUse` summary carries.
const SUMMARY_CHARS: usize = 120;

/// `message_delta.usage` (or any Anthropic usage object) as one call's usage. `output_tokens`
/// already includes thinking; the thinking part is `output_tokens_details.thinking_tokens`.
pub fn call_usage(usage: &Value) -> CallUsage {
    CallUsage {
        input_tokens: u64_at(usage, "/input_tokens"),
        output_tokens: u64_at(usage, "/output_tokens"),
        thinking_tokens: u64_at(usage, "/output_tokens_details/thinking_tokens"),
        cache_read_tokens: u64_at(usage, "/cache_read_input_tokens"),
        cache_write_tokens: u64_at(usage, "/cache_creation_input_tokens"),
    }
}

/// A `stop_reason` string.
pub fn stop_reason(reason: Option<&str>) -> StopReason {
    match reason {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("refusal") => StopReason::Refusal,
        _ => StopReason::Unknown,
    }
}

/// What a `claude` process has spent, as a `result` reports it: its `total_cost_usd` and
/// `modelUsage` are running totals (claude 2.1.304), helper agents and the turns `claude` started
/// by itself included. A resumed process starts its totals from the last `cost-state` entry in the
/// transcript it resumes ([`saved_spent`]), so they include what earlier processes of the same
/// session spent. A turn's usage is the difference from the previous `result` on the same
/// process, or from that saved state for a resumed process's first turn ([`turn_usage`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spent {
    pub cost_usd: Option<f64>,
    /// `modelUsage` summed over the models, when the `result` has it.
    pub tokens: Option<CallUsage>,
}

/// The process's running totals in a `result`.
pub fn spent(result: &Value) -> Spent {
    totals(result.get("total_cost_usd"), result.get("modelUsage"))
}

/// The totals a transcript's `cost-state` entry saved (`totalCostUSD`, and `modelUsage` keyed like
/// a `result`'s), which `claude --resume` starts from.
pub fn saved_spent(cost_state: &Value) -> Spent {
    totals(cost_state.get("totalCostUSD"), cost_state.get("modelUsage"))
}

fn totals(cost_usd: Option<&Value>, model_usage: Option<&Value>) -> Spent {
    let tokens = model_usage.and_then(Value::as_object).map(|models| {
        models
            .values()
            .fold(CallUsage::default(), |sum, m| CallUsage {
                input_tokens: sum.input_tokens + u64_at(m, "/inputTokens"),
                output_tokens: sum.output_tokens + u64_at(m, "/outputTokens"),
                thinking_tokens: sum.thinking_tokens + u64_at(m, "/thinkingTokens"),
                cache_read_tokens: sum.cache_read_tokens + u64_at(m, "/cacheReadInputTokens"),
                cache_write_tokens: sum.cache_write_tokens + u64_at(m, "/cacheCreationInputTokens"),
            })
    });
    Spent {
        cost_usd: cost_usd.and_then(Value::as_f64),
        tokens,
    }
}

/// A turn's usage from its `result`: what the process spent since `before` (the previous turn's
/// `result` on this process; for its first turn, the saved state a resumed process started from,
/// or nothing for a new one). Without `modelUsage`, the tokens are the `result`'s own `usage`.
/// Returns the new running totals too.
pub fn turn_usage(
    result: &Value,
    model_calls: u32,
    duration_ms: u64,
    before: &Spent,
) -> (TurnUsage, Spent) {
    let now = spent(result);
    let tokens = match now.tokens {
        Some(total) => {
            let was = before.tokens.unwrap_or_default();
            CallUsage {
                input_tokens: total.input_tokens.saturating_sub(was.input_tokens),
                output_tokens: total.output_tokens.saturating_sub(was.output_tokens),
                thinking_tokens: total.thinking_tokens.saturating_sub(was.thinking_tokens),
                cache_read_tokens: total
                    .cache_read_tokens
                    .saturating_sub(was.cache_read_tokens),
                cache_write_tokens: total
                    .cache_write_tokens
                    .saturating_sub(was.cache_write_tokens),
            }
        }
        None => call_usage(&result.get("usage").cloned().unwrap_or(Value::Null)),
    };
    let cost_usd = now
        .cost_usd
        .map(|total| (total - before.cost_usd.unwrap_or(0.0)).max(0.0));
    let usage = TurnUsage {
        model_calls,
        input_tokens: tokens.input_tokens,
        output_tokens: tokens.output_tokens,
        thinking_tokens: tokens.thinking_tokens,
        cache_read_tokens: tokens.cache_read_tokens,
        cache_write_tokens: tokens.cache_write_tokens,
        cost_usd,
        duration_ms,
    };
    let totals = Spent {
        cost_usd: now.cost_usd.or(before.cost_usd),
        tokens: now.tokens.or(before.tokens),
    };
    (usage, totals)
}

/// A `rate_limit_event`'s report.
pub fn rate_limit(msg: &Value) -> Option<RateLimitInfo> {
    let info = msg.get("rate_limit_info")?;
    let status = match str_at(info, "/status")? {
        "allowed" => RateLimitStatus::Allowed,
        "allowed_warning" => RateLimitStatus::AllowedWarning,
        "rejected" => RateLimitStatus::Rejected,
        _ => return None,
    };
    Some(RateLimitInfo {
        status,
        window: str_at(info, "/rateLimitType").map(str::to_owned),
        resets_at: info.get("resetsAt").and_then(timestamp),
    })
}

/// A `rate_limit_event`'s usage windows (`unifiedWindows`), for `Engine::usage`.
pub fn usage_reading(msg: &Value, read_at: Timestamp) -> Option<UsageReading> {
    let windows = msg
        .pointer("/rate_limit_info/unifiedWindows")?
        .as_object()?;
    let windows: Vec<UsageWindow> = windows
        .iter()
        .filter_map(|(name, window)| {
            let utilization = window.get("utilization")?.as_f64()?;
            Some(UsageWindow {
                name: name.clone(),
                used_percent: utilization * 100.0,
                resets_at: window.get("resetsAt").and_then(timestamp),
            })
        })
        .collect();
    (!windows.is_empty()).then_some(UsageReading { windows, read_at })
}

/// A Unix time in seconds (or milliseconds, if it is that large).
#[allow(clippy::cast_possible_truncation)] // A fractional Unix time loses only its fraction.
fn timestamp(value: &Value) -> Option<Timestamp> {
    let n = value
        .as_i64()
        .or_else(|| value.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))?;
    if n > 100_000_000_000 {
        Timestamp::from_millisecond(n).ok()
    } else {
        Timestamp::from_second(n).ok()
    }
}

/// A `system/api_retry` message: `(attempt, max_retries reported, error, delay_ms)`.
pub fn api_retry(msg: &Value) -> (u32, Option<u32>, ProviderError, u64) {
    let attempt = u32::try_from(u64_at(msg, "/attempt")).unwrap_or(u32::MAX);
    let max = msg
        .get("max_retries")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    let status = msg
        .get("error_status")
        .and_then(Value::as_u64)
        .and_then(|n| u16::try_from(n).ok());
    let kind = match status {
        Some(429) => ProviderErrorKind::RateLimited,
        Some(529) => ProviderErrorKind::Overloaded,
        Some(401 | 403) => ProviderErrorKind::Auth,
        Some(413) => ProviderErrorKind::ContextOverflow,
        Some(400..=499) => ProviderErrorKind::InvalidRequest,
        Some(500..=599) => ProviderErrorKind::Server,
        None => ProviderErrorKind::Network,
        Some(_) => ProviderErrorKind::Other,
    };
    let message = msg.get("error").map_or_else(
        || match status {
            Some(status) => format!("the request failed with HTTP {status}"),
            None => "the request failed".to_owned(),
        },
        |e| {
            e.as_str().map_or_else(
                || str_at(e, "/message").map_or_else(|| e.to_string(), str::to_owned),
                str::to_owned,
            )
        },
    );
    let delay = u64_at(msg, "/retry_delay_ms");
    (
        attempt,
        max,
        ProviderError {
            kind,
            status,
            message,
        },
        delay,
    )
}

/// A `system/compact_boundary` message: `(automatic, tokens_before, tokens_after)`.
pub fn compacted(msg: &Value) -> (bool, Option<u64>, Option<u64>) {
    let meta = msg.get("compact_metadata").unwrap_or(msg);
    let trigger = str_at(meta, "/trigger").or_else(|| str_at(msg, "/trigger"));
    let number = |key: &str| {
        meta.get(key)
            .or_else(|| msg.get(key))
            .and_then(Value::as_u64)
    };
    (
        trigger == Some("auto"),
        number("pre_tokens"),
        number("post_tokens"),
    )
}

/// The one-line summary of a built-in tool use for the worker session view: `Bash` → its
/// command, `Read`/`Edit`/`Write` → the file, `Grep`/`Glob` → the pattern, otherwise the first
/// string argument; cut to 120 characters.
pub fn builtin_summary(name: &str, input: &Value) -> String {
    let key = match name {
        "Bash" => Some("command"),
        "Read" | "Edit" | "Write" | "NotebookEdit" => Some("file_path"),
        "Grep" | "Glob" => Some("pattern"),
        _ => None,
    };
    let text = key
        .and_then(|k| input.get(k).and_then(Value::as_str))
        .or_else(|| {
            input
                .as_object()
                .and_then(|o| o.values().find_map(Value::as_str))
        })
        .unwrap_or("");
    let one_line: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if one_line.chars().count() > SUMMARY_CHARS {
        let cut: String = one_line.chars().take(SUMMARY_CHARS - 1).collect();
        format!("{cut}…")
    } else {
        one_line
    }
}

/// The cap failure's message, with the number written the way the spec writes it (`8,192`).
pub fn output_cap_message(cap: u32) -> String {
    let digits = cap.to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("the reply passed the {grouped}-token output cap (thinking included)")
}

/// The plain sentence for a failed `result`.
pub fn result_error(result: &Value) -> String {
    if let Some(errors) = result.get("errors").and_then(Value::as_array) {
        let joined: Vec<String> = errors
            .iter()
            .map(|e| {
                e.as_str().map_or_else(
                    || str_at(e, "/message").map_or_else(|| e.to_string(), str::to_owned),
                    str::to_owned,
                )
            })
            .filter(|s| !s.is_empty())
            .collect();
        if !joined.is_empty() {
            return joined.join("; ");
        }
    }
    if let Some(text) = str_at(result, "/result").filter(|t| !t.is_empty()) {
        return text.to_owned();
    }
    match str_at(result, "/subtype") {
        Some(subtype) => format!("claude ended the turn with {subtype}"),
        None => "claude ended the turn with an error".to_owned(),
    }
}
