//! What routing knows about an account's quota: its pools, and the largest one.
//!
//! Two sources feed it, both stored on the account's row:
//!
//! - `accounts.usage`: the usage windows the engines report while they run (Claude's
//!   `rate_limit_event` windows, ChatGPT's usage headers), kept by `settings_api::usage_feed`.
//! - `accounts.quota`: the provider quota checks Settings ▸ Accounts runs (ChatGPT's and Kimi
//!   Code's windows, Grok's credits, Copilot's monthly allowances, OpenRouter's key budget, an API
//!   key's prepaid balance where its provider reports one: `qm_auth::quota::BalanceSource`),
//!   converted here by [`from_summary`]. Only numbers and reset times are stored, never a
//!   provider string.
//!
//! A **balance** (any provider's) has no limit to measure it against, so it is measured against the
//! balance at the last top-up: each check keeps that reference beside the balance, and a check
//! that finds the balance higher than the one before (a top-up) starts a new reference there.
//! The first check is its own reference, so it reads 0% used; a balance of nothing is used up.
//!
//! [`read`] turns both into routing's [`Usage`] and whether the account is cut off:
//!
//! - **The largest pool** decides the reading: a prepaid or key budget (never resets) beats any
//!   window, and a longer window beats a shorter one (weekly beats 5-hour; a monthly allowance
//!   beats weekly). Pools of the same length (Claude's `seven_day` and `seven_day_opus`, or the
//!   same window from both sources) count the one most used. A window of unknown length counts
//!   only when nothing else is known.
//! - **A window past its reset time** has reset since it was read, so it counts as 0% used.
//! - **Cut off** is any pool that stops the whole account at 100% used, until its reset (a budget
//!   until the next check). Copilot's premium-request allowance doesn't stop the account (its
//!   included models still run), so it only ranks.
//! - No pool at all is [`Usage::Unknown`], never "all left". Providers without a quota source
//!   (Claude has none in Settings: the app never reads its credentials) are known only from the
//!   windows their engine reports.

use jiff::Timestamp;
use qm_auth::quota::{Money, QuotaKind, QuotaSummary};
use qm_engine::{UsageReading, UsageWindow};
use qm_routing::rank::Usage;
use serde::{Deserialize, Serialize};

/// A used percentage at or above this means the pool is used up.
const USED_UP: f64 = 100.0;

/// A month, for allowances that reset monthly without saying how long they run.
const MONTH_SECONDS: u64 = 30 * 86_400;

/// How long a pool runs before it resets, for picking the largest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Span {
    /// Not reported.
    #[default]
    Unknown,
    /// A window of this many seconds.
    Seconds(u64),
    /// A balance or budget that doesn't reset with time.
    Balance,
}

/// One pool, as `accounts.quota` stores it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pool {
    /// `seven_day`, `premium_requests`, `key_budget`: the app's own names, never the provider's.
    pub name: String,
    pub used_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<Timestamp>,
    #[serde(default)]
    pub span: Span,
    /// Using it up stops the whole account, not just some of its models.
    #[serde(default)]
    pub stops_account: bool,
    /// A balance's amount left, in its currency (the name says which).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<f64>,
    /// The balance at the last top-up, which `used_percent` is measured against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_up: Option<f64>,
}

/// `accounts.quota`'s JSON.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Stored {
    pub pools: Vec<Pool>,
}

/// A provider quota check's buckets as pools. Unlimited buckets can't run out, and code review,
/// code completions and a Grok product's share don't serve agents (or not alone), so they are
/// left out; a bucket with no number is too.
/// `previous` is the account's stored `accounts.quota`, for each balance's last top-up.
pub fn from_summary(summary: &QuotaSummary, previous: Option<&str>) -> Stored {
    let previous = previous
        .and_then(|j| serde_json::from_str::<Stored>(j).ok())
        .unwrap_or_default();
    let pools = summary
        .buckets
        .iter()
        .filter(|b| !b.unlimited)
        .filter_map(|b| {
            if b.kind == QuotaKind::Balance {
                return b.balance.map(|money| balance_pool(money, &previous));
            }
            let used_percent = b.remaining_percent.map(|left| 100.0 - left).or_else(|| {
                let (left, all) = (b.remaining?, b.entitlement.filter(|e| *e > 0)?);
                #[expect(clippy::cast_precision_loss, reason = "request counts are small")]
                Some(100.0 - left as f64 / all as f64 * 100.0)
            })?;
            let window = b.window_seconds.filter(|s| *s > 0).map(Span::Seconds);
            let (name, span, stops_account) = match b.kind {
                QuotaKind::Primary | QuotaKind::Secondary => {
                    let name = match b.window_seconds {
                        Some(18_000) => "five_hour",
                        Some(604_800) => "seven_day",
                        _ if b.kind == QuotaKind::Primary => "primary",
                        _ => "secondary",
                    };
                    (name, window.unwrap_or_default(), true)
                }
                QuotaKind::Chat => ("chat", window.unwrap_or(Span::Seconds(MONTH_SECONDS)), true),
                QuotaKind::Monthly => (
                    "monthly",
                    window.unwrap_or(Span::Seconds(MONTH_SECONDS)),
                    true,
                ),
                QuotaKind::Credits => (
                    "credits",
                    window.unwrap_or(Span::Seconds(MONTH_SECONDS)),
                    true,
                ),
                QuotaKind::PremiumRequests => (
                    "premium_requests",
                    window.unwrap_or(Span::Seconds(MONTH_SECONDS)),
                    false,
                ),
                QuotaKind::KeyBudget => ("key_budget", Span::Balance, true),
                _ => return None,
            };
            Some(Pool {
                name: name.to_owned(),
                used_percent: used_percent.clamp(0.0, 100.0),
                resets_at: b.resets_at,
                span,
                stops_account,
                balance: None,
                top_up: None,
            })
        })
        .collect();
    Stored { pools }
}

/// A balance, measured against the balance at its last top-up (module docs): the previous
/// check's reference while the balance hasn't gone up since that check, else this balance.
fn balance_pool(money: Money, previous: &Stored) -> Pool {
    let name = format!("balance_{}", money.currency.code());
    let amount = money.amount.max(0.0);
    let last = previous
        .pools
        .iter()
        .find(|p| p.name == name)
        .and_then(|p| Some((p.balance?, p.top_up?)))
        .filter(|(last, top_up)| last.is_finite() && top_up.is_finite());
    let top_up = match last {
        Some((last, top_up)) if amount <= last => top_up.max(amount),
        _ => amount,
    };
    let used_percent = if top_up > 0.0 {
        100.0 - amount / top_up * 100.0
    } else {
        USED_UP
    };
    Pool {
        name,
        used_percent: used_percent.clamp(0.0, 100.0),
        resets_at: None,
        span: Span::Balance,
        stops_account: true,
        balance: Some(amount),
        top_up: Some(top_up),
    }
}

/// How long an engine's window runs, from its name (`five_hour`, `seven_day`, `seven_day_opus`).
fn engine_span(window: &UsageWindow) -> Span {
    match window.name.as_str() {
        "five_hour" => Span::Seconds(5 * 3_600),
        name if name.starts_with("seven_day") => Span::Seconds(7 * 86_400),
        _ => Span::Unknown,
    }
}

/// Every pool on the account's row: engine windows, then quota checks. Unreadable JSON counts as
/// no reading.
fn pools(usage: Option<&str>, quota: Option<&str>) -> Vec<Pool> {
    let engine = usage
        .and_then(|j| serde_json::from_str::<UsageReading>(j).ok())
        .map(|r| r.windows)
        .unwrap_or_default()
        .into_iter()
        .map(|w| Pool {
            span: engine_span(&w),
            name: w.name,
            used_percent: w.used_percent,
            resets_at: w.resets_at,
            // Today's rule for engine windows: any one used up cuts the account off.
            stops_account: true,
            balance: None,
            top_up: None,
        });
    let checked = quota
        .and_then(|j| serde_json::from_str::<Stored>(j).ok())
        .map(|s| s.pools)
        .unwrap_or_default();
    engine
        .chain(checked)
        .filter(|p| p.used_percent.is_finite())
        .collect()
}

impl Pool {
    /// Percent used now: a window past its reset time has reset since it was read.
    fn used_at(&self, now: Timestamp) -> f64 {
        if self.resets_at.is_some_and(|at| at <= now) {
            0.0
        } else {
            self.used_percent.clamp(0.0, 100.0)
        }
    }
}

fn largest(pools: &[Pool], now: Timestamp) -> Option<&Pool> {
    pools.iter().max_by(|a, b| {
        a.span
            .cmp(&b.span)
            .then(a.used_at(now).total_cmp(&b.used_at(now)))
    })
}

/// The account's usage on its largest pool and whether it is cut off, from `accounts.usage` and
/// `accounts.quota` (module docs).
pub fn read(usage: Option<&str>, quota: Option<&str>, now: Timestamp) -> (Usage, bool) {
    let pools = pools(usage, quota);
    let cut_off = cutting(&pools, now).next().is_some();
    let usage = largest(&pools, now).map_or(Usage::Unknown, |p| Usage::Known {
        used_percent: p.used_at(now),
    });
    (usage, cut_off)
}

/// The pools that cut the account off: used up, stopping the whole account, and not reset yet.
fn cutting(pools: &[Pool], now: Timestamp) -> impl Iterator<Item = &Pool> {
    pools.iter().filter(move |p| {
        p.stops_account
            && p.used_at(now) >= USED_UP
            && (p.span == Span::Balance || p.resets_at.is_some_and(|at| at > now))
    })
}

/// What a cut-off account waits for, for the Settings line: a window to reset, or (for a spent
/// budget or balance, which never resets) a check that finds money left. `None` if not cut off.
pub fn cut_off_until(
    usage: Option<&str>,
    quota: Option<&str>,
    now: Timestamp,
) -> Option<&'static str> {
    let pools = pools(usage, quota);
    let mut cutting = cutting(&pools, now).peekable();
    cutting.peek()?;
    Some(if cutting.any(|p| p.span != Span::Balance) {
        "it resets"
    } else {
        "a check finds money left"
    })
}

/// The largest pool and how much of it is used, for the Settings line: "7-day window 60% used",
/// "balance 30% used since the last top-up".
pub fn largest_line(usage: Option<&str>, quota: Option<&str>, now: Timestamp) -> Option<String> {
    let pools = pools(usage, quota);
    let pool = largest(&pools, now)?;
    let used = pool.used_at(now);
    Some(if pool.top_up.is_some() {
        format!("balance {used:.0}% used since the last top-up")
    } else {
        format!("{} {used:.0}% used", pool_label(pool.span, &pool.name))
    })
}

fn pool_label(span: Span, name: &str) -> String {
    match (span, name) {
        (Span::Balance, _) => "prepaid budget".to_owned(),
        (_, "premium_requests") => "monthly premium requests".to_owned(),
        (_, "chat") => "monthly chat allowance".to_owned(),
        (_, "monthly") => "monthly allowance".to_owned(),
        (Span::Seconds(s), "credits") if s == 7 * 86_400 => "weekly credits".to_owned(),
        (_, "credits") => "credits".to_owned(),
        (Span::Seconds(s), _) if s == 5 * 3_600 => "5-hour window".to_owned(),
        (Span::Seconds(s), _) if s == 7 * 86_400 => "7-day window".to_owned(),
        (Span::Seconds(s), _) if s % 86_400 == 0 => format!("{}-day window", s / 86_400),
        (Span::Seconds(s), _) if s % 3_600 == 0 => format!("{}-hour window", s / 3_600),
        _ => "usage window".to_owned(),
    }
}
