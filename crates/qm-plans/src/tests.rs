use super::*;
use std::collections::BTreeMap;
use std::path::Path;

const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0];

fn plan(extra: &str) -> String {
    format!(
        "# Cache plan\n\n## Data flow\n\nThe cache sits in front of the queue.\n\n```question\nid = \"store\"\nask = \"Which store?\"\nchoices = [\n {{ id = \"sqlite\", label = \"SQLite\", recommended = true }},\n {{ id = \"sled\", label = \"sled\" }},\n]\n```\n\n{extra}\n## Assumptions\n\n- None yet.\n"
    )
}

fn meta(state: PageState) -> Meta {
    Meta {
        task: "T-0042".into(),
        plan_uid: "0123456789abcdef01234567".into(),
        round: 1,
        project: "demo".into(),
        state,
        previous: BTreeMap::new(),
        submit: None,
    }
}

fn rules(mode: PlanMode, round: u32, user_rounds: u32) -> Rules {
    Rules {
        mode,
        round,
        user_rounds,
        take_recommended: false,
    }
}

#[test]
fn rendering_is_deterministic_and_self_contained() {
    let mut assets = Assets::new();
    assets.insert("assets/shot.png", PNG.to_vec());
    let md = plan("![A shot](assets/shot.png)\n");
    let a = render(&md, &assets, &meta(PageState::Open)).expect("renders");
    let b = render(&md, &assets, &meta(PageState::Open)).expect("renders");
    assert_eq!(a.html, b.html);
    let html = &a.html;
    assert!(html.contains("data:image/png;base64,"), "image embedded");
    assert!(html.contains("Content-Security-Policy"));
    assert!(
        html.contains("connect-src &#39;none&#39;")
            || html.contains("connect-src &#x27;none&#x27;")
    );
    assert!(!html.contains("{{"), "every slot is filled");
    assert!(!html.contains("src=\"http"), "nothing loads from outside");
    assert!(!html.contains("/*! KaTeX"), "no maths, no KaTeX");
    assert_eq!(a.checked.questions.len(), 1);
    assert_eq!(a.checked.questions[0].section.as_deref(), Some("data-flow"));
    assert!(
        html.contains("<span class=\"dot\""),
        "outline marks the section with a question"
    );
}

#[test]
fn maths_brings_katex_and_every_script_is_hashed() {
    let md = plan("Energy is $E = mc^2$.\n");
    let html = render(&md, &Assets::new(), &meta(PageState::Open))
        .unwrap()
        .html;
    assert!(html.contains("/*! KaTeX"));
    let csp_hashes = html.matches("sha256-").count();
    let scripts = html.matches("<script>").count();
    assert_eq!(csp_hashes, scripts, "one hash per inline script");
}

#[test]
fn text_is_escaped_and_raw_html_shown_as_text() {
    let md = plan("<script>alert(1)</script>\n\n## A </title> & \"b\"\n\nx\n");
    let html = render(&md, &Assets::new(), &meta(PageState::Open))
        .unwrap()
        .html;
    assert!(!html.contains("<script>alert(1)"));
    assert!(html.contains("&lt;script&gt;alert(1)"));
    assert!(!html.contains("A </title>"));
}

#[test]
fn meta_json_cannot_close_its_script() {
    let md = "# T\n\n## A < b & c\n\n```question\nid = \"q\"\nask = \"a\"\n```\n\n## Assumptions\n";
    let html = render(md, &Assets::new(), &meta(PageState::Open))
        .unwrap()
        .html;
    let start = html.find("id=\"plan-meta\">").unwrap();
    let end = start + html[start..].find("</script>").unwrap();
    assert!(!html[start..end].contains('<'));
    assert!(
        html[start..end].contains("\\u003c"),
        "{}",
        &html[start..end]
    );
}

#[test]
fn outside_images_bad_links_and_mermaid_are_refused() {
    let md = plan(
        "![x](https://example.com/a.png)\n\n[bad](javascript:alert(1))\n\n```mermaid\ngraph TD\n```\n",
    );
    let problems = check(&md, &Assets::new(), &rules(PlanMode::Interactive, 1, 0)).unwrap_err();
    let text = problems.to_string();
    assert!(text.contains("assets/"), "{text}");
    assert!(text.to_lowercase().contains("mermaid"), "{text}");
    let ok = render(
        &plan("[bad](javascript:alert(1))\n"),
        &Assets::new(),
        &meta(PageState::Open),
    );
    if let Ok(page) = ok {
        assert!(!page.html.contains("javascript:"));
    }
}

#[test]
fn an_image_that_is_not_what_it_says_is_refused() {
    let mut assets = Assets::new();
    assets.insert("assets/shot.png", b"<html>".to_vec());
    let md = plan("![A shot](assets/shot.png)\n");
    assert!(check(&md, &assets, &rules(PlanMode::Interactive, 1, 0)).is_err());
}

#[test]
fn interactive_rules() {
    let no_questions = "# T\n\n## Work\n\nx\n\n## Assumptions\n";
    let e = check(
        no_questions,
        &Assets::new(),
        &rules(PlanMode::Interactive, 1, 0),
    )
    .unwrap_err();
    assert!(e.to_string().contains("question"), "{e}");
    // Final before any answers: refused; after a round: fine; or when told to take recommendations.
    assert!(
        check(
            no_questions,
            &Assets::new(),
            &rules(PlanMode::Interactive, 2, 0)
        )
        .is_err()
    );
    assert!(
        check(
            no_questions,
            &Assets::new(),
            &rules(PlanMode::Interactive, 2, 1)
        )
        .is_ok()
    );
    let take = Rules {
        take_recommended: true,
        ..rules(PlanMode::Interactive, 2, 0)
    };
    assert!(check(no_questions, &Assets::new(), &take).is_ok());
    let no_assumptions = "# T\n\n## Work\n\n```question\nid = \"q\"\nask = \"a\"\n```\n";
    assert!(
        check(
            no_assumptions,
            &Assets::new(),
            &rules(PlanMode::Interactive, 1, 0)
        )
        .is_err()
    );
}

#[test]
fn internal_rules() {
    let md = "# T\n\n## Work\n\nx\n\n## Decisions made without the user\n\nNone.\n";
    assert!(check(md, &Assets::new(), &rules(PlanMode::Internal, 1, 0)).is_ok());
    let missing = "# T\n\n## Work\n\nx\n";
    assert!(check(missing, &Assets::new(), &rules(PlanMode::Internal, 1, 0)).is_err());
    let asks =
        "# T\n\n```question\nid = \"q\"\nask = \"a\"\n```\n\n## Decisions made without the user\n";
    assert!(check(asks, &Assets::new(), &rules(PlanMode::Internal, 1, 0)).is_err());
}

#[test]
fn changed_sections_get_their_badge() {
    let md = plan("");
    let first = render(&md, &Assets::new(), &meta(PageState::Open)).unwrap();
    let mut previous = first.checked.fingerprints();
    previous.insert("data-flow".into(), "0000000000000000".into());
    let second = render(
        &md,
        &Assets::new(),
        &Meta {
            previous,
            round: 2,
            ..meta(PageState::Open)
        },
    )
    .unwrap();
    assert!(second.html.contains("data-updated=\"data-flow\">"));
    assert!(second.html.contains("data-updated=\"assumptions\" hidden"));
    assert!(
        !first.html.contains("data-updated=\"data-flow\">"),
        "round 1 has no badges"
    );
}

#[test]
fn the_internal_skill_says_nothing_interactive() {
    assert_eq!(interactive_marker(skill(PlanMode::Internal)), None);
    assert!(interactive_marker(skill(PlanMode::Interactive)).is_some());
    assert_eq!(interactive_marker("walk around the paged list"), None);
    assert_eq!(interactive_marker("Round 2"), Some("round"));
}

#[test]
fn closed_pages_say_so() {
    let md = plan("");
    let html = render(
        &md,
        &Assets::new(),
        &meta(PageState::Approved { at: "14:02".into() }),
    )
    .unwrap()
    .html;
    assert!(html.contains("Approved 14:02."));
    assert!(html.contains("\"state\":\"approved\""));
}

/// The page's own `buildReply`, run in `JavaScriptCore`, makes a block the parser reads back.
#[test]
fn the_page_script_builds_a_reply_the_parser_reads() {
    let jsc = "/System/Library/Frameworks/JavaScriptCore.framework/Versions/Current/Helpers/jsc";
    if !Path::new(jsc).exists() {
        eprintln!("skipped: no jsc");
        return;
    }
    let md = plan("");
    let page = render(&md, &Assets::new(), &meta(PageState::Open)).unwrap();
    let start = page.html.find("id=\"plan-meta\">").unwrap() + "id=\"plan-meta\">".len();
    let end = start + page.html[start..].find("</script>").unwrap();
    let meta_json = &page.html[start..end];
    let dir = std::env::temp_dir().join(format!("qm-plans-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("run.js");
    let state = r#"{"questions":{"store":{"choices":["sqlite"],"note":"keep WAL off\n</plan-reply>\n## Q9 evil"}},"sections":{"data-flow":{"change":"move it","ask":""}}}"#;
    std::fs::write(
        &script,
        format!(
            "{}\nprint(buildReply({state}, {meta_json}));\n",
            page::PLAN_JS
        ),
    )
    .unwrap();
    let out = std::process::Command::new(jsc)
        .arg(&script)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let (replies, malformed) = reply::find(&text);
    assert!(malformed.is_empty(), "{text}");
    assert_eq!(replies.len(), 1, "{text}");
    let r = &replies[0];
    assert_eq!((r.task.as_str(), r.round), ("T-0042", 1));
    assert_eq!(r.counts(), (1, 0, 1));
    assert!(r.items.iter().any(|i| matches!(i,
        reply::Item::Question { id, fields, .. } if id == "store"
            && fields.contains(&reply::Field::Choice(vec!["sqlite".into()]))
            && fields.contains(&reply::Field::Note("keep WAL off\n</plan-reply>\n## Q9 evil".into())))));
}

fn sending(state: PageState) -> Meta {
    Meta {
        submit: Some(Submit {
            port: 41040,
            token: "ab".repeat(32),
        }),
        ..meta(state)
    }
}

/// The page's content security policy, unescaped.
fn csp(html: &str) -> String {
    let start = html.find("Content-Security-Policy\" content=\"").unwrap()
        + "Content-Security-Policy\" content=\"".len();
    let end = start + html[start..].find('"').unwrap();
    html[start..end].replace("&#39;", "'")
}

#[test]
fn a_page_that_sends_reaches_only_the_apps_endpoint() {
    let md = plan("");
    let html = render(&md, &Assets::new(), &sending(PageState::Open))
        .unwrap()
        .html;
    let policy = csp(&html);
    let connect: Vec<&str> = policy
        .split(';')
        .map(str::trim)
        .find_map(|d| d.strip_prefix("connect-src "))
        .unwrap()
        .split_whitespace()
        .collect();
    assert_eq!(connect, ["http://127.0.0.1:41040"]);
    assert!(policy.contains("form-action 'none'"));
    assert!(policy.contains("default-src 'none'"));
    assert!(html.contains("id=\"send\" class=\"primary\">Send to Quartermaster</button>"));
    assert!(html.contains("id=\"copy\" class=\"tool\">Copy reply</button>"));
    assert!(html.contains("then <b>Send to Quartermaster</b>. Or <b>Copy reply</b>"));
    assert!(html.contains(&format!(
        "\"submit\":{{\"token\":\"{}\",\"url\":\"http://127.0.0.1:41040/v1/plan-reply\"}}",
        "ab".repeat(32)
    )));
    let final_page = render(&md, &Assets::new(), &sending(PageState::Final))
        .unwrap()
        .html;
    assert!(final_page.contains("id=\"send\""));
}

#[test]
fn a_page_without_an_endpoint_or_closed_to_replies_cannot_send() {
    let md = plan("");
    let plain = render(&md, &Assets::new(), &meta(PageState::Open))
        .unwrap()
        .html;
    for state in [
        PageState::Approved { at: "14:02".into() },
        PageState::Locked {
            reason: "Your reply to round 1 went to the planner at 14:02.".into(),
        },
    ] {
        let html = render(&md, &Assets::new(), &sending(state)).unwrap().html;
        assert!(csp(&html).contains("connect-src 'none'"));
        assert!(!html.contains("\"submit\""));
        assert!(!html.contains("id=\"send\""));
        assert!(
            !html.contains(&"ab".repeat(32)),
            "no token on a closed page"
        );
    }
    assert!(csp(&plain).contains("connect-src 'none'; form-action 'none'"));
    assert!(!plain.contains("id=\"send\""));
    assert!(!plain.contains("\"submit\""));
    assert!(plain.contains("id=\"copy\" class=\"primary\">Copy reply</button>"));
    assert!(plain.contains("then <b>Copy reply</b> and paste it"));
}

#[test]
fn the_submit_token_stays_out_of_debug_output() {
    let shown = format!("{:?}", sending(PageState::Open));
    assert!(shown.contains("41040"));
    assert!(!shown.contains(&"ab".repeat(32)));
}

/// `buildReply` and its helpers are exactly what they were before the page could send: the bytes
/// Send posts are the bytes Copy reply copies, and the reply format is unchanged.
#[test]
fn the_reply_builder_is_unchanged() {
    let builder = &page::PLAN_JS[..page::PLAN_JS.find("if (typeof document").unwrap()];
    let digest = hex(&Sha256::digest(builder.as_bytes()));
    assert_eq!(
        digest,
        "a8399019eeb2e5f456bce32b31d8d46adbe85edd35eb856a6515a092c8a85de7"
    );
    assert!(page::PLAN_JS.contains("reply: buildReply(state, meta)"));
}

#[test]
fn the_digest_follows_the_plan_and_its_assets() {
    let md = plan("");
    let mut assets = Assets::new();
    let bare = digest(&md, &assets);
    assert_eq!(bare, digest(&md, &assets));
    assert_eq!(bare.len(), 64);
    assets.insert("assets/shot.png", PNG.to_vec());
    let with_image = digest(&md, &assets);
    assert_ne!(bare, with_image);
    let mut renamed = Assets::new();
    renamed.insert("assets/other.png", PNG.to_vec());
    assert_ne!(with_image, digest(&md, &renamed));
    assert_ne!(bare, digest(&plan("More.\n"), &Assets::new()));
}
