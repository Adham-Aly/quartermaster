//! What to tell the helper as the inbox changes: the whole inbox for the menu, and a banner for
//! each request that is new since the notifier started.
//!
//! The helper's lines are JSON: `state` (the menu's items, the terminal apps you are looking at
//! and the one a click brings forward) and `post` (one banner). `state` also withdraws the banners
//! of requests that have left the inbox, so nothing is lost while the helper is down.

use jiff::Timestamp;
use qm_types::api::{ActionKey, ActionKind};
use qm_types::attention::{Looking, Waiting, WaitingItem};
use serde_json::json;
use std::collections::HashSet;

/// The keys already seen, and the last `state` sent.
#[derive(Debug)]
pub(crate) struct Tracker {
    /// Requests waiting before this were there when the notifier started: no banner.
    since: Timestamp,
    known: HashSet<String>,
    sent: Option<String>,
}

impl Tracker {
    pub(crate) fn new(since: Timestamp) -> Self {
        Self {
            since,
            known: HashSet::new(),
            sent: None,
        }
    }

    /// A new helper knows nothing: the next step sends the whole state.
    pub(crate) fn reconnected(&mut self) {
        self.sent = None;
    }

    /// Notes what is waiting without telling anyone (notifications are off, or the helper is
    /// down): those requests never get a banner.
    pub(crate) fn absorb(&mut self, waiting: &Waiting) {
        self.known = waiting.items.iter().map(|i| key(&i.key)).collect();
    }

    /// The lines that bring the helper up to date with `waiting`.
    pub(crate) fn step(&mut self, waiting: &Waiting) -> Vec<String> {
        let mut lines = Vec::new();
        let state = state(waiting);
        if self.sent.as_ref() != Some(&state) {
            lines.push(state.clone());
            self.sent = Some(state);
        }
        let mut known = HashSet::with_capacity(waiting.items.len());
        for item in &waiting.items {
            let key = key(&item.key);
            let new = !self.known.contains(&key) && item.at >= self.since;
            if new && waiting.looking != Looking::Here {
                lines.push(post(item, &key));
            }
            known.insert(key);
        }
        self.known = known;
        lines
    }
}

/// A request's key as the helper knows it (and as a click brings it back).
pub(crate) fn key(key: &ActionKey) -> String {
    serde_json::to_string(key).unwrap_or_default()
}

fn kind(kind: ActionKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn state(waiting: &Waiting) -> String {
    let items: Vec<_> = waiting
        .items
        .iter()
        .map(|i| {
            json!({
                "key": key(&i.key),
                "kind": kind(i.kind),
                "title": i.title,
                "from": i.from,
            })
        })
        .collect();
    let looking: &[String] = match &waiting.looking {
        Looking::In(apps) => apps,
        Looking::Away | Looking::Here => &[],
    };
    json!({
        "op": "state",
        "items": items,
        "looking": looking,
        "terminal": waiting.terminal,
    })
    .to_string()
}

/// A banner's headline for each kind of request.
fn headline(kind: ActionKind) -> &'static str {
    match kind {
        ActionKind::Approval => "Approval needed",
        ActionKind::Decision => "Decision needed",
        ActionKind::Question => "Question for you",
        ActionKind::Plan => "Plan ready for you",
        ActionKind::Review => "Review ready",
        ActionKind::Report => "Report ready",
        ActionKind::Setup => "Setup needs you",
        ActionKind::Notice => "Notice",
        _ => "Waiting on you",
    }
}

fn post(item: &WaitingItem, key: &str) -> String {
    json!({
        "op": "post",
        "key": key,
        "title": headline(item.kind),
        "subtitle": item.from,
        "body": item.title,
    })
    .to_string()
}
