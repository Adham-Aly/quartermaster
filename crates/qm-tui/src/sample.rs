//! Sample data for the Bridge and the other screens, for checking the UI by eye.
//!
//! The real binary shows it only when the environment variable [`ENV`] selects a sample; it is
//! a developer switch, never mentioned in the UI or `--help`. While it shows, the UI ignores the
//! core's snapshots, so the picture holds still; every command still goes to the core.

use crate::app::App;
use crate::hit::Pane;
use crate::settings::model::{
    Account, AccountStatus, Login, LoginMethod, LoginStatus, LoginStep, Model as SettingsModel,
    Pin, Placement, Provider, QuartermasterModel, QuartermasterNow, Settings,
};
use jiff::Timestamp;
use jiff::civil::date;
use jiff::tz::TimeZone;
use qm_types::api::{
    AccountBadge, ActionItem, ActionKey, ActionKind, ActionOpen, AgentContextView, AgentRef,
    Attention, Card, CardAction, CardButton, CardKind, CardLine, ChatLine, CompactingView,
    CompactionNote, ContextBasisView, ContextOccupancy, CrewRow, CrewTask, DiffFile,
    DiffFileStatus, DiffHunk, DiffLine, DiffLineKind, HeldAnswer, HeldChoice, NoteOutcome,
    PointView, ProjectRow, ProjectTask, SessionLine, SessionLineKind, SessionView, Snapshot,
    Styled, TaskDetail, TaskRow, Tone, TurnTokens, Waiting,
};
use qm_types::domain::{
    Autonomy, DeliveryMode, MessageKind, MessageSource, Provider as ProviderId,
};
use qm_types::domain::{Effort, Tier};
use qm_types::ids::{
    AccountId, HeldActionId, MessageId, ModelId, SecondmateId, TaskId, TaskLabelId,
};
use std::time::Duration;

/// The developer switch: `QM_DEV_SAMPLE=bridge quartermaster`.
pub const ENV: &str = "QM_DEV_SAMPLE";

/// The value of [`ENV`] that shows the sample Bridge.
pub const BRIDGE: &str = "bridge";

/// The value of [`ENV`] that opens the approval's split diff review.
pub const DIFF: &str = "diff";

/// The value of [`ENV`] that shows the chat sample.
pub const CHAT: &str = "chat";

/// The value of [`ENV`] that shows the sample decision card.
pub const DECISION: &str = "decision";

/// The value of [`ENV`] that shows the sample worker session.
pub const SESSION: &str = "session";

/// The value of [`ENV`] that shows the Bridge sample with the Quartermaster thinking.
pub const THINKING: &str = "thinking";

/// The value of [`ENV`] that shows the sample Bridge with a badge on the menu's Settings entry
/// (and so on the menu button).
pub const MENU_BADGE: &str = "menu-badge";

/// The Bridge with the Quartermaster compacting its context, and C-0042's last compaction
/// failed.
pub const COMPACTING: &str = "compacting";

/// The Bridge with no context readings yet: every figure unknown, never a 0.
pub const CONTEXT_UNKNOWN: &str = "context-unknown";

/// The Bridge with the Actions inbox focused and a long list in it.
pub const ACTIONS: &str = "actions";

/// Puts the sample worker session into `app`: C-0042's session in the Bridge sample's middle
/// panel, the input focused and empty. Nothing waits for you.
pub(crate) fn install_session(app: &mut App) {
    install(app);
    app.snapshot.attention = Attention::default();
    app.snapshot.session = Some(session_view());
    app.select_crew(task(42));
    app.enter_session(task(42));
}

/// The sample worker session's data.
#[doc(hidden)]
pub fn session_view() -> SessionView {
    let line = |id: u64, kind: SessionLineKind, minute: i8, text: &str| {
        SessionLine::new(id, kind, at(14, minute), text)
    };
    let mut brief = line(
        1,
        SessionLineKind::Supervisor,
        2,
        "Brief · C-0042 · fix(auth): token refresh race",
    );
    brief.detail = Some(String::from(
        "# Brief · C-0042 · fix(auth): token refresh race\n\nThe token refresh in api-server \
         sometimes logs people out. The repro from yesterday is solid; fix it from the confirmed \
         reproduction in C-0041.",
    ));
    let mut view = SessionView::new(task(42));
    view.title = String::from("fix(auth): token refresh race");
    view.note = String::from("api-server · sonnet-5-5 · supervisor: Quartermaster");
    view.lines = vec![
        brief,
        line(
            2,
            SessionLineKind::Agent,
            2,
            "Reading **AGENTS.md** and *CONTRIBUTING.md*, then calling `delivery_check` before any edit.",
        ),
        line(3, SessionLineKind::Tool, 3, "Read AGENTS.md"),
        line(
            4,
            SessionLineKind::Tool,
            3,
            "delivery_check  → push ok · PR ok · checks readable",
        ),
        line(
            5,
            SessionLineKind::Tool,
            4,
            "Bash cargo test auth  → 41 passed",
        ),
        line(
            6,
            SessionLineKind::Agent,
            5,
            "Baseline is green. Capturing the logout before changing anything: two concurrent \
             refreshes against a local server on port 41420.",
        ),
        line(
            7,
            SessionLineKind::You,
            12,
            "A unit test that reproduces the race is enough evidence here; skip the full e2e run.",
        ),
        line(
            8,
            SessionLineKind::Agent,
            13,
            "Understood. I'll drive two refreshes through the token store concurrently and record \
             that before and after.",
        ),
        line(9, SessionLineKind::Tool, 13, "Read src/auth/refresh.rs"),
        line(10, SessionLineKind::Tool, 14, "Edit src/auth/refresh.rs"),
        line(
            11,
            SessionLineKind::Tool,
            14,
            "Bash cargo test auth::refresh_race  → 1 failed (the race, reproduced)",
        ),
    ];
    view.status = String::from("running 14m · job J3 idle");
    view.turn_running = true;
    view.open = true;
    view.placeholder = String::from("message C-0042 directly…");
    view
}

/// The value of [`ENV`] that shows S-02's session.
pub const SECONDMATE: &str = "secondmate";

/// Shows the sample in `app` if [`ENV`] asks for it; returns whether it did.
pub(crate) fn install_from_env(app: &mut App) -> bool {
    if install_settings_from_env(app) {
        return true;
    }
    match std::env::var(ENV).as_deref() {
        Ok(BRIDGE) => install(app),
        Ok(DIFF) => install_diff(app),
        Ok(MENU_BADGE) => {
            install(app);
            app.view.badges.push(crate::popups::MenuEntry::Settings);
        }
        Ok(CHAT) => install_chat(app),
        Ok(SESSION) => install_session(app),
        Ok(THINKING) => install_thinking(app),
        Ok(DECISION) => install_decision(app),
        Ok(SECONDMATE) => install_secondmate(app),
        Ok(ACTIONS) => install_actions(app),
        Ok(COMPACTING) => {
            install(app);
            app.snapshot.context = compacting_context();
        }
        Ok(CONTEXT_UNKNOWN) => {
            install(app);
            app.snapshot.context = Vec::new();
        }
        _ => return false,
    }
    true
}

fn secondmate_ref() -> AgentRef {
    AgentRef::Secondmate {
        id: SecondmateId::new(2).expect("secondmate ids start at 1"),
    }
}

/// Puts S-02's session into `app`, in the Bridge sample's middle panel, the input focused and
/// empty.
pub(crate) fn install_secondmate(app: &mut App) {
    install(app);
    app.snapshot.session = Some(secondmate_view());
    app.select_crew(secondmate_ref());
    app.enter_session(secondmate_ref());
}

/// S-02's session as the core builds it (`qm_core::sessions::view::load_secondmate`): its
/// conversation with the Quartermaster's routed request and your direct message, no cards.
#[doc(hidden)]
pub fn secondmate_view() -> SessionView {
    let mut routed = ChatLine::new(
        MessageId::new(11),
        MessageSource::Supervisor,
        MessageKind::Message,
        at(13, 50),
        "Add rate limiting to web-app's API client: back off on 429 and honour Retry-After. \
         Base it on web-app's standing branch.",
    );
    routed.context = Some(String::from("R-0193"));
    let mut started = ChatLine::new(
        MessageId::new(12),
        MessageSource::Agent,
        MessageKind::Reply,
        at(13, 51),
        "Started C-0045 from `sm/web-app` on a medium model. I'll report back when its PR is up.",
    );
    started.acknowledgement = Some(String::from("Started: C-0045"));
    let asked = ChatLine::new(
        MessageId::new(13),
        MessageSource::You,
        MessageKind::Message,
        at(14, 5),
        "keep the retry cap at 3, the dashboard can't wait longer than that",
    );
    let mut reply = ChatLine::new(
        MessageId::new(14),
        MessageSource::Agent,
        MessageKind::Reply,
        at(14, 5),
        "Noted. I've told C-0045 to cap retries at 3.",
    );
    reply.context = Some(String::from("reply"));
    reply.acknowledgement = Some(String::from("Steered: C-0045"));
    let mut view = SessionView::new(secondmate_ref());
    view.title = String::from("secondmate");
    view.note = String::from("web-app frontend and its API client · idle");
    view.status = String::from("idle");
    view.subtitle = String::from("Charter · web-app: the frontend, its API client and its build");
    view.open = true;
    view.placeholder = String::from("message S-02 directly…");
    view.conversation = vec![routed, started, asked, reply];
    view
}

/// Puts the chat sample into `app`: the sample Bridge around the sample conversation, the input
/// box focused and empty.
pub(crate) fn install_chat(app: &mut App) {
    install(app);
    app.snapshot = chat();
    app.focus = Pane::Input;
    app.draft.text = String::new();
}

/// Puts the thinking sample into `app`: the sample Bridge after you sent your reply, the
/// Quartermaster in a turn that hasn't written anything yet, the input focused and empty.
pub(crate) fn install_thinking(app: &mut App) {
    install(app);
    app.snapshot = thinking();
    app.focus = Pane::Input;
    app.draft.text = String::new();
}

/// The thinking sample's data: the sample Bridge, your reply sent, the
/// Quartermaster's row busy.
#[doc(hidden)]
pub fn thinking() -> Snapshot {
    let mut out = snapshot();
    out.conversation.push(ChatLine::new(
        MessageId::new(3),
        MessageSource::You,
        MessageKind::Message,
        at(14, 3),
        "yes, start the rate limiting build",
    ));
    out.turn_running = true;
    out.thinking = true;
    if let Some(root) = out.crew.first_mut() {
        "working".clone_into(&mut root.status);
        root.busy = true;
    }
    out
}

/// Puts the sample Bridge into `app`: the data, the crew pane focused with C-0042 selected, and
/// your half-typed reply in the input box.
pub(crate) fn install(app: &mut App) {
    app.snapshot = snapshot();
    app.focus = Pane::Crew;
    app.draft.text = String::from("yes, start the rate limiting build");
    app.view.selected_crew = Some(task(42));
    app.view.sample = true;
    app.view.settings.model = settings();
    app.view.settings.loaded = true;
}

fn task(n: u32) -> AgentRef {
    AgentRef::Crewmate { task: task_id(n) }
}

fn task_id(n: u32) -> TaskId {
    TaskId::new(n).expect("task ids start at 1")
}

/// Today at `hour:minute` on this Mac's clock, so the header reads exactly that in any time zone.
fn at(hour: i8, minute: i8) -> Timestamp {
    date(2026, 10, 19)
        .at(hour, minute, 0, 0)
        .to_zoned(TimeZone::system())
        .expect("a valid local time")
        .timestamp()
}

fn s(text: &str, tone: Tone) -> Styled {
    Styled::new(text, tone)
}

/// The sample Bridge's data.
#[doc(hidden)]
pub fn snapshot() -> Snapshot {
    let mut out = Snapshot::default();
    out.machine = String::from("macbook");
    let mut claude = AccountBadge::new("Claude Max");
    claude.usage = Some(String::from("7d 35%"));
    out.accounts = vec![claude, AccountBadge::new("ChatGPT Pro")];
    out.projects = projects();
    out.crew = crew();
    out.conversation = conversation();
    out.tasks = tasks();
    out.actions = actions(vec![held_item(approval_card(), at(14, 2))]);
    out.context = context();
    // What the core answers when the approval card's Diff is pressed (the sample ignores the
    // core's snapshots, so it carries the answer already).
    out.diff = Some(diff());
    let mut attention = Attention::default();
    attention.approvals = 1;
    attention.decisions = 1;
    attention.running = 4;
    out.attention = attention;
    out
}

fn projects() -> Vec<ProjectRow> {
    let task = |n: u32, title: &str| ProjectTask::new(label_id(n), title);
    let mut api = ProjectRow::new("api-server", DeliveryMode::DirectPr, Autonomy::Normal);
    api.active = true;
    api.tasks = vec![
        task(42, "fix(auth): token refresh race"),
        task(39, "feat(billing): prorate plan changes"),
    ];
    let mut web = ProjectRow::new("web-app", DeliveryMode::DirectPr, Autonomy::Auto);
    web.active = true;
    web.tasks = vec![
        task(45, "feat(upload): resumable uploads"),
        task(47, "fix(ci): flaky upload test"),
    ];
    let mut dotfiles = ProjectRow::new("dotfiles", DeliveryMode::BranchOnly, Autonomy::Normal);
    dotfiles.tasks = vec![task(48, "fix(zsh): slow prompt start")];
    vec![
        api,
        web,
        dotfiles,
        ProjectRow::new("personal", DeliveryMode::BranchOnly, Autonomy::Normal),
    ]
}

fn crew() -> Vec<CrewRow> {
    let mut approve = CrewRow::new(task(39), 1, "C-0039 feat", "approve");
    approve.needs_you = true;
    approve.waiting = Some(Waiting::Approval);
    let mut decide = CrewRow::new(task(47), 1, "C-0047 fix", "decide");
    decide.needs_you = true;
    decide.waiting = Some(Waiting::Decision);
    let secondmate = AgentRef::Secondmate {
        id: SecondmateId::new(2).expect("secondmate ids start at 1"),
    };
    // C-0042 and C-0045 are in a turn; C-0046 runs but sits between turns.
    let busy = |mut row: CrewRow| {
        row.busy = true;
        row
    };
    // Each crewmate under its task: C-0046 reviews C-0042's fix in T-0042; S-02 runs two tasks.
    let under = |mut row: CrewRow, n: u32, project: &str| {
        row.task = Some(CrewTask::new(label_id(n), format!("T-{n:04} {project}")));
        row
    };
    vec![
        CrewRow::new(AgentRef::Quartermaster, 0, "Quartermaster", "ready"),
        under(
            busy(CrewRow::new(task(42), 1, "C-0042 fix", "12m")),
            42,
            "api-server",
        ),
        under(
            CrewRow::new(task(46), 1, "C-0046 review", "2m"),
            42,
            "api-server",
        ),
        under(approve, 39, "api-server"),
        under(decide, 47, "web-app"),
        CrewRow::new(secondmate, 1, "S-02 web-app", "idle"),
        under(
            busy(CrewRow::new(task(45), 2, "C-0045 build", "3m")),
            45,
            "web-app",
        ),
        under(
            CrewRow::new(task(48), 2, "C-0048 fix", "waits"),
            48,
            "dotfiles",
        ),
    ]
}

fn label_id(n: u32) -> TaskLabelId {
    TaskLabelId::new(n).expect("task ids start at 1")
}

fn conversation() -> Vec<ChatLine> {
    let mut yours = ChatLine::new(
        MessageId::new(1),
        MessageSource::You,
        MessageKind::Message,
        at(13, 41),
        "the token refresh in api-server sometimes logs people out. the repro from yesterday \
         is solid, go ahead and fix it",
    );
    yours.acknowledgement = Some(String::from("Started: C-0042"));
    let mut reply = ChatLine::new(
        MessageId::new(2),
        MessageSource::Agent,
        MessageKind::Reply,
        at(14, 2),
        "The rate-limiting plan for api-server is back: a token bucket per API key in Redis, \
         100 requests a minute by default. Want me to start the build?",
    );
    reply.context = Some(String::from("after C-0043 reported"));
    vec![yours, reply]
}

/// The sample approval for C-0039, listed in Actions.
fn approval_card() -> Card {
    let mut card = Card::new(CardKind::Approval, "Approval needed");
    card.held = Some(HeldActionId::new(1));
    card.title = Some(String::from("C-0039"));
    card.lines = vec![
        CardLine::new(2, vec![s("feat(billing): proration preview", Tone::Text)]),
        CardLine::new(
            2,
            vec![
                s(
                    &format!("PR #212 @ a1b2c3d · {} · 14/14 ", diff().stat),
                    Tone::Dim,
                ),
                s("✓", Tone::Ok),
            ],
        ),
        CardLine::new(
            2,
            vec![
                s("Careful review: ", Tone::Dim),
                s("pass", Tone::Ok),
                s(" · risk low", Tone::Dim),
            ],
        ),
    ];
    card.buttons = vec![
        CardButton::new(
            "Approve",
            'a',
            CardAction::Answer {
                answer: HeldAnswer::Approve,
            },
        ),
        CardButton::new("Diff", 'd', CardAction::ShowDiff),
        CardButton::new(
            "Reject",
            'r',
            CardAction::Answer {
                answer: HeldAnswer::Reject,
            },
        ),
    ];
    card
}

/// The chat sample (the reply, the app's `✓ Steered: C-0042`, the
/// Quartermaster's `✓ Noted for next time`, your 14:20 message that nothing came of), a plan
/// reply sent from its page as one line, then a question and a message of yours queued behind the
/// turn that asked it. The notice, the question and a plan's round wait in Actions.
#[doc(hidden)]
pub fn chat() -> Snapshot {
    let mut out = snapshot();
    out.conversation = chat_conversation();
    out.actions = actions(vec![
        held_item(approval_card(), at(14, 2)),
        notice_item(),
        question_item(),
        plan_item(),
        review_item(),
    ]);
    out
}

fn chat_conversation() -> Vec<ChatLine> {
    let you = |id: i64, hour: i8, minute: i8, text: &str| {
        ChatLine::new(
            MessageId::new(id),
            MessageSource::You,
            MessageKind::Message,
            at(hour, minute),
            text,
        )
    };

    let asked = you(1, 14, 10, "how's 42 going?");
    let mut reply = ChatLine::new(
        MessageId::new(2),
        MessageSource::Agent,
        MessageKind::Reply,
        at(14, 10),
        "C-0042 has the fix in and is re-running the auth suite before it opens the PR. Nothing \
         needs you yet.",
    );
    reply.context = Some(String::from("reply"));

    let mut steered = you(
        3,
        14,
        12,
        "tell 42 a unit test that reproduces the race is enough, skip the full e2e run",
    );
    steered.acknowledgement = Some(String::from("Steered: C-0042"));

    let mut noted = you(4, 14, 13, "don't ping me for lint-only CI failures");
    noted.acknowledgement = Some(String::from("Noted for next time"));

    let nothing = you(5, 14, 20, TTL);

    let sent = ChatLine::new(
        MessageId::new(9),
        MessageSource::App,
        MessageKind::Message,
        at(14, 22),
        "Plan reply sent from the page · C-0039 round 1 · 4 of 4 answered",
    )
    .with_one_line();

    let mut question = ChatLine::new(
        MessageId::new(6),
        MessageSource::Agent,
        MessageKind::Question,
        at(14, 24),
        "Which cache should the new TTL apply to? The **session cache** lives in `api-server`, \
         the *page cache* in web-app.",
    );
    question.context = Some(String::from("question"));

    let mut queued = you(7, 14, 25, "also, what is the TTL set to now?");
    queued.queued = true;

    vec![
        asked, reply, steered, noted, nothing, sent, question, queued,
    ]
}

/// The chat sample's message that nothing came of.
const TTL: &str = "what did we decide about the cache TTL";

/// The notice on [`TTL`], as Actions lists it.
fn notice_item() -> ActionItem {
    let mut notice = Card::new(CardKind::Notice, "Nothing came of your message");
    notice.note = Some(String::from("· after one retry"));
    notice.lines = vec![CardLine::new(2, vec![s(&format!("“{TTL}”"), Tone::Text)])];
    notice.buttons = vec![
        CardButton::new("Ask again", 'a', CardAction::AskAgain),
        CardButton::new("Leave it", 'l', CardAction::LeaveIt),
    ];
    let title = format!("{}: {TTL}", notice.heading);
    ActionItem::new(
        ActionKey::Notice {
            message: MessageId::new(5),
        },
        ActionKind::Notice,
        title,
        at(14, 20),
        notice,
        ActionOpen::Conversation,
    )
    .with_from(AgentRef::Quartermaster)
    .with_anchor(MessageId::new(5))
}

/// The Quartermaster's question with options, as Actions lists it: the card carries the question.
fn question_item() -> ActionItem {
    let text = "Which cache should the new TTL apply to? The session cache lives in api-server, \
                the page cache in web-app.";
    let mut card = Card::new(CardKind::Question, "Pick an answer, or type your own");
    card.lines = vec![CardLine::new(2, vec![s(text, Tone::Text)])];
    let options = ["The session cache", "The page cache", "Both"];
    card.buttons = options
        .iter()
        .zip('1'..)
        .map(|(option, key)| {
            CardButton::new(
                format!("{key} · {option}"),
                key,
                CardAction::Reply {
                    text: (*option).to_owned(),
                },
            )
        })
        .collect();
    ActionItem::new(
        ActionKey::Question {
            message: MessageId::new(6),
        },
        ActionKind::Question,
        "Which cache should the new TTL apply to?",
        at(14, 24),
        card,
        ActionOpen::Conversation,
    )
    .with_from(AgentRef::Quartermaster)
    .with_anchor(MessageId::new(6))
}

/// A plan's round whose questions wait on you, as Actions lists it.
fn plan_item() -> ActionItem {
    let page = "/tmp/quartermaster/plans/T-0044/plan.html";
    let mut card = Card::new(CardKind::Plan, "Plan questions");
    card.title = Some(String::from("C-0044 plan(auth): session store"));
    card.note = Some(String::from("· api-server · round 2"));
    card.lines = vec![
        CardLine::new(
            2,
            vec![s(
                "3 questions open. Open the page, answer, then Send to Quartermaster (or Copy \
                 reply and paste it here).",
                Tone::Text,
            )],
        ),
        CardLine::new(2, vec![s(page, Tone::Dim)]),
    ];
    card.buttons = vec![
        CardButton::new("Open", 'o', CardAction::OpenPlan { task: task_id(44) }),
        CardButton::new(
            "Copy path",
            'c',
            CardAction::CopyPlanPath {
                path: page.to_owned(),
            },
        ),
    ];
    let task = task_id(44);
    ActionItem::new(
        ActionKey::Plan { task },
        ActionKind::Plan,
        "Plan questions · plan(auth): session store",
        at(14, 24),
        card,
        ActionOpen::Plan { task },
    )
    .with_task(task)
    .with_from(AgentRef::Crewmate { task })
}

fn review_item() -> ActionItem {
    let task = task_id(45);
    let mut card = Card::new(CardKind::Review, "Review ready");
    card.title = Some(String::from("C-0045 review(auth): token refresh race"));
    card.note = Some(String::from("· api-server"));
    card.lines = vec![
        CardLine::new(
            2,
            vec![s(
                "Verdict findings · risk medium · 2 findings: 1 error, 1 warning",
                Tone::Text,
            )],
        ),
        CardLine::new(
            2,
            vec![s(
                "Open the report. Go ahead lets the Quartermaster do with the findings what you \
                 asked for, or nothing if you didn't say; or tell the Quartermaster what to do.",
                Tone::Dim,
            )],
        ),
    ];
    card.buttons = vec![
        CardButton::new("Open", 'o', CardAction::OpenReport { task }),
        CardButton::new("Go ahead", 'g', CardAction::ApproveReview { task }),
    ];
    ActionItem::new(
        ActionKey::Review { task },
        ActionKind::Review,
        "Review ready · review(auth): token refresh race",
        at(14, 26),
        card,
        ActionOpen::Session {
            agent: AgentRef::Crewmate { task },
        },
    )
    .with_task(task)
    .with_from(AgentRef::Crewmate { task })
}

fn tasks() -> Vec<TaskDetail> {
    let mut detail = TaskDetail::new(task_id(42), "fix(auth): token refresh race");
    detail.rows = vec![
        TaskRow::new(Some("project"), vec![s("api-server", Tone::Text)]),
        TaskRow::new(Some("task"), vec![s("T-0042", Tone::Text)]),
        TaskRow::new(Some("model"), vec![s("sonnet-5-5", Tone::Text)]),
        TaskRow::new(Some("account"), vec![s("Claude Max", Tone::Text)]),
        TaskRow::new(Some("tier"), vec![s("medium · effort Medium", Tone::Text)]),
        TaskRow::new(
            None,
            vec![s(
                "why: “Fixing from the confirmed reproduction in C-0041”",
                Tone::Dim,
            )],
        ),
        TaskRow::new(
            Some("branch"),
            vec![s("fix/upload-retry-on-slow-tmp ← main", Tone::Text)],
        ),
        TaskRow::new(Some("verify"), vec![s("Default (standard)", Tone::Text)]),
        TaskRow::new(
            Some("status"),
            vec![s("running", Tone::Ok), s(" 12m", Tone::Text)],
        ),
    ];
    detail.activity = vec![
        String::from("Bash cargo test auth"),
        String::from("Edit src/auth/refresh.rs"),
    ];
    detail.running = true;
    vec![detail]
}

/// The values of [`ENV`] that open Settings on the sample: the device-code login waiting,
/// connected, failed, or a browser login waiting.
pub const SETTINGS: &str = "settings";
pub const SETTINGS_CONNECTED: &str = "settings-connected";
pub const SETTINGS_FAILED: &str = "settings-failed";
pub const SETTINGS_BROWSER: &str = "settings-browser";
/// Settings with no login in progress and the Anthropic key refused.
pub const SETTINGS_BROKEN: &str = "settings-broken";

/// Opens Settings on the sample if [`ENV`] asks for it; returns whether it did.
pub(crate) fn install_settings_from_env(app: &mut App) -> bool {
    let value = std::env::var(ENV);
    let login = match value.as_deref() {
        Ok(SETTINGS) => SampleLogin::DeviceCode,
        Ok(SETTINGS_CONNECTED) => SampleLogin::Connected,
        Ok(SETTINGS_FAILED) => SampleLogin::Failed,
        Ok(SETTINGS_BROWSER) => SampleLogin::Browser,
        Ok(SETTINGS_BROKEN) => {
            install(app);
            app.view.settings.model = settings_broken();
            app.view.settings.loaded = true;
            app.open_popup(crate::popups::Popup::Settings);
            return true;
        }
        _ => return false,
    };
    install(app);
    app.view.settings.model = settings_with(login);
    app.view.settings.loaded = true;
    app.open_popup(crate::popups::Popup::Settings);
    true
}

/// Which login the Settings sample shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SampleLogin {
    /// GitHub Copilot's device code, waiting.
    DeviceCode,
    /// GitHub Copilot connected: 36 models, starter tiers filled.
    Connected,
    /// GitHub Copilot's login failed.
    Failed,
    /// ChatGPT's browser login, waiting.
    Browser,
}

/// The sample settings: three accounts, explicit model+effort tier entries and a fixed catalogue
/// including unplaced models, the pin on Opus 5.5 then Sonnet 5.5, Auto for the Quartermaster,
/// and GitHub Copilot's device-code login waiting.
pub(crate) fn settings() -> Settings {
    settings_with(SampleLogin::DeviceCode)
}

fn sample_account(id: i64, label: &str, detail: &str) -> Account {
    let mut account = Account::new(AccountId::new(id), label);
    detail.clone_into(&mut account.detail);
    account
}

/// A sample model ID ("claude:opus-5-5").
fn model_id(id: &str) -> ModelId {
    ModelId::new(id).expect("sample model IDs have no whitespace")
}

fn sample_model(
    id: &str,
    name: &str,
    account: &str,
    tiers: &[(Tier, Option<Effort>)],
    supported: &[Effort],
) -> SettingsModel {
    let mut model = SettingsModel::new(model_id(id), name, account);
    model.placements = tiers
        .iter()
        .map(|&(tier, effort)| Placement::new(tier, effort))
        .collect();
    model.supported = supported.to_vec();
    model.listed = true;
    model
}

/// An unplaced sample catalogue model (the legacy `listed` flag does not filter the catalogue).
fn unlisted(account_id: &str, account: &str, name: &str, supported: &[Effort]) -> SettingsModel {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let mut m = sample_model(
        &format!("{account_id}:{slug}"),
        name,
        account,
        &[],
        supported,
    );
    m.listed = false;
    m
}

fn sample_provider(id: ProviderId, name: &str, method: LoginMethod) -> Provider {
    Provider::new(id, name, method)
}

fn sample_models() -> Vec<SettingsModel> {
    let claude = [
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::ExtraHigh,
        Effort::Max,
    ];
    let gpt = [Effort::Low, Effort::Medium, Effort::High, Effort::ExtraHigh];
    let mut models = vec![
        sample_model(
            "claude:fable-5-1",
            "Fable 5.1",
            "Claude Max",
            &[(Tier::Premium, Some(Effort::ExtraHigh))],
            &claude,
        ),
        sample_model(
            "claude:opus-5-5",
            "Opus 5.5",
            "Claude Max",
            &[(Tier::High, Some(Effort::ExtraHigh))],
            &claude,
        ),
        sample_model(
            "chatgpt:gpt-6-sol",
            "GPT-6 Sol",
            "ChatGPT Pro",
            &[(Tier::High, Some(Effort::High))],
            &gpt,
        ),
        sample_model(
            "claude:sonnet-5-5",
            "Sonnet 5.5",
            "Claude Max",
            &[(Tier::Medium, Some(Effort::Medium))],
            &claude,
        ),
        sample_model(
            "chatgpt:gpt-5-6-luna",
            "GPT-5.6 Luna",
            "ChatGPT Pro",
            &[(Tier::Medium, Some(Effort::Medium))],
            &gpt,
        ),
    ];
    let mut newest = sample_model(
        "chatgpt:gpt-6-1-sol",
        "GPT-6.1 Sol",
        "ChatGPT Pro",
        &[],
        &gpt,
    );
    newest.new = true;
    models.push(newest);
    models.push(sample_model(
        "claude:haiku-5-5",
        "Haiku 5.5",
        "Claude Max",
        &[],
        &[Effort::Low, Effort::Medium, Effort::High],
    ));
    // 57 more: 30 + 24 + 3.
    for family in [
        "GPT-5", "GPT-5.1", "GPT-5.2", "GPT-5.3", "GPT-5.4", "GPT-5.5",
    ] {
        for variant in ["", " mini", " nano", " codex", " pro"] {
            models.push(unlisted(
                "chatgpt",
                "ChatGPT Pro",
                &format!("{family}{variant}"),
                &gpt,
            ));
        }
    }
    for family in [
        "Opus 4",
        "Opus 4.1",
        "Opus 4.5",
        "Opus 4.6",
        "Sonnet 4",
        "Sonnet 4.5",
        "Sonnet 4.6",
        "Haiku 4.5",
        "Haiku 5",
        "Opus 5",
        "Sonnet 5",
        "Fable 5",
    ] {
        for variant in ["", " (1M context)"] {
            models.push(unlisted(
                "claude",
                "Claude Max",
                &format!("{family}{variant}"),
                &claude,
            ));
        }
    }
    for name in ["Opus 5.5 (API)", "Sonnet 5.5 (API)", "Haiku 5.5 (API)"] {
        models.push(unlisted("anthropic-key", "Anthropic key", name, &claude));
    }
    models
}

/// The settings sample with no login in progress, the Anthropic key active but refused, as the
/// core reports it (its problem line, and Reconnect).
pub(crate) fn settings_broken() -> Settings {
    let mut settings = settings_with(SampleLogin::DeviceCode);
    settings.login = None;
    if let Some(key) = settings.accounts.get_mut(2) {
        key.status = AccountStatus::Active;
        key.api_key = true;
        key.provider = Some(ProviderId::Anthropic);
        key.quota_problem = Some(String::from(
            "Provider quota unavailable for API-key accounts; see the provider billing dashboard.",
        ));
        key.quota_checked_at = Some(String::from("today 12:04"));
        key.problem = Some(String::from(
            "the key was refused (401); routing skips this account until you reconnect it",
        ));
    }
    settings
}

/// The settings sample with `login` in progress.
pub(crate) fn settings_with(login: SampleLogin) -> Settings {
    let mut claude = sample_account(1, "Claude Max", "your claude login · Claude engine");
    claude.preferred = true;
    claude.follows_cli = true;
    claude.usage = Some(String::from(
        "today: 18 turns · 142k input · 24k output tokens",
    ));
    claude.quota_problem = Some(String::from(
        "Quota lookup is not supported for this provider or credential type.",
    ));
    claude.reserve_at = Some(80);
    claude.routing = Some(String::from("7-day window 35% used · reserve at 80% used"));
    let mut chatgpt = sample_account(2, "ChatGPT Pro", "Sign in with ChatGPT · provider engine");
    chatgpt.usage = Some(String::from(
        "today: 9 turns · 63k input · 8k output tokens",
    ));
    chatgpt.quota_lines = vec![
        String::from("5-hour window: 82% remaining · resets today 16:00"),
        String::from("Weekly allowance: 94% remaining · resets Mon 09:00"),
    ];
    chatgpt.quota_checked_at = Some(String::from("today 12:03"));
    chatgpt.quota_refreshing = true;
    chatgpt.reserve_at = Some(5);
    chatgpt.routing = Some(String::from(
        "7-day window 6% used · reserve at 5% used · held in reserve",
    ));
    let mut key = sample_account(3, "Anthropic key", "API key · pay per token");
    key.status = AccountStatus::Paused;
    key.routing = Some(String::from(
        "quota unknown (pay-per-token keys serve last) · no reserve mark",
    ));

    let mut claude_login = sample_provider(ProviderId::Claude, "Claude", LoginMethod::Detected);
    claude_login.connected = Some(String::from("logged in · Max"));
    let providers = vec![
        claude_login,
        sample_provider(ProviderId::ChatGpt, "ChatGPT", LoginMethod::Browser),
        sample_provider(
            ProviderId::Copilot,
            "GitHub Copilot",
            LoginMethod::DeviceCode,
        ),
        sample_provider(ProviderId::Xai, "Grok (xAI)", LoginMethod::DeviceCode),
        sample_provider(ProviderId::Kimi, "Kimi", LoginMethod::DeviceCode),
        sample_provider(ProviderId::OpenRouter, "OpenRouter", LoginMethod::Browser),
    ];

    let copilot = |status: LoginStatus| Login {
        provider: ProviderId::Copilot,
        name: String::from("GitHub Copilot"),
        waiting_for: String::from("GitHub"),
        step: LoginStep::DeviceCode {
            url: String::from("https://github.com/login/device"),
            code: String::from("WDJB-MJHT"),
        },
        status,
        expires_in: Some(Duration::from_secs(14 * 60 + 32)),
        received: None,
        account: None,
    };
    let login = match login {
        SampleLogin::DeviceCode => copilot(LoginStatus::Waiting),
        SampleLogin::Connected => copilot(LoginStatus::Connected {
            models: 36,
            starter_tiers: true,
        }),
        SampleLogin::Failed => copilot(LoginStatus::Failed {
            reason: String::from("GitHub refused the code. Try again."),
        }),
        SampleLogin::Browser => Login {
            provider: ProviderId::ChatGpt,
            name: String::from("ChatGPT"),
            waiting_for: String::from("ChatGPT"),
            step: LoginStep::Browser {
                url: String::from("https://auth.openai.com/log-in"),
            },
            status: LoginStatus::Waiting,
            expires_in: Some(Duration::from_mins(10)),
            received: None,
            account: None,
        },
    };

    Settings {
        accounts: vec![claude, chatgpt, key],
        models: sample_models(),
        pin: Pin::new(
            true,
            Some(model_id("claude:opus-5-5")),
            Some(model_id("claude:sonnet-5-5")),
        ),
        quartermaster: QuartermasterModel::Auto,
        quartermaster_effort: None,
        quartermaster_now: Some(QuartermasterNow::new(
            model_id("claude:opus-5-5"),
            Effort::High,
        )),
        providers,
        login: Some(login),
        unavailable: None,
        tier_efforts: qm_types::config::TierEfforts::default(),
        autocompact_window: qm_types::config::AutocompactWindow::default(),
        kind_tiers: sample_kind_tiers(),
    }
}

/// Every Task kinds row at its default, but review set to premium.
fn sample_kind_tiers() -> Vec<qm_types::api::KindTierRow> {
    use qm_types::domain::KindRow;
    KindRow::ALL
        .iter()
        .map(|&row| {
            let tier = if row == KindRow::Review {
                Tier::Premium
            } else {
                row.default_tier()
            };
            qm_types::api::KindTierRow::new(row, tier, row.default_tier())
        })
        .collect()
}

/// Puts the decision sample into `app`: the sample Bridge with the sample decision open in its
/// Actions popup.
pub(crate) fn install_decision(app: &mut App) {
    install(app);
    app.snapshot = decision();
    app.focus = Pane::Actions;
    app.draft.text = String::new();
    let held = decision_card().held;
    if let Some(index) = app
        .snapshot
        .actions
        .iter()
        .position(|item| item.card.held == held)
    {
        app.open_action(index);
    }
}

/// The sample decision card, as the app builds it.
#[doc(hidden)]
pub fn decision_card() -> Card {
    let mut card = Card::new(CardKind::Decision, "Decision");
    card.held = Some(HeldActionId::new(2));
    card.title = Some(String::from("C-0047 fix(ci): flaky upload test"));
    card.note = Some(String::from("· api-server · high · opus-5-5"));
    card.lines = vec![
        CardLine::new(
            0,
            vec![s(
                "Held by the app: 3 repair attempts used. The upload test still times out on the \
                 Linux runner after each fix.",
                Tone::Dim,
            )],
        ),
        CardLine::new(
            0,
            vec![s(
                "Supervisor: the timeout moved from 30s to 31s after the last change, so the fix \
                 isn't reaching the cause. The runner's /tmp is a slow network mount; that's my \
                 guess.",
                Tone::Text,
            )],
        ),
        CardLine::new(0, Vec::new()),
    ];
    let choose = |choice: HeldChoice| CardAction::Answer {
        answer: HeldAnswer::Choose {
            choice,
            words: None,
        },
    };
    card.buttons = vec![
        CardButton::new(
            "1 · Retry with my guidance",
            '1',
            choose(HeldChoice::RetryWithGuidance),
        ),
        CardButton::new(
            "2 · Try a premium model",
            '2',
            choose(HeldChoice::StrongerModel {
                tier: Tier::Premium,
                premium: true,
            }),
        ),
        CardButton::new("3 · Discard the work", '3', choose(HeldChoice::Discard)),
    ];
    card
}

/// The sample Bridge with C-0047's decision waiting in Actions.
#[doc(hidden)]
pub fn decision() -> Snapshot {
    let mut out = snapshot();
    out.actions = actions(vec![
        held_item(approval_card(), at(14, 2)),
        held_item(decision_card(), at(14, 9)),
    ]);
    out
}

/// Opens the sample through the approval card's real Diff action, including its held ID and
/// return-to-Actions state. The Bridge snapshot already carries the matching parsed diff.
pub(crate) fn install_diff(app: &mut App) {
    install(app);
    app.focus = Pane::Actions;
    let index = app
        .snapshot
        .actions
        .iter()
        .position(|item| item.card.held == Some(HeldActionId::new(1)))
        .expect("the sample approval");
    app.open_action(index);
    let button = app.snapshot.actions[index]
        .card
        .buttons
        .iter()
        .position(|button| matches!(button.action, CardAction::ShowDiff))
        .expect("the approval's Diff button");
    app.press_action_button(button, std::time::Instant::now());
}

/// A Diff screen's content for the sample's C-0039. Only changed paths are present; counts and
/// the compatibility patch are derived from these same files, never an invented summary.
#[doc(hidden)]
pub fn diff() -> qm_types::api::DiffView {
    use DiffLineKind::{Addition as A, Deletion as D};
    let mut view = qm_types::api::DiffView::new(
        HeldActionId::new(1),
        "C-0039 · feat(billing): proration preview",
    );
    let proration = diff_proration();
    let tests = diff_file(
        DiffFileStatus::Added,
        None,
        Some("tests/billing/proration.rs"),
        vec![diff_hunk(
            0,
            1,
            "",
            &[
                (A, "use api_server::billing::{Plan, preview};"),
                (A, ""),
                (A, "#[test]"),
                (A, "fn unused_days_become_credit() {"),
                (A, "    let plan = Plan::monthly(3000);"),
                (A, "    let invoice = preview(&plan, plan.midpoint());"),
                (A, "    assert_eq!(invoice.credit, 1500);"),
                (A, "    assert_eq!(invoice.days, 15);"),
                (A, "}"),
            ],
        )],
    );
    let legacy = diff_file(
        DiffFileStatus::Deleted,
        Some("src/billing/legacy.rs"),
        None,
        vec![diff_hunk(
            1,
            0,
            "",
            &[
                (D, "// Flat previews ignored unused days."),
                (D, "pub fn flat_preview(price: i64) -> i64 {"),
                (D, "    price"),
                (D, "}"),
            ],
        )],
    );
    let guide = diff_guide();
    let mut image = diff_file(
        DiffFileStatus::Added,
        None,
        Some("assets/billing/preview.png"),
        Vec::new(),
    );
    image.binary = true;
    image.metadata.push(String::from(
        "Binary files /dev/null and b/assets/billing/preview.png differ",
    ));
    let mut script = diff_file(
        DiffFileStatus::Modified,
        Some("scripts/check-billing.sh"),
        Some("scripts/check-billing.sh"),
        Vec::new(),
    );
    script.metadata.extend([
        String::from("old mode 100644"),
        String::from("new mode 100755"),
    ]);
    view.files = vec![proration, tests, legacy, guide, image, script];
    let mut additions = 0;
    let mut deletions = 0;
    for file in &view.files {
        view.lines
            .extend(file.metadata.iter().map(|line| s(line, Tone::Accent)));
        for hunk in &file.hunks {
            view.lines.push(s(&hunk.header, Tone::Accent));
            for line in &hunk.lines {
                let (prefix, tone) = match line.kind {
                    DiffLineKind::Addition => {
                        additions += 1;
                        ("+", Tone::Ok)
                    }
                    DiffLineKind::Deletion => {
                        deletions += 1;
                        ("-", Tone::Error)
                    }
                    _ => (" ", Tone::Text),
                };
                view.lines.push(s(&format!("{prefix}{}", line.text), tone));
                if line.no_newline {
                    view.lines
                        .push(s("\\ No newline at end of file", Tone::Dim));
                }
            }
        }
    }
    view.stat = format!("+{additions} −{deletions} · {} files", view.files.len());
    view
}

fn diff_proration() -> DiffFile {
    use DiffLineKind::{Addition as A, Context as C, Deletion as D};
    diff_file(
        DiffFileStatus::Modified,
        Some("src/billing/proration.rs"),
        Some("src/billing/proration.rs"),
        vec![
            diff_hunk(
                12,
                12,
                "pub fn preview(plan: &Plan, at: Date) -> Preview {",
                &[
                    (C, "    let days = plan.days_left(at);"),
                    (D, "    Preview::flat(plan.price)"),
                    (A, "    let credit = plan.unused_credit(at);"),
                    (A, "    let remaining = days.max(0);"),
                    (A, "    Preview::prorated(plan.price, credit, remaining)"),
                    (C, "}"),
                ],
            ),
            diff_hunk(
                40,
                42,
                "impl Preview {",
                &[
                    (C, "    pub fn label(&self) -> String {"),
                    (D, "        format!(\"{} due\", self.total)"),
                    (
                        A,
                        "        // Keep the invoice’s currency: € / 円, not a converted estimate.",
                    ),
                    (
                        A,
                        "\tformat!(\"{} due · credit {} · remaining {} days\", self.total, self.credit, self.days)",
                    ),
                    (C, "    }"),
                    (C, "}"),
                ],
            ),
        ],
    )
}

fn diff_guide() -> DiffFile {
    use DiffLineKind::{Addition as A, Context as C, Deletion as D};
    let mut guide = diff_file(
        DiffFileStatus::Renamed,
        Some("docs/billing/preview.md"),
        Some("docs/billing/proration.md"),
        vec![diff_hunk(
            1,
            1,
            "",
            &[
                (D, "# Billing preview"),
                (A, "# Proration preview"),
                (C, ""),
                (C, "Review the estimate before changing your plan."),
                (C, "Amounts are shown in the invoice currency."),
                (A, "Unused days appear as credit on the next invoice."),
            ],
        )],
    );
    guide.hunks[0]
        .lines
        .last_mut()
        .expect("guide's final line")
        .no_newline = true;
    guide
}

fn diff_file(
    status: DiffFileStatus,
    old: Option<&str>,
    new: Option<&str>,
    hunks: Vec<DiffHunk>,
) -> DiffFile {
    let mut file = DiffFile::new(old.map(str::to_owned), new.map(str::to_owned));
    file.status = status;
    file.metadata.push(format!(
        "diff --git a/{} b/{}",
        old.or(new).unwrap_or_default(),
        new.or(old).unwrap_or_default()
    ));
    match status {
        DiffFileStatus::Added => file.metadata.push(String::from("new file mode 100644")),
        DiffFileStatus::Deleted => file.metadata.push(String::from("deleted file mode 100644")),
        DiffFileStatus::Renamed => file.metadata.extend([
            String::from("similarity index 56%"),
            format!("rename from {}", old.unwrap_or_default()),
            format!("rename to {}", new.unwrap_or_default()),
        ]),
        _ => {}
    }
    if !hunks.is_empty() {
        file.metadata.push(format!(
            "--- {}",
            old.map_or_else(|| String::from("/dev/null"), |p| format!("a/{p}"))
        ));
        file.metadata.push(format!(
            "+++ {}",
            new.map_or_else(|| String::from("/dev/null"), |p| format!("b/{p}"))
        ));
    }
    file.hunks = hunks;
    file
}

fn diff_hunk(
    old_start: u64,
    new_start: u64,
    section: &str,
    lines: &[(DiffLineKind, &str)],
) -> DiffHunk {
    let mut old = old_start;
    let mut new = new_start;
    let lines = lines
        .iter()
        .map(|&(kind, text)| {
            let mut line = DiffLine::new(kind, text);
            if kind != DiffLineKind::Addition {
                line.old_number = Some(old);
                old += 1;
            }
            if kind != DiffLineKind::Deletion {
                line.new_number = Some(new);
                new += 1;
            }
            line
        })
        .collect();
    let (old_count, new_count) = (old - old_start, new - new_start);
    let mut hunk = DiffHunk::new(
        format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@{suffix}",
            suffix = if section.is_empty() {
                String::new()
            } else {
                format!(" {section}")
            }
        ),
        old_start,
        old_count,
        new_start,
        new_count,
    );
    hunk.lines = lines;
    hunk
}

/// A held action's item, as the core lists it: an approval is titled with its first line, a
/// decision with its card's title.
fn held_item(card: Card, at: Timestamp) -> ActionItem {
    let kind = match card.kind {
        CardKind::Approval => ActionKind::Approval,
        _ => ActionKind::Decision,
    };
    let first = card.lines.first().map(|line| {
        line.spans
            .iter()
            .map(|s| s.text.as_str())
            .collect::<String>()
    });
    let title = match kind {
        ActionKind::Approval => first,
        _ => card.title.clone(),
    }
    .unwrap_or_else(|| card.heading.clone());
    let id = card.held.unwrap_or_else(|| HeldActionId::new(0));
    ActionItem::new(
        ActionKey::Held { id },
        kind,
        title.trim(),
        at,
        card,
        ActionOpen::Conversation,
    )
}

/// The inbox the core would build from the sample: a configuration prompt, then `items` and a
/// secondmate's question, newest first.
fn actions(items: Vec<ActionItem>) -> Vec<ActionItem> {
    let mut setup = Card::new(CardKind::Notice, "Setup");
    setup.lines = vec![CardLine::new(
        0,
        vec![s(
            "GitHub isn't signed in: pull requests can't open.",
            Tone::Text,
        )],
    )];
    setup.buttons = vec![CardButton::new("Open Setup", 's', CardAction::OpenSetup)];
    let mut out = vec![ActionItem::new(
        ActionKey::Setup {
            issue: String::from("github"),
        },
        ActionKind::Setup,
        "Sign in to GitHub",
        at(13, 2),
        setup,
        ActionOpen::Setup,
    )];
    let mut rest = items;
    let mut question = Card::new(CardKind::Question, "Question");
    question.lines = vec![CardLine::new(
        0,
        vec![s(
            "Should the nightly report include the staging deploys, or production only?",
            Tone::Text,
        )],
    )];
    question.buttons = vec![CardButton::new(
        "Reply…",
        'r',
        CardAction::OpenChat {
            to: secondmate_ref(),
        },
    )];
    rest.push(
        ActionItem::new(
            ActionKey::Question {
                message: MessageId::new(90),
            },
            ActionKind::Question,
            "Staging deploys in the nightly report?",
            at(14, 20),
            question,
            ActionOpen::Session {
                agent: secondmate_ref(),
            },
        )
        .with_from(secondmate_ref()),
    );
    rest.sort_by_key(|item| std::cmp::Reverse(item.at));
    out.extend(rest);
    out
}

/// The sample Bridge with the Actions pane focused on its newest item.
pub(crate) fn install_actions(app: &mut App) {
    install(app);
    let first = app.snapshot.actions.get(1).map(|item| item.key.clone());
    app.view.selected_action = first;
    app.focus = Pane::Actions;
}

/// `seconds` before now: ages and running counts are drawn against the wall clock.
fn ago(seconds: i64) -> Timestamp {
    Timestamp::now() - jiff::SignedDuration::from_secs(seconds)
}

/// A reading of `tokens` against an auto-compact point of `point`, `seconds` old.
fn occupancy(tokens: u64, point: u64, seconds: i64) -> ContextOccupancy {
    let mut out = ContextOccupancy::default();
    out.tokens = Some(tokens);
    out.point = PointView::At(point);
    out.window = Some(200_000);
    out.model = Some(String::from("claude-sonnet-5-5"));
    out.observed_at = Some(ago(seconds));
    out.basis = ContextBasisView::LastCall;
    out
}

/// The sample's context readings: the Quartermaster after a finished turn and an automatic
/// compaction, C-0042 high and mid-turn, S-02 not read yet.
fn context() -> Vec<AgentContextView> {
    let qm_turn = TurnTokens::new(false, 1_240, 880, 3)
        .with_categories(Some(312), Some(94_000), Some(2_100))
        .with_cost(Some(String::from("$0.04")))
        .with_at(Some(ago(40)));
    let compacted = CompactionNote::new(at(13, 58), true, NoteOutcome::Done)
        .with_sizes(Some(182_000), Some(21_000), true)
        .with_took_ms(Some(14_200));
    let crew_turn = TurnTokens::new(true, 8_400, 3_100, 7).with_categories(
        Some(1_900),
        Some(121_000),
        Some(6_300),
    );
    vec![
        AgentContextView::new(AgentRef::Quartermaster, occupancy(98_000, 160_000, 40))
            .with_turn(Some(qm_turn))
            .with_notes(vec![compacted]),
        AgentContextView::new(task(42), occupancy(141_000, 160_000, 6)).with_turn(Some(crew_turn)),
        AgentContextView::new(
            secondmate_ref(),
            ContextOccupancy::unknown("waiting for the first model call"),
        ),
    ]
}

/// [`context`], with the Quartermaster compacting and C-0042's last compaction failed.
fn compacting_context() -> Vec<AgentContextView> {
    let mut views = context();
    let failed = CompactionNote::new(
        ago(300),
        true,
        NoteOutcome::Failed(String::from("the summary request timed out")),
    );
    views[0] =
        views[0]
            .clone()
            .with_compacting(Some(CompactingView::new(true, ago(12), Some(163_000))));
    views[1] = views[1].clone().with_notes(vec![failed]);
    views
}
