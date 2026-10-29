//! Display-only labels for app-owned metadata. Never run this over a snapshot, authored prose,
//! tool output, a draft, or a command payload. Call before wrapping/measuring/selecting text.

use qm_types::api::{ActionItem, AgentRef, Card, CardKind, ChatLine, Snapshot, Tone};
use qm_types::domain::MessageSource;
use qm_types::ids::{SecondmateId, TaskId};

pub(crate) struct Presenter<'a>(pub(crate) &'a Snapshot);

impl<'a> Presenter<'a> {
    fn crewmate(&self, task: TaskId) -> Option<&'a str> {
        self.0
            .actions
            .iter()
            .find_map(|item| {
                (!matches!(item.from, Some(AgentRef::Secondmate { .. }))
                    && (item.task == Some(task) || item.from == Some(AgentRef::Crewmate { task })))
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

    fn secondmate(&self, id: SecondmateId) -> Option<&'a str> {
        let agent = AgentRef::Secondmate { id };
        self.0
            .actions
            .iter()
            .filter(|item| item.from == Some(agent))
            .find_map(|item| item.from_label.as_deref().filter(|s| !s.is_empty()))
            .or_else(|| {
                self.0
                    .crew
                    .iter()
                    .find(|row| row.agent == agent)
                    .map(|row| row.label.as_str())
                    .filter(|s| !s.is_empty())
            })
            .or_else(|| {
                self.0
                    .session
                    .as_ref()
                    .filter(|view| view.agent == agent)
                    .map(|view| view.title.as_str())
                    .filter(|s| !s.is_empty())
            })
    }

    /// Friendly display labels only; the original `AgentRef` still routes every action.
    pub(crate) fn agent(&self, agent: AgentRef) -> String {
        match agent {
            AgentRef::Crewmate { task } => self.crewmate(task).unwrap_or("Crewmate").to_owned(),
            AgentRef::Secondmate { id } => self.secondmate(id).unwrap_or("Secondmate").to_owned(),
            AgentRef::Quartermaster => "Quartermaster".to_owned(),
        }
    }

    /// Source labels use retained names first, including reports no longer in the live crew.
    pub(crate) fn source(&self, item: &ActionItem) -> Option<String> {
        if let Some(name) = item.from_label.as_ref().filter(|name| !name.is_empty()) {
            return Some(name.clone());
        }
        if let Some(agent @ AgentRef::Secondmate { .. }) = item.from {
            return Some(self.agent(agent));
        }
        let task = item.task.or(match item.from {
            Some(AgentRef::Crewmate { task }) => Some(task),
            _ => None,
        });
        if let Some(task) = task {
            return Some(self.crewmate(task).unwrap_or("Crewmate").to_owned());
        }
        item.from.map(|agent| self.agent(agent))
    }

    fn reference(&self, prefix: u8, number: u32) -> &str {
        if prefix == b'S' {
            return SecondmateId::new(number)
                .and_then(|id| self.secondmate(id))
                .unwrap_or("secondmate");
        }
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
                    && matches!(bytes[i], b'T' | b'C' | b'S')
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

    /// Failure reasons in these generated templates are opaque engine diagnostics. Only the
    /// wake summary before the delimiter is app-owned metadata.
    pub(crate) fn app_message(&self, text: &str) -> String {
        if text.starts_with("The turn stopped early: ") {
            return text.to_owned();
        }
        if let Some(rest) = text.strip_prefix("A turn after ")
            && let Some((short, reason)) = rest.split_once(" stopped: ")
        {
            return format!("A turn after {} stopped: {reason}", self.metadata(short));
        }
        self.metadata(text)
    }

    pub(crate) fn chat(&self, chat: &ChatLine) -> ChatLine {
        let mut shown = chat.clone();
        if chat.acknowledgement_by_app {
            shown.acknowledgement = chat.acknowledgement.as_deref().map(|s| self.metadata(s));
        }
        shown.context = chat.context.as_deref().map(|s| self.metadata(s));
        if chat.source == MessageSource::App {
            shown.text = self.app_message(&chat.text);
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
        if card.kind == CardKind::Notice && !card.note_verbatim {
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
