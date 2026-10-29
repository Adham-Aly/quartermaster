//! Kimi Code's plan usage: `GET https://api.kimi.com/coding/v1/usages` with the subscription's
//! own access token. Kimi doesn't document it; these are the shapes other tools read from it.
//!
//! - **Ratio pools** under `usages`: `limit_5h`, `limit_7d` and `limit_month_total`, each
//!   `{ used_ratio (0-1), reset_time }`.
//! - **The older count shape**, read only for a window the pools didn't give: a weekly
//!   `usage { limit, used, remaining, resetTime }` and a five-hour `limits[] { window, detail }`.
//!   Counts may come as strings.
//!
//! An answer with none of `usages`, `usage` or `limits` is unrecognized; one with them but no
//! window is no reading. The membership level and every other string stay unread.

use super::{QuotaBucket, QuotaError, QuotaKind, QuotaSummary};
use jiff::Timestamp;
use serde_json::Value;

const FIVE_HOURS: u64 = 5 * 3_600;
const WEEK: u64 = 7 * 86_400;

/// The answer's windows, `None` when it names no window, or [`QuotaError::InvalidResponse`] when
/// it isn't an answer this reader knows.
pub(super) fn parse(value: &Value) -> Result<Option<QuotaSummary>, QuotaError> {
    let root = value.as_object().ok_or(QuotaError::InvalidResponse)?;
    if !["usages", "usage", "limits"]
        .iter()
        .any(|key| root.contains_key(*key))
    {
        return Err(QuotaError::InvalidResponse);
    }
    let mut buckets = Vec::new();
    for (key, kind, seconds) in [
        ("limit_5h", QuotaKind::Primary, Some(FIVE_HOURS)),
        ("limit_7d", QuotaKind::Secondary, Some(WEEK)),
        ("limit_month_total", QuotaKind::Monthly, None),
    ] {
        let pool = &value["usages"][key];
        let Some(used) = pool["used_ratio"]
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
        else {
            continue;
        };
        buckets.push(window(kind, used * 100.0, &pool["reset_time"], seconds));
    }
    let mut older = Vec::new();
    if let Some(weekly) = counted(&value["usage"], QuotaKind::Secondary, WEEK) {
        older.push(weekly);
    }
    for limit in value["limits"].as_array().into_iter().flatten() {
        if window_seconds(&limit["window"]) == Some(FIVE_HOURS)
            && let Some(session) = counted(&limit["detail"], QuotaKind::Primary, FIVE_HOURS)
        {
            older.push(session);
        }
    }
    for bucket in older {
        if !buckets.iter().any(|b| b.kind == bucket.kind) {
            buckets.push(bucket);
        }
    }
    Ok((!buckets.is_empty()).then(|| QuotaSummary::new(buckets)))
}

fn window(kind: QuotaKind, used_percent: f64, reset: &Value, seconds: Option<u64>) -> QuotaBucket {
    let mut b = QuotaBucket::new(kind);
    b.remaining_percent = Some(100.0 - used_percent.clamp(0.0, 100.0));
    b.resets_at = timestamp(reset);
    b.window_seconds = seconds;
    b
}

/// A `{ limit, used, remaining, resetTime }` detail.
fn counted(detail: &Value, kind: QuotaKind, seconds: u64) -> Option<QuotaBucket> {
    let limit = count(&detail["limit"]).filter(|n| *n > 0.0)?;
    let used = count(&detail["used"])
        .or_else(|| Some(limit - count(&detail["remaining"])?))
        .filter(|n| *n >= 0.0)?;
    let reset = ["resetTime", "resetAt", "reset_time", "reset_at"]
        .iter()
        .map(|key| &detail[*key])
        .find(|v| !v.is_null())
        .unwrap_or(&Value::Null);
    Some(window(kind, used / limit * 100.0, reset, Some(seconds)))
}

fn count(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|n: &f64| n.is_finite())
}

/// A `{ duration, timeUnit }` window's length.
fn window_seconds(window: &Value) -> Option<u64> {
    let unit = match window["timeUnit"].as_str()? {
        "TIME_UNIT_MINUTE" => 60,
        "TIME_UNIT_HOUR" => 3_600,
        "TIME_UNIT_DAY" => 86_400,
        _ => return None,
    };
    window["duration"]
        .as_u64()
        .filter(|d| *d > 0)?
        .checked_mul(unit)
}

/// An RFC 3339 time, or Unix seconds (milliseconds, if that large).
fn timestamp(value: &Value) -> Option<Timestamp> {
    if let Some(text) = value.as_str() {
        return text.parse().ok();
    }
    let n = value.as_i64()?;
    if n > 1_000_000_000_000 {
        Timestamp::from_millisecond(n).ok()
    } else {
        Timestamp::from_second(n).ok()
    }
}
