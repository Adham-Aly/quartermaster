//! The approval cards: pure builders over the held row and the facts its `settle.held` event
//! recorded. The pull request card matches the sample in `crates/qm-tui/src/sample.rs`.

use crate::delivery::github_tools::write_words;
use crate::held::HeldCardBuilder;
use qm_store::held::{HeldPayload, HeldRow, MergePayload};
use qm_types::api::{Card, CardAction, CardButton, CardKind, CardLine, HeldAnswer, Styled, Tone};
use qm_types::domain::GithubWriteAction;

/// The heading of every approval card.
pub const APPROVAL_HEADING: &str = "Approval needed";
/// How much of a GitHub write's body the card shows.
pub const WRITE_PREVIEW_CHARS: usize = 200;

fn s(text: impl Into<String>, tone: Tone) -> Styled {
    Styled::new(text, tone)
}

fn approve() -> CardButton {
    CardButton::new(
        "Approve",
        'a',
        CardAction::Answer {
            answer: HeldAnswer::Approve,
        },
    )
}

fn reject() -> CardButton {
    CardButton::new(
        "Reject",
        'r',
        CardAction::Answer {
            answer: HeldAnswer::Reject,
        },
    )
}

fn diff() -> CardButton {
    CardButton::new("Diff", 'd', CardAction::ShowDiff)
}

fn size(facts: &serde_json::Value) -> String {
    let n = |k: &str| {
        facts
            .get(k)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    format!("+{} \u{2212}{}", n("additions"), n("deletions"))
}

fn base_card(row: &HeldRow, task: qm_types::ids::TaskId) -> Card {
    let mut card = Card::new(CardKind::Approval, APPROVAL_HEADING);
    card.held = Some(row.id);
    card.title = Some(task.to_string());
    card.lines = vec![CardLine::new(2, vec![s(row.title.clone(), Tone::Text)])];
    card
}

/// `~` for your home folder, as the card shows paths.
fn tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_owned(),
    }
}

/// The cards for kinds `merge` (pull request, in place) and `push`.
#[derive(Clone, Copy, Debug, Default)]
pub struct MergeCards;

impl HeldCardBuilder for MergeCards {
    #[allow(clippy::too_many_lines)]
    fn card(&self, row: &HeldRow, facts: &serde_json::Value) -> Card {
        match &row.payload {
            HeldPayload::Merge(MergePayload::PullRequest { task, pr, head, .. }) => {
                let mut card = base_card(row, *task);
                let checks = match (
                    facts
                        .get("checks_passed")
                        .and_then(serde_json::Value::as_u64),
                    facts
                        .get("checks_total")
                        .and_then(serde_json::Value::as_u64),
                ) {
                    (Some(p), Some(t)) if t > 0 => Some((p, t)),
                    _ => None,
                };
                let mut line = vec![];
                match checks {
                    Some((p, t)) => {
                        line.push(s(
                            format!("PR #{pr} @ {} · {} · {p}/{t} ", head.short(), size(facts)),
                            Tone::Dim,
                        ));
                        line.push(s("✓", Tone::Ok));
                    }
                    None => line.push(s(
                        format!("PR #{pr} @ {} · {} · no checks", head.short(), size(facts)),
                        Tone::Dim,
                    )),
                }
                card.lines.push(CardLine::new(2, line));
                if let Some(review) = facts.get("review").filter(|r| !r.is_null()) {
                    let verdict = review
                        .get("verdict")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?");
                    let risk = review
                        .get("risk")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    let mut spans = vec![
                        s("Careful review: ", Tone::Dim),
                        s(
                            verdict,
                            if verdict == "pass" {
                                Tone::Ok
                            } else {
                                Tone::Warn
                            },
                        ),
                    ];
                    if !risk.is_empty() {
                        spans.push(s(format!(" · risk {risk}"), Tone::Dim));
                    }
                    card.lines.push(CardLine::new(2, spans));
                }
                card.buttons = vec![approve(), diff(), reject()];
                card
            }
            HeldPayload::Merge(MergePayload::InPlace {
                task,
                branch,
                head,
                checkout,
                target_branch,
            }) => {
                let mut card = base_card(row, *task);
                card.lines.push(CardLine::new(
                    2,
                    vec![s(
                        format!(
                            "merge {branch} @ {} into {target_branch} · {}",
                            head.short(),
                            size(facts)
                        ),
                        Tone::Dim,
                    )],
                ));
                card.lines.push(CardLine::new(
                    2,
                    vec![s(
                        format!(
                            "your checkout {} must be clean when you approve",
                            tilde(checkout)
                        ),
                        Tone::Dim,
                    )],
                ));
                card.buttons = vec![approve(), diff(), reject()];
                card
            }
            HeldPayload::Push(p) => {
                let mut card = base_card(row, p.task);
                card.lines.push(CardLine::new(
                    2,
                    vec![s(
                        format!(
                            "push {} @ {} to {} · {}",
                            p.branch,
                            p.head.short(),
                            p.remote,
                            size(facts)
                        ),
                        Tone::Dim,
                    )],
                ));
                card.lines.push(CardLine::new(
                    2,
                    vec![s("CI runs after the push", Tone::Dim)],
                ));
                card.buttons = vec![approve(), diff(), reject()];
                card
            }
            _ => {
                let mut card = Card::new(CardKind::Approval, APPROVAL_HEADING);
                card.held = Some(row.id);
                card.title = Some(row.title.clone());
                card.buttons = vec![approve(), reject()];
                card
            }
        }
    }
}

/// The card for kind `github_write`.
#[derive(Clone, Copy, Debug, Default)]
pub struct GithubWriteCards;

impl HeldCardBuilder for GithubWriteCards {
    fn card(&self, row: &HeldRow, _facts: &serde_json::Value) -> Card {
        let mut card = Card::new(CardKind::Approval, APPROVAL_HEADING);
        card.held = Some(row.id);
        if let HeldPayload::GithubWrite(p) = &row.payload {
            card.title = Some(p.task.to_string());
            let what = match p.action {
                GithubWriteAction::OpenIssue => format!(
                    "open an issue in {}: “{}”",
                    p.repo,
                    p.title.as_deref().unwrap_or("")
                ),
                GithubWriteAction::AddLabels | GithubWriteAction::RemoveLabels => {
                    let verb = if p.action == GithubWriteAction::AddLabels {
                        "add"
                    } else {
                        "remove"
                    };
                    let target = p
                        .number
                        .map_or_else(|| p.repo.clone(), |n| format!("{}#{n}", p.repo));
                    let prep = if verb == "add" { "to" } else { "from" };
                    format!("{verb} labels {} {prep} {target}", p.labels.join(", "))
                }
                GithubWriteAction::Comment => write_words(p.action, &p.repo, p.number),
            };
            card.lines = vec![CardLine::new(2, vec![s(what, Tone::Text)])];
            if let Some(body) = p.body.as_deref().filter(|b| !b.trim().is_empty()) {
                let mut preview: String = body
                    .chars()
                    .take(WRITE_PREVIEW_CHARS)
                    .map(|c| if c == '\n' { ' ' } else { c })
                    .collect();
                if body.chars().count() > WRITE_PREVIEW_CHARS {
                    preview.push('…');
                }
                card.lines
                    .push(CardLine::new(2, vec![s(preview, Tone::Dim)]));
            }
        } else {
            card.title = Some(row.title.clone());
        }
        card.buttons = vec![approve(), reject()];
        card
    }
}
