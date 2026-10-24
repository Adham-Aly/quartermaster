//! The reply the plan page builds and the user pastes into a chat:
//!
//! ```text
//! <plan-reply task="C-0042" plan="b81f0c3a9e7d2f4c6a1e5d08" round="2">
//! Reply to round 2 of the plan for C-0042 (1 of 2 questions answered).
//!
//! ## Q1 storage-engine
//! Choice: sqlite (SQLite)
//! Note:
//! > keep WAL off for now
//!
//! ## Q2 rollout
//! Left open.
//!
//! ## Section data-flow (Data flow)
//! Change:
//! > put the cache in front of the queue
//! </plan-reply>
//! ```
//!
//! - The header must match exactly; the block ends at the first line that is exactly
//!   `</plan-reply>`.
//! - Items start with `## Q<n> <question-id>` or `## Section <section-id> (<title>)`.
//! - Fields: `Choice: <id>[, <id>…] (<labels>)`, `Other:`, `Answer:`, `Note:`, `Change:`, `Ask:`,
//!   `Left open.`. A field's text follows as lines starting `> `. Every line the user typed is
//!   quoted, so nothing they type can end the block or start an item.
//! - CRLF and text around the block are tolerated; other lines inside are kept verbatim for the
//!   planner, a model, to read.
//!
//! `template/plan.js`'s `buildReply` produces exactly this; a test runs it and parses the result.

/// The opening tag's name.
pub const OPEN: &str = "<plan-reply ";
/// The closing line.
pub const CLOSE: &str = "</plan-reply>";
/// The most characters one block may hold.
pub const MAX_REPLY_CHARS: usize = 32_000;

/// One pasted reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanReply {
    /// `C-0042`.
    pub task: String,
    pub plan_uid: String,
    pub round: u32,
    pub items: Vec<Item>,
    /// The block exactly as pasted, header and closing line included, with CRLF made LF.
    pub raw: String,
    /// Where the block sits in the text it was found in (byte offsets, CRLF made LF).
    pub span: (usize, usize),
}

/// One answered question or one section note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Question {
        number: u32,
        id: String,
        fields: Vec<Field>,
    },
    Section {
        id: String,
        title: String,
        fields: Vec<Field>,
    },
}

/// A field of an item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Field {
    /// The chosen choice ids.
    Choice(Vec<String>),
    Other(String),
    Answer(String),
    Note(String),
    Change(String),
    Ask(String),
    LeftOpen,
    /// A line the grammar doesn't know, kept for the planner.
    Unknown(String),
}

impl PlanReply {
    /// Questions answered (a choice, an "Other" or an answer), questions left open, and section
    /// notes.
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut answered = 0;
        let mut open = 0;
        let mut sections = 0;
        for item in &self.items {
            match item {
                Item::Question { fields, .. } => {
                    let answers = fields.iter().any(|f| {
                        matches!(f, Field::Choice(_) | Field::Other(_) | Field::Answer(_))
                    });
                    if answers {
                        answered += 1;
                    } else {
                        open += 1;
                    }
                }
                Item::Section { .. } => sections += 1,
            }
        }
        (answered, open, sections)
    }

    /// The question ids it answers.
    pub fn question_ids(&self) -> impl Iterator<Item = &str> {
        self.items.iter().filter_map(|item| match item {
            Item::Question { id, .. } => Some(id.as_str()),
            Item::Section { .. } => None,
        })
    }
}

/// Why a block that opened couldn't be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// No closing line.
    Unclosed,
    /// Over [`MAX_REPLY_CHARS`].
    TooLong,
}

/// Every reply block in `text`, in order, and any that opened but couldn't be read.
pub fn find(text: &str) -> (Vec<PlanReply>, Vec<Malformed>) {
    let text = text.replace("\r\n", "\n");
    let mut replies = Vec::new();
    let mut malformed = Vec::new();
    let mut offset = 0;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let start = offset;
        offset += line.len();
        let Some((task, plan_uid, round)) = header(line.trim()) else {
            i += 1;
            continue;
        };
        let open = i;
        let close = (open + 1..lines.len()).find(|&j| lines[j].trim_end() == CLOSE);
        let Some(close) = close else {
            malformed.push(Malformed::Unclosed);
            break;
        };
        let end = start + lines[open..=close].iter().map(|l| l.len()).sum::<usize>();
        let raw = text[start..end].trim_end_matches('\n').to_owned();
        offset = end;
        i = close + 1;
        if raw.chars().count() > MAX_REPLY_CHARS {
            malformed.push(Malformed::TooLong);
            continue;
        }
        let body: Vec<&str> = lines[open + 1..close]
            .iter()
            .map(|l| l.trim_end_matches('\n'))
            .collect();
        replies.push(PlanReply {
            task,
            plan_uid,
            round,
            items: items(&body),
            raw,
            span: (start, end),
        });
    }
    (replies, malformed)
}

/// Parses `<plan-reply task="C-0042" plan="<24 hex>" round="2">`; a page made before crewmates had
/// names says `T-0042` for the same crewmate.
fn header(line: &str) -> Option<(String, String, u32)> {
    let rest = line.strip_prefix(OPEN)?.strip_suffix('>')?;
    let rest = rest.strip_prefix("task=\"")?;
    let (task, rest) = rest.split_once('"')?;
    let digits = task
        .strip_prefix("C-")
        .or_else(|| task.strip_prefix("T-"))?;
    if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let rest = rest.strip_prefix(" plan=\"")?;
    let (uid, rest) = rest.split_once('"')?;
    if uid.len() != 24 || !uid.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let rest = rest.strip_prefix(" round=\"")?;
    let round = rest.strip_suffix('"')?;
    if round.is_empty() || round.len() > 6 || !round.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((task.to_owned(), uid.to_owned(), round.parse().ok()?))
}

/// The items of a block's body.
fn items(body: &[&str]) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    // Inside a quoted field: whether it has had a line yet.
    let mut quoting: Option<bool> = None;
    for &line in body {
        if let (Some(fresh), Some(rest)) = (quoting, line.strip_prefix('>')) {
            let text = rest.strip_prefix(' ').unwrap_or(rest);
            if let Some(Some(
                Field::Other(t)
                | Field::Answer(t)
                | Field::Note(t)
                | Field::Change(t)
                | Field::Ask(t),
            )) = last_fields(&mut items).map(|fields| fields.last_mut())
            {
                if !fresh {
                    t.push('\n');
                }
                t.push_str(text);
            }
            quoting = Some(false);
            continue;
        }
        quoting = None;
        if let Some(item) = item_header(line) {
            items.push(item);
            continue;
        }
        let Some(fields) = last_fields(&mut items) else {
            // The summary line and blank lines before the first item.
            continue;
        };
        let field = match line.trim_end() {
            "" => continue,
            "Left open." => Field::LeftOpen,
            "Other:" => Field::Other(String::new()),
            "Answer:" => Field::Answer(String::new()),
            "Note:" => Field::Note(String::new()),
            "Change:" => Field::Change(String::new()),
            "Ask:" => Field::Ask(String::new()),
            other => match other.strip_prefix("Choice: ") {
                Some(choice) => Field::Choice(choice_ids(choice)),
                None => Field::Unknown(other.to_owned()),
            },
        };
        if matches!(
            field,
            Field::Other(_) | Field::Answer(_) | Field::Note(_) | Field::Change(_) | Field::Ask(_)
        ) {
            quoting = Some(true);
        }
        fields.push(field);
    }
    items
}

fn last_fields(items: &mut [Item]) -> Option<&mut Vec<Field>> {
    items.last_mut().map(|item| match item {
        Item::Question { fields, .. } | Item::Section { fields, .. } => fields,
    })
}

/// `## Q3 naming` or `## Section data-flow (3.2 Data flow)`.
fn item_header(line: &str) -> Option<Item> {
    let rest = line.strip_prefix("## ")?;
    if let Some(rest) = rest.strip_prefix("Section ") {
        let (id, title) = rest.split_once(' ').unwrap_or((rest, ""));
        let title = title
            .strip_prefix('(')
            .and_then(|t| t.strip_suffix(')'))
            .unwrap_or(title);
        return Some(Item::Section {
            id: id.to_owned(),
            title: title.to_owned(),
            fields: Vec::new(),
        });
    }
    let rest = rest.strip_prefix('Q')?;
    let (number, id) = rest.split_once(' ')?;
    Some(Item::Question {
        number: number.parse().ok()?,
        id: id.trim().to_owned(),
        fields: Vec::new(),
    })
}

/// `sqlite, sled (SQLite, sled)` → `["sqlite", "sled"]`.
fn choice_ids(text: &str) -> Vec<String> {
    let ids = text.split_once(" (").map_or(text, |(ids, _)| ids);
    ids.split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `text` with every reply block replaced by `with(reply)`, for the orchestrator's turn input.
pub fn replace(text: &str, with: impl Fn(&PlanReply) -> String) -> String {
    let normal = text.replace("\r\n", "\n");
    let (replies, _) = find(&normal);
    let mut out = String::with_capacity(normal.len());
    let mut at = 0;
    for reply in &replies {
        out.push_str(&normal[at..reply.span.0]);
        out.push_str(&with(reply));
        out.push('\n');
        at = reply.span.1;
    }
    out.push_str(&normal[at..]);
    out
}

/// Whether `text` holds nothing but reply blocks and whitespace.
pub fn only_replies(text: &str) -> bool {
    let normal = text.replace("\r\n", "\n");
    let (replies, malformed) = find(&normal);
    !replies.is_empty()
        && malformed.is_empty()
        && replace(&normal, |_| String::new()).trim().is_empty()
}

#[cfg(test)]
mod tests;
