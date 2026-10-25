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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kinds(summary: &QuotaSummary) -> Vec<(QuotaKind, Option<f64>, Option<u64>)> {
        summary
            .buckets
            .iter()
            .map(|b| {
                (
                    b.kind,
                    b.remaining_percent.map(|p| (p * 100.0).round() / 100.0),
                    b.window_seconds,
                )
            })
            .collect()
    }

    #[test]
    fn reads_the_session_weekly_and_monthly_pools() {
        let summary = parse(&json!({
            "limits": [{
                "window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
                "detail": {"limit": "100", "used": "25", "remaining": "75"}
            }],
            "usages": {
                "limit_5h": {"used_ratio": 0.625, "reset_time": "2026-09-16T20:15:44Z"},
                "limit_7d": {"used_ratio": 0.125, "reset_time": "2026-09-20T00:00:00Z"},
                "limit_month_total": {"used_ratio": 0.0056, "reset_time": "2026-10-17T00:00:00Z"},
                "limit_month_code": {"used_ratio": 0, "reset_time": "2026-10-17T00:00:00Z"}
            },
            "user": {"membership": {"level": "\u{1b}[31mLEVEL_ADVANCED"}},
        }))
        .unwrap()
        .unwrap();
        // The ratio pools win over the older five-hour count.
        assert_eq!(
            kinds(&summary),
            [
                (QuotaKind::Primary, Some(37.5), Some(18_000)),
                (QuotaKind::Secondary, Some(87.5), Some(604_800)),
                (QuotaKind::Monthly, Some(99.44), None),
            ]
        );
        assert_eq!(
            summary.buckets[0].resets_at,
            Some("2026-09-16T20:15:44Z".parse().unwrap())
        );
        assert!(!format!("{summary:?}").contains("LEVEL"));
    }

    #[test]
    fn reads_the_older_count_shape_when_no_pool_is_reported() {
        let summary = parse(&json!({
            "usage": {"limit": "100", "used": "19", "remaining": "81",
                      "resetTime": "2026-09-19T16:45:59.449979Z"},
            "limits": [
                {"window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
                 "detail": {"limit": 100, "remaining": 75}},
                {"window": {"duration": 1, "timeUnit": "TIME_UNIT_DAY"},
                 "detail": {"limit": 100, "used": 99}}
            ]
        }))
        .unwrap()
        .unwrap();
        assert_eq!(
            kinds(&summary),
            [
                (QuotaKind::Secondary, Some(81.0), Some(604_800)),
                (QuotaKind::Primary, Some(75.0), Some(18_000)),
            ]
        );
        assert!(summary.buckets[0].resets_at.is_some());
    }

    #[test]
    fn unknown_answers_are_refused_and_empty_ones_are_no_reading() {
        for value in [json!([]), json!({}), json!({"data": {"used_ratio": 0.5}})] {
            assert_eq!(parse(&value), Err(QuotaError::InvalidResponse));
        }
        for value in [
            json!({"usages": {}}),
            json!({"usages": {"limit_5h": {"used_ratio": -1}}, "limits": []}),
            json!({"usage": {"limit": 0, "used": 0}}),
        ] {
            assert_eq!(parse(&value), Ok(None));
        }
        // An over-full pool is used up, not over 100%.
        let full = parse(&json!({"usages": {"limit_7d": {"used_ratio": 1.4}}}))
            .unwrap()
            .unwrap();
        assert_eq!(full.buckets[0].remaining_percent, Some(0.0));
    }
}
