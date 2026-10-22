//! The question block: a ```` ```question ```` fence of TOML, placed in the section it concerns.
//!
//! ````markdown
//! ```question
//! id = "storage-engine"
//! ask = "Which storage engine should the cache use?"
//! why = "It decides the migration story in §4."
//! multiple = false
//! other = true
//! choices = [
//!   { id = "sqlite", label = "SQLite", detail = "already a dependency", recommended = true },
//!   { id = "sled",   label = "sled",   detail = "embedded, but unmaintained" },
//! ]
//! ```
//! ````
//!
//! No choices makes an open question with a text box. Errors name the line in `plan.md`.

use crate::{MAX_CHOICES, Problem};
use serde::{Deserialize, Serialize};

const MAX_ASK_CHARS: usize = 300;
const MAX_LABEL_CHARS: usize = 60;
const MAX_ID_CHARS: usize = 40;
const MAX_DETAIL_CHARS: usize = 300;
const MAX_WHY_CHARS: usize = 2_000;

/// One question, as written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    /// `[a-z0-9-]{1,40}`, unique in the plan.
    pub id: String,
    /// The question, inline Markdown.
    pub ask: String,
    /// Context and trade-offs, Markdown.
    #[serde(default)]
    pub why: Option<String>,
    /// The user may choose several.
    #[serde(default)]
    pub multiple: bool,
    /// An "Other" field beside the choices (default: on when there are choices).
    #[serde(default)]
    pub other: Option<bool>,
    #[serde(default)]
    pub choices: Vec<Choice>,
    /// The h2/h3 section the block sits in, set by the parser.
    #[serde(skip)]
    pub section: Option<String>,
}

/// One choice.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub recommended: bool,
}

impl Question {
    /// Whether the page offers an "Other" field.
    pub fn offers_other(&self) -> bool {
        !self.choices.is_empty() && self.other.unwrap_or(true)
    }
}

fn valid_id(id: &str) -> bool {
    (1..=MAX_ID_CHARS).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Parses one block whose fence opens on `line`; problems name lines in `plan.md`.
pub(crate) fn parse(body: &str, line: usize) -> Result<Question, Vec<Problem>> {
    let question: Question = toml::from_str(body).map_err(|e| {
        // The TOML error's span is a byte range in the body; its line is after the fence.
        let at = e.span().map_or(line, |span| {
            line + 1 + body[..span.start].matches('\n').count()
        });
        vec![Problem::at(
            at,
            format!("the question block isn't valid: {}", e.message()),
        )]
    })?;
    let mut problems = Vec::new();
    let mut fail = |message: String| problems.push(Problem::at(line, message));
    let name = if valid_id(&question.id) {
        question.id.clone()
    } else {
        fail(format!(
            "question id {:?} must be 1 to {MAX_ID_CHARS} of a-z, 0-9 and -",
            question.id
        ));
        "this question".to_owned()
    };
    if question.ask.trim().is_empty() {
        fail(format!("{name}: `ask` is empty"));
    }
    if question.ask.chars().count() > MAX_ASK_CHARS {
        fail(format!(
            "{name}: `ask` is over {MAX_ASK_CHARS} characters; put the context in `why`"
        ));
    }
    if question
        .why
        .as_ref()
        .is_some_and(|w| w.chars().count() > MAX_WHY_CHARS)
    {
        fail(format!("{name}: `why` is over {MAX_WHY_CHARS} characters"));
    }
    if question.choices.len() > MAX_CHOICES {
        fail(format!(
            "{name}: {} choices; at most {MAX_CHOICES}",
            question.choices.len()
        ));
    }
    if question.choices.len() == 1 {
        fail(format!(
            "{name}: one choice is no choice; give two or more, or none for an open question"
        ));
    }
    let mut seen = Vec::new();
    for choice in &question.choices {
        if !valid_id(&choice.id) {
            fail(format!(
                "{name}: choice id {:?} must be 1 to {MAX_ID_CHARS} of a-z, 0-9 and -",
                choice.id
            ));
        }
        if seen.contains(&choice.id.as_str()) {
            fail(format!("{name}: choice id {:?} is used twice", choice.id));
        }
        seen.push(&choice.id);
        if choice.label.trim().is_empty() || choice.label.chars().count() > MAX_LABEL_CHARS {
            fail(format!(
                "{name}: choice {:?} needs a label of 1 to {MAX_LABEL_CHARS} characters",
                choice.id
            ));
        }
        if choice
            .detail
            .as_ref()
            .is_some_and(|d| d.chars().count() > MAX_DETAIL_CHARS)
        {
            fail(format!(
                "{name}: choice {:?}'s detail is over {MAX_DETAIL_CHARS} characters",
                choice.id
            ));
        }
    }
    let recommended = question.choices.iter().filter(|c| c.recommended).count();
    if recommended > 1 && !question.multiple {
        fail(format!(
            "{name}: {recommended} choices are recommended; recommend one, or set multiple = true"
        ));
    }
    if question.multiple && question.choices.is_empty() {
        fail(format!("{name}: multiple = true needs choices"));
    }
    if problems.is_empty() {
        Ok(question)
    } else {
        Err(problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_good_block_parses_and_offers_other_by_default() {
        let q = parse(
            "id = \"store\"\nask = \"Which?\"\nchoices = [\n { id = \"a\", label = \"A\", recommended = true },\n { id = \"b\", label = \"B\" },\n]\n",
            10,
        )
        .expect("valid");
        assert_eq!(q.id, "store");
        assert!(q.offers_other());
        assert_eq!(q.choices.len(), 2);
    }

    #[test]
    fn errors_name_the_line_in_plan_md() {
        // The fence is on line 10; the bad value is on the block's third line, line 13.
        let e = parse("id = \"x\"\nask = \"q\"\nmultiple = \"yes\"\n", 10).unwrap_err();
        assert_eq!(e[0].line, Some(13), "{e:?}");
        let e = parse("id = \"Bad Id\"\nask = \"q\"\n", 4).unwrap_err();
        assert_eq!(e[0].line, Some(4));
        assert!(e[0].message.contains("a-z"), "{e:?}");
    }

    #[test]
    fn duplicate_choices_two_recommendations_and_unknown_keys_are_refused() {
        let e = parse(
            "id = \"x\"\nask = \"q\"\nchoices = [{ id = \"a\", label = \"A\", recommended = true }, { id = \"a\", label = \"B\", recommended = true }]\n",
            1,
        )
        .unwrap_err();
        let text: Vec<_> = e.iter().map(|p| p.message.as_str()).collect();
        assert!(text.iter().any(|m| m.contains("used twice")), "{text:?}");
        assert!(text.iter().any(|m| m.contains("recommended")), "{text:?}");
        assert!(parse("id = \"x\"\nask = \"q\"\nsurprise = 1\n", 1).is_err());
    }

    #[test]
    fn too_many_choices_are_refused() {
        let choices: Vec<String> = (0..10)
            .map(|i| format!("{{ id = \"c{i}\", label = \"C{i}\" }}"))
            .collect();
        let body = format!(
            "id = \"x\"\nask = \"q\"\nchoices = [{}]\n",
            choices.join(", ")
        );
        assert!(
            parse(&body, 1).unwrap_err()[0]
                .message
                .contains("at most 9")
        );
    }
}
