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

#[cfg(test)]
mod tests {
    use super::*;
    use qm_types::ids::{HeldActionId, TaskId};
    use serde_json::Value;

    fn at(second: i64) -> Timestamp {
        Timestamp::from_second(second).unwrap()
    }

    fn held(id: i64, second: i64) -> WaitingItem {
        WaitingItem::new(
            ActionKey::Held {
                id: HeldActionId::new(id),
            },
            ActionKind::Approval,
            format!("Merge PR #{id}"),
            at(second),
        )
        .with_from(Some("C-0042".into()))
    }

    fn waiting(items: Vec<WaitingItem>, looking: Looking) -> Waiting {
        Waiting::new(true, items, looking)
    }

    fn ops(lines: &[String]) -> Vec<(String, Option<String>)> {
        lines
            .iter()
            .map(|l| {
                let v: Value = serde_json::from_str(l).unwrap();
                (
                    v["op"].as_str().unwrap().to_owned(),
                    v["body"].as_str().map(str::to_owned),
                )
            })
            .collect()
    }

    #[test]
    fn a_new_request_posts_once_and_the_menu_follows_the_inbox() {
        let mut t = Tracker::new(at(100));
        assert_eq!(
            ops(&t.step(&waiting(vec![], Looking::Away))),
            [("state".into(), None)]
        );
        let one = waiting(vec![held(1, 150)], Looking::Away);
        assert_eq!(
            ops(&t.step(&one)),
            [
                ("state".into(), None),
                ("post".into(), Some("Merge PR #1".into()))
            ]
        );
        // Nothing changed: nothing to say.
        assert_eq!(t.step(&one), [] as [String; 0]);
        // Answered: the menu empties (and the helper withdraws its banner).
        assert_eq!(
            ops(&t.step(&waiting(vec![], Looking::Away))),
            [("state".into(), None)]
        );
    }

    #[test]
    fn requests_from_before_the_start_never_post() {
        let mut t = Tracker::new(at(100));
        let lines = t.step(&waiting(vec![held(1, 50)], Looking::Away));
        assert_eq!(ops(&lines), [("state".into(), None)]);
    }

    #[test]
    fn looking_holds_the_banner_back_for_good() {
        let mut t = Tracker::new(at(100));
        let seen = waiting(vec![held(1, 150)], Looking::Here);
        assert_eq!(ops(&t.step(&seen)), [("state".into(), None)]);
        // Looking away later doesn't post what you already saw (and the menu is unchanged).
        let away = waiting(vec![held(1, 150)], Looking::Away);
        assert_eq!(t.step(&away), [] as [String; 0]);
    }

    #[test]
    fn looking_in_a_terminal_leaves_the_choice_to_the_helper() {
        let mut t = Tracker::new(at(100));
        let lines = t.step(&waiting(
            vec![held(1, 150)],
            Looking::In(vec!["com.mitchellh.ghostty".into()]),
        ));
        let state: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(state["looking"], json!(["com.mitchellh.ghostty"]));
        assert_eq!(state["items"][0]["kind"], "approval");
        assert_eq!(state["items"][0]["from"], "C-0042");
        let post: Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(post["title"], "Approval needed");
        assert_eq!(post["subtitle"], "C-0042");
        assert_eq!(post["key"], r#"{"source":"held","id":1}"#);
    }

    #[test]
    fn what_was_absorbed_never_posts_and_a_new_helper_gets_the_state() {
        let mut t = Tracker::new(at(100));
        t.absorb(&waiting(vec![held(1, 150)], Looking::Away));
        t.reconnected();
        let report = WaitingItem::new(
            ActionKey::Report {
                task: TaskId::new(7).unwrap(),
            },
            ActionKind::Report,
            "Why the build is slow",
            at(160),
        );
        let lines = t.step(&waiting(vec![held(1, 150), report], Looking::Away));
        assert_eq!(
            ops(&lines),
            [
                ("state".into(), None),
                ("post".into(), Some("Why the build is slow".into()))
            ]
        );
    }

    #[test]
    fn a_key_reads_back() {
        let k = ActionKey::Plan {
            task: TaskId::new(3).unwrap(),
        };
        assert_eq!(serde_json::from_str::<ActionKey>(&key(&k)).unwrap(), k);
    }
}
