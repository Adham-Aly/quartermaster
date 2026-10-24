//! The review wait's pure parts: what `waiting_on` keeps and what the supervisor is told.
use super::*;

#[test]
fn the_wait_keeps_the_report_and_the_wake_text() {
    let wait = waiting_on(ReportId::new(7), "C-0003 reported.");
    assert!(is_review_wait(&wait));
    assert_eq!(wait["review"], 7);
    assert_eq!(wait["wake"], "C-0003 reported.");
    assert!(!is_review_wait(&serde_json::json!({ "plan": 1 })));
    assert!(!is_review_wait(&serde_json::json!({ "decision": "x" })));
}

#[test]
fn the_follow_up_has_no_default_action() {
    let task = TaskId::new(3).unwrap();
    for by in [GoAhead::Card, GoAhead::Autonomy, GoAhead::Auto] {
        let text = follow_up(task, by);
        assert!(
            text.contains("only what the user asked for when they asked for the review"),
            "{text}"
        );
        assert!(
            text.contains("if neither says, do nothing with them"),
            "{text}"
        );
        assert!(
            text.ends_with(&format!("Then settle_task {task}.")),
            "{text}"
        );
    }
    assert!(follow_up(task, GoAhead::Card).contains("pressed Go ahead on its card"));
    assert!(follow_up(task, GoAhead::Autonomy).contains("switched to auto autonomy"));
    assert!(follow_up(task, GoAhead::Auto).contains("don't wait for the user"));
}
