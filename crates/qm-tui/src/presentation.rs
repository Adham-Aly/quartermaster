//! Display-only labels for app-owned metadata. Never run this over a snapshot, authored prose,
//! tool output, a draft, or a command payload. Call before wrapping/measuring/selecting text.

use qm_types::api::{ActionItem, AgentRef, Card, CardKind, ChatLine, Snapshot, Tone};
use qm_types::domain::MessageSource;
use qm_types::ids::TaskId;

pub(crate) struct Presenter<'a>(pub(crate) &'a Snapshot);

impl<'a> Presenter<'a> {
    fn crewmate(&self, task: TaskId) -> Option<&'a str> {
        self.0
            .actions
            .iter()
            .find_map(|item| {
                (item.task == Some(task) || item.from == Some(AgentRef::Crewmate { task }))
                    .then_some(item.from_label.as_deref())
                    .flatten()
                    .filter(|name| !name.is_empty())
            })
            .or_else(|| {
                self.0
                    .crew
                    .iter()
                    .find(|row| row.agent == AgentRef::Crewmate { task })
                    .and_then(|row| row.task.as_ref())
                    .map(|task| task.name.as_str())
                    .filter(|name| !name.is_empty())
            })
    }

    /// Source labels use retained names first, including reports no longer in the live crew.
    pub(crate) fn source(&self, item: &ActionItem) -> Option<String> {
        if let Some(name) = item.from_label.as_ref().filter(|name| !name.is_empty()) {
            return Some(name.clone());
        }
        let task = item.task.or(match item.from {
            Some(AgentRef::Crewmate { task }) => Some(task),
            _ => None,
        });
        if let Some(task) = task {
            return Some(self.crewmate(task).unwrap_or("Crewmate").to_owned());
        }
        item.from.map(crate::session::draw::agent_label)
    }

    fn reference(&self, prefix: u8, number: u32) -> &str {
        if prefix == b'T' {
            if let Some(name) = self
                .0
                .crew
                .iter()
                .filter_map(|row| row.task.as_ref())
                .find(|task| task.id.get() == number && !task.name.is_empty())
                .map(|task| task.name.as_str())
            {
                return name;
            }
            // Older app events used T-n for the crewmate now called C-n.
            return TaskId::new(number)
                .and_then(|task| self.crewmate(task))
                .unwrap_or("task");
        }
        TaskId::new(number)
            .and_then(|task| self.crewmate(task))
            .unwrap_or("crewmate")
    }

    /// Standalone references only. Paths, file names, inline/fenced code and larger identifiers
    /// are technical content, even in an app-generated error, and must remain byte-for-byte.
    pub(crate) fn metadata(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut code = false;
        for token in text.split_inclusive(char::is_whitespace) {
            let path = token.contains(['/', '\\']);
            let bytes = token.as_bytes();
            let mut copied = 0;
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'`' {
                    // A backtick run (including a fenced block) opens/closes one code region.
                    code = !code;
                    i += 1;
                    while i < bytes.len() && bytes[i] == b'`' {
                        i += 1;
                    }
                    continue;
                }
                if !code
                    && !path
                    && matches!(bytes[i], b'T' | b'C')
                    && bytes.get(i + 1) == Some(&b'-')
                    && (i == 0 || !identifier(bytes[i - 1]))
                {
                    let mut end = i + 2;
                    while end < bytes.len() && bytes[end].is_ascii_digit() {
                        end += 1;
                    }
                    let file_suffix = bytes.get(end) == Some(&b'.')
                        && bytes.get(end + 1).is_some_and(u8::is_ascii_alphanumeric);
                    if end > i + 2
                        && !file_suffix
                        && bytes.get(end).is_none_or(|b| !identifier(*b))
                        && let Ok(number) = token[i + 2..end].parse::<u32>()
                        && number > 0
                    {
                        out.push_str(&token[copied..i]);
                        out.push_str(self.reference(bytes[i], number));
                        copied = end;
                        i = end;
                        continue;
                    }
                }
                i += 1;
            }
            out.push_str(&token[copied..]);
        }
        out
    }

    pub(crate) fn chat(&self, chat: &ChatLine) -> ChatLine {
        let mut shown = chat.clone();
        if chat.acknowledgement_by_app {
            shown.acknowledgement = chat.acknowledgement.as_deref().map(|s| self.metadata(s));
        }
        shown.context = chat.context.as_deref().map(|s| self.metadata(s));
        if chat.source == MessageSource::App {
            shown.text = self.metadata(&chat.text);
        }
        shown
    }

    /// Only app errors/statuses, never the authored heading/title/body/options or their payloads.
    pub(crate) fn card(&self, card: &Card) -> Card {
        let mut shown = card.clone();
        for line in &mut shown.lines {
            if line.verbatim {
                continue;
            }
            for span in &mut line.spans {
                if span.tone == Tone::Error {
                    span.text = self.metadata(&span.text);
                }
            }
        }
        if card.kind == CardKind::Notice {
            shown.note = card.note.as_deref().map(|s| self.metadata(s));
        }
        if let Some(answered) = &card.answered
            && (answered.starts_with("No longer applies") || answered.starts_with("Superseded"))
        {
            shown.answered = Some(self.metadata(answered));
        }
        shown
    }
}

fn identifier(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') || !byte.is_ascii()
}

#[cfg(test)]
mod tests {
    use super::*;
    use qm_types::api::{
        ActionKey, ActionKind, ActionOpen, CardAction, CardButton, CardLine, CrewRow, CrewTask,
        Styled,
    };
    use qm_types::ids::TaskLabelId;

    #[test]
    fn metadata_has_friendly_names_without_touching_paths_code_or_identifier_substrings() {
        let task = TaskId::new(42).unwrap();
        let mut snapshot = Snapshot::default();
        let mut row = CrewRow::new(AgentRef::Crewmate { task }, 1, "C-0042 fix", "idle");
        row.task = Some(
            CrewTask::new(TaskLabelId::new(17).unwrap(), "T-0017 project")
                .named("Token refresh", "project"),
        );
        snapshot.crew.push(row);
        let p = Presenter(&snapshot);
        assert_eq!(
            p.metadata("after C-0042 reported · T-0017; C-99's T-98."),
            "after Token refresh reported · Token refresh; crewmate's task."
        );
        let technical = "tasks/C-0042/errors artifacts/T-0017/C-0042 C-0042.md C-0042.log:3 C-0042_suffix preC-0042 C-0042-more `C-0042` ```\nT-0017\n```";
        assert_eq!(p.metadata(technical), technical);
        assert_eq!(
            p.metadata("C-0042, café (T-0017)"),
            "Token refresh, café (Token refresh)"
        );
    }

    #[test]
    fn presentation_does_not_rewrite_authored_cards_or_commands() {
        let snapshot = Snapshot::default();
        let mut card = Card::new(CardKind::Question, "Authored C-0042");
        card.title = Some("Keep T-0017".into());
        card.lines = vec![CardLine::new(
            0,
            vec![
                Styled::new("raw C-0042 T-0017", Tone::Text),
                Styled::new("C-0042 missing in tasks/C-0042", Tone::Error),
            ],
        )];
        card.buttons.push(CardButton::new(
            "1 · C-0042",
            '1',
            CardAction::Reply {
                text: "C-0042 T-0017".into(),
            },
        ));
        card.answered = Some("Answered · C-0042".into());
        let shown = Presenter(&snapshot).card(&card);
        assert_eq!(shown.heading, card.heading);
        assert_eq!(shown.title, card.title);
        assert_eq!(shown.buttons, card.buttons);
        assert_eq!(shown.answered, card.answered);
        assert_eq!(shown.lines[0].spans[0], card.lines[0].spans[0]);
        assert_eq!(
            shown.lines[0].spans[1].text,
            "crewmate missing in tasks/C-0042"
        );
    }

    #[test]
    fn held_failure_provenance_preserves_external_errors_but_formats_refusals() {
        let mut app = app_with_metadata();
        let diagnostic = "fatal: bad revision 'C-0042'";
        let mut external = CardLine::new(2, vec![Styled::new(diagnostic, Tone::Error)]);
        external.verbatim = true;
        let card = &mut app.snapshot.actions[0].card;
        card.kind = CardKind::Decision;
        card.held = Some(qm_types::ids::HeldActionId::new(7));
        card.answered = None;
        card.buttons.push(CardButton::new(
            "Retry",
            'r',
            CardAction::Answer {
                answer: qm_types::api::HeldAnswer::Choose {
                    choice: qm_types::api::HeldChoice::RetryWithGuidance,
                    words: Some("Keep C-0042 unchanged".into()),
                },
            },
        ));
        card.lines = vec![
            external,
            CardLine::new(
                2,
                vec![Styled::new("C-0042 is no longer ready", Tone::Error)],
            ),
        ];
        let original = app.snapshot.clone();
        let card = &app.snapshot.actions[0].card;
        let shown = Presenter(&app.snapshot).card(card);
        assert_eq!(shown.lines[0], card.lines[0]);
        assert_eq!(
            shown.lines[1].spans[0].text,
            "Token refresh is no longer ready"
        );
        assert_eq!(shown.held, card.held);
        assert_eq!(shown.buttons, card.buttons);
        app.open_action(0);
        let popup = frame(&mut app);
        assert!(popup.contains(diagnostic), "{popup}");
        assert!(
            popup.contains("Token refresh is no longer ready"),
            "{popup}"
        );
        assert_eq!(app.snapshot, original);
    }

    fn frame(app: &mut crate::app::App) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(170, 48)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..48)
            .map(|y| {
                (0..170)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn app_with_metadata() -> crate::app::App {
        use qm_types::domain::MessageKind;
        use qm_types::ids::MessageId;
        let mut app = crate::app::App::new();
        let task = TaskId::new(42).unwrap();
        let at = jiff::Timestamp::UNIX_EPOCH;
        let mut user = ChatLine::new(
            MessageId::new(1),
            MessageSource::You,
            MessageKind::Message,
            at,
            "please fix this",
        );
        user.acknowledgement = Some("Started: C-0042".into());
        user.acknowledgement_by_app = true;
        let mut agent = ChatLine::new(
            MessageId::new(2),
            MessageSource::Agent,
            MessageKind::Reply,
            at,
            "Ready for review.",
        );
        agent.context = Some("after T-0017 reported".into());
        app.snapshot.conversation = vec![
            user,
            agent,
            ChatLine::new(
                MessageId::new(3),
                MessageSource::App,
                MessageKind::Message,
                at,
                "C-0042 reported for T-0017",
            ),
        ];
        let mut row = CrewRow::new(AgentRef::Crewmate { task }, 1, "C-0042 fix", "idle");
        row.task = Some(
            CrewTask::new(TaskLabelId::new(17).unwrap(), "T-0017 project")
                .named("Token refresh", "project"),
        );
        app.snapshot.crew.push(row);
        let mut card = Card::new(CardKind::Notice, "Something went wrong");
        card.note = Some("C-0042 failed for T-0017".into());
        card.lines.push(CardLine::new(
            0,
            vec![Styled::new("Cannot answer C-0042 for T-0017", Tone::Error)],
        ));
        card.answered = Some("No longer applies: C-0042 left T-0017".into());
        app.snapshot.actions.push(
            ActionItem::new(
                ActionKey::Report { task },
                ActionKind::Report,
                "Report ready",
                at,
                card,
                ActionOpen::Conversation,
            )
            .with_task(task)
            .with_from(AgentRef::Crewmate { task })
            .with_from_label("Token refresh"),
        );
        app.flash("Cannot open C-0042 for T-0017", std::time::Instant::now());
        app
    }

    #[test]
    fn full_frames_format_app_metadata_before_selection_and_popup_geometry() {
        let mut app = app_with_metadata();
        let original = app.snapshot.clone();
        let shown = frame(&mut app);
        assert!(shown.contains("Started: Token refresh"), "{shown}");
        assert!(shown.contains("after Token refresh reported"), "{shown}");
        assert!(
            shown.contains("Cannot open Token refresh for Token refresh"),
            "{shown}"
        );
        assert!(
            !shown.contains("C-0042") && !shown.contains("T-0017"),
            "{shown}"
        );
        assert!(app.term.drawn.text.iter().any(|row| {
            row.text
                .contains("Token refresh reported for Token refresh")
        }));
        app.open_action(0);
        let popup = frame(&mut app);
        assert!(
            popup.contains("Cannot answer Token refresh for Token refresh"),
            "{popup}"
        );
        assert!(
            popup.contains("No longer applies: Token refresh left Token refresh"),
            "{popup}"
        );
        assert!(
            !popup.contains("C-0042") && !popup.contains("T-0017"),
            "{popup}"
        );
        assert_eq!(app.snapshot, original);
    }

    #[test]
    fn full_frames_preserve_authored_acknowledgements() {
        let mut app = app_with_metadata();
        let authored = "Noted C-0042 and T-0017 exactly as requested";
        app.snapshot.conversation[0].acknowledgement = Some(authored.into());
        app.snapshot.conversation[0].acknowledgement_by_app = false;
        let original = app.snapshot.clone();
        let shown = frame(&mut app);
        assert!(shown.contains(authored), "{shown}");
        assert_eq!(
            Presenter(&app.snapshot).chat(&app.snapshot.conversation[0]),
            app.snapshot.conversation[0]
        );
        assert_eq!(app.snapshot, original);
    }

    #[test]
    fn full_frames_keep_authored_prose_paths_options_drafts_and_reply_payloads() {
        use qm_types::api::{Command, SessionLine, SessionLineKind, SessionView};
        use qm_types::domain::MessageKind;
        use qm_types::ids::MessageId;
        let mut app = app_with_metadata();
        let at = jiff::Timestamp::UNIX_EPOCH;
        let authored = "Authored C-0081 and T-0082";
        for (i, source) in [
            MessageSource::You,
            MessageSource::Agent,
            MessageSource::Supervisor,
        ]
        .into_iter()
        .enumerate()
        {
            app.snapshot.conversation.push(ChatLine::new(
                MessageId::new(10 + i64::try_from(i).unwrap()),
                source,
                MessageKind::Message,
                at,
                authored,
            ));
        }
        app.snapshot.conversation.push(ChatLine::new(
            MessageId::new(15),
            MessageSource::App,
            MessageKind::Message,
            at,
            "C-0042 failed at artifacts/T-0017/C-0042/report.md",
        ));
        app.set_input("Editable C-0081 T-0082".into());
        let shown = frame(&mut app);
        assert_eq!(shown.matches(authored).count(), 3, "{shown}");
        assert!(
            shown.contains("artifacts/T-0017/C-0042/report.md"),
            "{shown}"
        );
        assert!(shown.contains("Editable C-0081 T-0082"), "{shown}");
        let item = &mut app.snapshot.actions[0];
        item.title = authored.into();
        item.card = Card::new(CardKind::Question, "Question");
        item.card.title = Some(authored.into());
        item.card.lines.push(CardLine::new(
            0,
            vec![Styled::new("+code C-0081 T-0082", Tone::Text)],
        ));
        item.card.buttons.push(CardButton::new(
            "1 · C-0081 T-0082",
            '1',
            CardAction::Reply {
                text: authored.into(),
            },
        ));
        let original = app.snapshot.clone();
        app.open_action(0);
        let popup = frame(&mut app);
        assert!(popup.contains(authored), "{popup}");
        assert!(popup.contains("+code C-0081 T-0082"), "{popup}");
        assert_eq!(app.snapshot, original);
        app.take_commands();
        app.press_action_button(0, std::time::Instant::now());
        assert_eq!(
            app.take_commands(),
            vec![Command::Send {
                to: AgentRef::Crewmate {
                    task: TaskId::new(42).unwrap()
                },
                text: authored.into(),
                images: vec![]
            }]
        );
        app.close_action();
        let agent = AgentRef::Crewmate {
            task: TaskId::new(42).unwrap(),
        };
        let mut session = SessionView::new(agent);
        session.open = true;
        session.lines.push(SessionLine::new(
            1,
            SessionLineKind::Tool,
            at,
            "RAW C-0081 T-0082 artifacts/T-0017/C-0042",
        ));
        app.snapshot.session = Some(session);
        app.enter_session(agent);
        app.toggle_tool_group(1);
        let shown = frame(&mut app);
        assert!(
            shown.contains("RAW C-0081 T-0082 artifacts/T-0017/C-0042"),
            "{shown}"
        );
    }

    #[test]
    fn copied_plan_reply_protocol_and_authored_diffs_are_not_metadata() {
        use qm_types::domain::MessageKind;
        use qm_types::ids::MessageId;
        let snapshot = Snapshot::default();
        let protocol =
            "Plan reply · C-0042 · round 2\nT-0017: approve\n```diff\n-C-0042\n+T-0017\n```";
        for source in [
            MessageSource::You,
            MessageSource::Agent,
            MessageSource::Supervisor,
        ] {
            let chat = ChatLine::new(
                MessageId::new(1),
                source,
                MessageKind::Message,
                jiff::Timestamp::UNIX_EPOCH,
                protocol,
            );
            assert_eq!(Presenter(&snapshot).chat(&chat), chat);
        }
    }

    #[test]
    fn retained_source_survives_a_closed_task_and_empty_names_never_fall_back_to_ids() {
        let task = TaskId::new(47).unwrap();
        let mut snapshot = Snapshot::default();
        let item = ActionItem::new(
            ActionKey::Report { task },
            ActionKind::Report,
            "Authored C-0047",
            jiff::Timestamp::UNIX_EPOCH,
            Card::new(CardKind::Report, "Report ready"),
            ActionOpen::Conversation,
        )
        .with_task(task)
        .with_from(AgentRef::Crewmate { task })
        .with_from_label("Upload retry");
        snapshot.actions.push(item.clone());
        assert_eq!(
            Presenter(&snapshot).source(&item).as_deref(),
            Some("Upload retry")
        );
        assert_eq!(
            Presenter(&snapshot).metadata("C-0047 reported"),
            "Upload retry reported"
        );
        snapshot.actions.clear();
        let mut old = item;
        old.from_label = Some(String::new());
        assert_eq!(
            Presenter(&snapshot).source(&old).as_deref(),
            Some("Crewmate")
        );
    }
}
