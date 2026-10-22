use super::*;

const UID: &str = "0123456789abcdef01234567";

fn block(body: &str) -> String {
    format!("<plan-reply task=\"T-0042\" plan=\"{UID}\" round=\"2\">\n{body}</plan-reply>\n")
}

#[test]
fn a_full_reply_parses() {
    let text = block(
        "Reply to round 2 of the plan for T-0042 (1 of 2 questions answered).\n\n## Q1 store\nChoice: sqlite, sled (SQLite, sled)\nNote:\n> line one\n>\n> line three\n\n## Q2 rollout\nLeft open.\n\n## Section data-flow (Data flow)\nChange:\n> move it\n",
    );
    let (replies, malformed) = find(&format!("hello\r\n{}", text.replace('\n', "\r\n")));
    assert_eq!(malformed, []);
    let r = &replies[0];
    assert_eq!(r.task, "T-0042");
    assert_eq!(r.plan_uid, UID);
    assert_eq!(r.round, 2);
    assert_eq!(r.counts(), (1, 1, 1));
    assert_eq!(
        r.items[0],
        Item::Question {
            number: 1,
            id: "store".into(),
            fields: vec![
                Field::Choice(vec!["sqlite".into(), "sled".into()]),
                Field::Note("line one\n\nline three".into()),
            ],
        }
    );
    assert_eq!(
        r.items[2],
        Item::Section {
            id: "data-flow".into(),
            title: "Data flow".into(),
            fields: vec![Field::Change("move it".into())],
        }
    );
    assert_eq!(r.question_ids().collect::<Vec<_>>(), ["store", "rollout"]);
}

#[test]
fn bad_headers_are_not_replies() {
    for header in [
        "<plan-reply task=\"X-1\" plan=\"0123456789abcdef01234567\" round=\"1\">",
        "<plan-reply task=\"T-1\" plan=\"short\" round=\"1\">",
        "<plan-reply task=\"T-1\" plan=\"0123456789abcdef01234567\" round=\"\">",
    ] {
        let (replies, malformed) = find(&format!("{header}\n## Q1 a\n</plan-reply>\n"));
        assert!(replies.is_empty() && malformed.is_empty(), "{header}");
    }
}

#[test]
fn unclosed_and_oversized_blocks_are_malformed() {
    let (_, malformed) = find(&format!(
        "<plan-reply task=\"T-1\" plan=\"{UID}\" round=\"1\">\n## Q1 a\n"
    ));
    assert_eq!(malformed, [Malformed::Unclosed]);
    let long = format!("Note:\n> {}\n", "x".repeat(MAX_REPLY_CHARS));
    let (replies, malformed) = find(&block(&format!("## Q1 a\n{long}")));
    assert_eq!(replies, []);
    assert_eq!(malformed, [Malformed::TooLong]);
}

#[test]
fn replace_and_only_replies() {
    let text = format!("before\n{}after", block("## Q1 a\nLeft open.\n"));
    assert_eq!(
        replace(&text, |_| "[reply]".into()),
        "before\n[reply]\nafter"
    );
    assert!(!only_replies(&text));
    assert!(only_replies(&format!(
        "  \n{}\n",
        block("## Q1 a\nLeft open.\n")
    )));
}

#[test]
fn unknown_lines_are_kept_for_the_planner() {
    let (replies, _) = find(&block("## Q1 a\nsomething else\n"));
    assert_eq!(
        replies[0].items[0],
        Item::Question {
            number: 1,
            id: "a".into(),
            fields: vec![Field::Unknown("something else".into())]
        }
    );
}
