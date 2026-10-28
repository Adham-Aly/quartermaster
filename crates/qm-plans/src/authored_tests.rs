use super::*;
use crate::{PageState, Submit};
use std::collections::BTreeMap;
use std::fmt::Write as _;

fn meta() -> Meta {
    Meta {
        task: "T-42".into(),
        task_label: None,
        plan_uid: "0123456789abcdef01234567".into(),
        round: 1,
        project: "test".into(),
        state: PageState::Open,
        previous: BTreeMap::new(),
        submit: None,
    }
}
fn rules() -> Rules {
    Rules {
        mode: PlanMode::Interactive,
        round: 1,
        user_rounds: 0,
        take_recommended: false,
    }
}
fn document(inner: &str) -> String {
    format!("<article><h1>Plan</h1><h2 id=\"assumptions\">Assumptions</h2>{inner}</article>")
}

#[test]
fn starter_is_valid_and_preserves_custom_markup() {
    let src = authoring_template().replace(
        "<p>Describe",
        "<p class=\"author-lead\" style=\"padding: 2px\">Describe",
    );
    let checked = check_html(&src, &Assets::new(), &rules()).unwrap();
    assert_eq!(checked.questions.len(), 1);
    assert_eq!(checked.questions[0].section.as_deref(), Some("assumptions"));
    let first = compose_html(&src, &Assets::new(), &meta()).unwrap();
    assert_eq!(first, compose_html(&src, &Assets::new(), &meta()).unwrap());
    assert!(
        first
            .html
            .contains("<p class=\"author-lead\" style=\"padding: 2px\">Describe")
    );
    for marker in [
        "q-direction",
        "q-other",
        "q-note",
        "sec-tools",
        "copy",
        "theme-toggle",
        "plan-meta",
        "buildReply",
    ] {
        assert!(first.html.contains(marker), "missing {marker}");
    }
    assert!(!first.html.contains("data-question="));
}

#[test]
fn authored_ids_paths_and_tool_output_are_not_redacted() {
    let authored = "<p class=\"author-prose\">Review T-42 with C-0042.</p><pre>/artifacts/T-42/C-0042/plan/plan.html\nTool output: task T-42, crewmate C-0042</pre>";
    let source = document(authored).replace("<h1>Plan</h1>", "<h1>Cache plan</h1>");
    let mut m = meta();
    for state in [
        PageState::Open,
        PageState::Final,
        PageState::Approved { at: "14:02".into() },
    ] {
        m.state = state;
        let html = compose_html(&source, &Assets::new(), &m).unwrap().html;
        assert!(
            html.contains(authored),
            "authored bytes must survive composition"
        );
        let header = html
            .split("<header")
            .nth(1)
            .unwrap()
            .split("</header>")
            .next()
            .unwrap();
        assert!(header.contains("class=\"page-meta\"><span>Cache plan</span>"));
        assert!(!header.contains("T-42"));
        assert!(!header.contains("C-0042"));
        assert!(html.contains("<title>Cache plan</title>"));
        assert!(html.contains("\"task\":\"T-42\""));
    }
}

#[test]
fn runtime_refresh_changes_only_metadata_and_controls() {
    let src = authoring_template();
    let mut m = meta();
    m.submit = Some(Submit {
        port: 12345,
        token: "a".repeat(64),
    });
    let open = compose_html(src, &Assets::new(), &m).unwrap();
    assert!(open.html.contains("id=\"send\""));
    assert!(open.html.contains("connect-src http://127.0.0.1:12345"));
    m.state = PageState::Locked {
        reason: "Closed <now>".into(),
    };
    let locked = compose_html(src, &Assets::new(), &m).unwrap();
    assert_eq!(open.checked, locked.checked);
    assert!(locked.html.contains("Closed &lt;now&gt;"));
    assert!(!locked.html.contains(&"a".repeat(64)));
    assert!(locked.html.contains("connect-src &#39;none&#39;"));
}

#[test]
fn round_rules_apply_to_html_not_technical_markdown() {
    let empty = document("<p>None.</p>");
    assert!(check_html(&empty, &Assets::new(), &rules()).is_err());
    let mut r = rules();
    r.round = 2;
    r.user_rounds = 1;
    assert!(check_html(&empty, &Assets::new(), &r).is_ok());
    r.mode = PlanMode::Internal;
    assert!(check_html(authoring_template(), &Assets::new(), &r).is_err());
    assert!(
        crate::check_technical(
            "# Agent handoff\n\n## Implementation\nChange src/cache.rs and test rollback.",
            &Assets::new()
        )
        .is_ok()
    );
    assert!(
        crate::check_technical(
            "# Agent handoff\n\n```question\nid=\"x\"\nask=\"Why?\"\n```",
            &Assets::new()
        )
        .is_err()
    );
}

#[test]
fn rejects_active_markup_parser_tricks_and_runtime_collisions() {
    for bad in [
        "<script>alert(1)</script>",
        "<iframe src=\"x\"></iframe>",
        "<form></form>",
        "<p onclick=\"alert(1)\">X</p>",
        "<a href=\"javascript:alert(1)\">X</a>",
        "<a href=\"java&#10;script:alert(1)\">X</a>",
        "<img src=\"https://example.com/x.png\" />",
        "<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>",
        "<p xmlns=\"urn:x\">X</p>",
        "<p class=\"choice\">X</p>",
        "<h2 id=\"copy\">X</h2>",
        "<h2 id=\"q-test\">X</h2>",
        "<h2 id=\"assumptions\">Duplicate</h2>",
        "<h2>No id</h2>",
        "<pre data-question=\"\"><b>Nested</b></pre>",
        "<img src=\"assets/a.png\"><h2 id=\"x\">X</h2></img>",
        "<pre data-render=\"text\">```question</pre>",
        "<img src=\"assets/x.png&quot; ) blah\" />",
    ] {
        let err = compose_html(&document(bad), &Assets::new(), &meta()).expect_err(bad);
        assert!(err.to_string().contains("plan.html"), "{err}");
    }
    for bad in [
        "<!DOCTYPE article><article><h1>X</h1></article>",
        "<article><h1>X</h1></article><article />",
        "<article><p>unclosed</article>",
    ] {
        assert!(compose_html(bad, &Assets::new(), &meta()).is_err(), "{bad}");
    }
    assert!(
        compose_html(
            &"x".repeat(crate::MAX_PLAN_BYTES + 1),
            &Assets::new(),
            &meta()
        )
        .is_err()
    );
}

#[test]
fn question_validation_open_multiple_other_and_numbering() {
    let first = "<pre data-question=\"\">id = \"first\"\nask = \"Q1 remains prose\"</pre>";
    let second = "<pre data-question=\"\">id = \"second\"\nask = \"Q1 remains prose\"\nmultiple = true\nother = false\nchoices = [{id=\"a\",label=\"A\",recommended=true},{id=\"b\",label=\"B\",recommended=true}]</pre>";
    let src = document(&format!("{first}{second}"));
    let page = compose_html(&src, &Assets::new(), &meta()).unwrap();
    assert_eq!(page.checked.questions.len(), 2);
    assert!(page.html.contains("q-answer"));
    assert!(page.html.contains("class=\"q-num\">Q2</span>"));
    assert!(page.html.contains("Q1 remains prose"));
    assert!(!page.checked.questions[1].offers_other());
    assert!(
        compose_html(
            &document(&format!("{first}{first}")),
            &Assets::new(),
            &meta()
        )
        .is_err()
    );
    assert!(
        compose_html(
            &document("<pre data-question=\"\">id=\"x\"\nask=\"\"</pre>"),
            &Assets::new(),
            &meta()
        )
        .is_err()
    );
    let mut many = String::new();
    for i in 0..61 {
        write!(
            many,
            "<pre data-question=\"\">id=\"q{i}\"\nask=\"Why?\"</pre>"
        )
        .unwrap();
    }
    assert!(compose_html(&document(&many), &Assets::new(), &meta()).is_err());
}

#[test]
fn diagrams_math_mockups_and_assets_use_existing_safe_renderers() {
    let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 10 10\"><rect id=\"shape\" width=\"10\" height=\"10\" /></svg>";
    let source = document(&format!(
        "<pre data-render=\"svg\">{}</pre><pre data-render=\"svg\">{}</pre><pre data-render=\"math\">x^2</pre><pre data-render=\"mockup\">&lt;p&gt;Preview&lt;/p&gt;</pre><img src=\"assets/drawing.svg\" alt=\"Flow\" />",
        escape(svg),
        escape(svg)
    ));
    let mut assets = Assets::new();
    assets.insert("assets/drawing.svg", svg.as_bytes().to_vec());
    let page = compose_html(&source, &assets, &meta()).unwrap();
    assert_eq!(page.checked.diagrams, 3);
    assert_eq!(page.checked.mockups, 1);
    assert!(page.html.contains("qm-d1-shape"));
    assert!(page.html.contains("qm-d2-shape"));
    assert!(page.html.contains("qm-d3-shape"));
    assert!(page.html.contains("sandbox=\"\""));
    assert!(page.html.contains("katex"));
}

#[test]
fn fingerprints_badges_and_source_are_stable() {
    let src = authoring_template();
    let a = compose_html(src, &Assets::new(), &meta()).unwrap();
    let mut m = meta();
    m.previous = a.checked.fingerprints();
    let unchanged = compose_html(src, &Assets::new(), &m).unwrap();
    assert!(unchanged.html.contains("data-updated=\"approach\" hidden"));
    let changed = compose_html(
        &src.replace("Describe the approach", "Explain the approach"),
        &Assets::new(),
        &m,
    )
    .unwrap();
    assert!(changed.html.contains("data-updated=\"approach\">updated"));
    assert!(check_html(&changed.html, &Assets::new(), &rules()).is_err());
}
