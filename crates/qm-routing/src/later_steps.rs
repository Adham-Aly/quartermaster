//! Carrying your model choices through a workflow.
//!
//! "Recorded on your turn, applied by the app later. … When you do name models or effort for
//! later steps, the Quartermaster records them on the first task as typed `later_steps`, for
//! example `{kind: build, model: sonnet-5-5, effort: medium}`, and only on your turns. When a
//! later task of that kind builds on the first task, directly or further down the chain, the app
//! applies the entry itself, whatever turn starts it. The routing record cites your message. Newer
//! words of yours, or a stronger-model retry you pick, take precedence."
//!
//! Your words on the turn that starts a task replace an entry altogether: a model, an effort or
//! premium named for the task itself means no entry applies to it, neither its model nor its
//! effort. Premium covers only the task it was asked for; it reaches a later step only through an
//! entry with `premium` set, for that kind.
//!
//! Recording needs a turn you started (`qm_policy::overrides`, `OverrideKind::LaterSteps`); this
//! module validates the entries and finds the one that applies.

use qm_types::domain::{Effort, TaskKind, Tier};
use qm_types::error::Refusal;
use qm_types::ids::{MessageId, ModelId, TaskId};
use serde::{Deserialize, Serialize};

/// One `later_steps` entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaterStep {
    pub kind: TaskKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// You asked for premium for this step.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub premium: bool,
}

/// The refusal for a named model that no tier holds.
pub fn not_in_any_tier(model: &ModelId) -> Refusal {
    Refusal::new(format!(
        "{model} isn't in any model level; place it in Settings ▸ Model levels or name another model."
    ))
}

/// Checks the entries a tool call carries: each names a model, an effort or premium; no kind
/// appears twice (which one would apply would be ambiguous); a named model is placed in some tier
/// (`tiers_of` gives the tiers a model is placed in); and premium comes only with a premium model.
pub fn check_later_steps(
    steps: &[LaterStep],
    tiers_of: impl Fn(&ModelId) -> Vec<Tier>,
) -> Result<(), Refusal> {
    for (i, step) in steps.iter().enumerate() {
        if step.model.is_none() && step.effort.is_none() && !step.premium {
            return Err(Refusal::new(format!(
                "the later step for {} names neither a model, an effort nor premium",
                step.kind
            )));
        }
        if let Some(model) = &step.model {
            let tiers = tiers_of(model);
            if tiers.is_empty() {
                return Err(not_in_any_tier(model));
            }
            if step.premium && !tiers.contains(&Tier::Premium) {
                return Err(Refusal::new(format!(
                    "the later step for {} asks for premium with {model}, which isn't in the \
                     premium level; leave the model out or name a premium model",
                    step.kind
                )));
            }
        }
        if steps[..i].iter().any(|s| s.kind == step.kind) {
            return Err(Refusal::new(format!(
                "later_steps names {} twice; give one entry per kind",
                step.kind
            )));
        }
    }
    Ok(())
}

/// One task up the `builds_on` chain, with the entries recorded on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainLink<'a> {
    pub task: TaskId,
    pub later_steps: &'a [LaterStep],
    /// Your message the entries were recorded from.
    pub message: Option<MessageId>,
}

/// The entry applied to a new task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedLaterStep {
    pub from: TaskId,
    pub message: Option<MessageId>,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    pub premium: bool,
}

/// Finds the entry for a new task of `kind`. `chain` is the `builds_on` chain, nearest task
/// first ("directly or further down the chain"); the nearest entry wins, since it was recorded
/// from newer words of yours. A stronger-model retry you picked, or a model, an effort or premium
/// named for this task (`named_now`), takes precedence, so none applies to it.
pub fn applicable(
    kind: TaskKind,
    stronger_retry: bool,
    named_now: bool,
    chain: &[ChainLink<'_>],
) -> Option<AppliedLaterStep> {
    if stronger_retry || named_now {
        return None;
    }
    chain.iter().find_map(|link| {
        link.later_steps
            .iter()
            .find(|s| s.kind == kind)
            .map(|s| AppliedLaterStep {
                from: link.task,
                message: link.message,
                model: s.model.clone(),
                effort: s.effort,
                premium: s.premium,
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(m: &str) -> ModelId {
        ModelId::new(m).unwrap()
    }

    fn step(
        kind: TaskKind,
        model: Option<&str>,
        effort: Option<Effort>,
        premium: bool,
    ) -> LaterStep {
        LaterStep {
            kind,
            model: model.map(id),
            effort,
            premium,
        }
    }

    /// Fable is premium only, Opus high and premium, Sonnet medium; anything else is in no tier.
    fn tiers_of(m: &ModelId) -> Vec<Tier> {
        match m.as_str() {
            "claude-fable-5-1" => vec![Tier::Premium],
            "claude-opus-5-5" => vec![Tier::High, Tier::Premium],
            "claude-sonnet-5-5" => vec![Tier::Medium],
            _ => Vec::new(),
        }
    }

    #[test]
    fn an_entry_may_name_only_premium() {
        let steps = [step(TaskKind::Build, None, None, true)];
        assert_eq!(check_later_steps(&steps, tiers_of), Ok(()));
        let steps = [step(TaskKind::Build, Some("claude-fable-5-1"), None, true)];
        assert_eq!(check_later_steps(&steps, tiers_of), Ok(()));
        let steps = [step(TaskKind::Build, Some("claude-opus-5-5"), None, true)];
        assert_eq!(check_later_steps(&steps, tiers_of), Ok(()));
    }

    #[test]
    fn an_empty_entry_is_refused() {
        let steps = [step(TaskKind::Build, None, None, false)];
        let refusal = check_later_steps(&steps, tiers_of).unwrap_err();
        assert!(
            refusal
                .to_string()
                .contains("neither a model, an effort nor premium")
        );
    }

    #[test]
    fn premium_with_a_model_outside_premium_is_refused() {
        let steps = [step(
            TaskKind::Review,
            Some("claude-sonnet-5-5"),
            None,
            true,
        )];
        let refusal = check_later_steps(&steps, tiers_of).unwrap_err();
        assert!(
            refusal.to_string().contains("isn't in the premium level"),
            "{refusal}"
        );
    }

    #[test]
    fn a_model_in_no_tier_is_refused() {
        let steps = [step(TaskKind::Build, Some("claude-nowhere-1"), None, false)];
        assert_eq!(
            check_later_steps(&steps, tiers_of),
            Err(not_in_any_tier(&id("claude-nowhere-1")))
        );
    }

    #[test]
    fn a_kind_named_twice_is_refused() {
        let steps = [
            step(TaskKind::Build, None, Some(Effort::Low), false),
            step(TaskKind::Build, None, None, true),
        ];
        assert!(check_later_steps(&steps, tiers_of).is_err());
    }

    #[test]
    fn premium_is_left_out_of_the_stored_form_when_unset() {
        let plain = step(TaskKind::Build, None, Some(Effort::Low), false);
        let text = toml::to_string(&plain).unwrap();
        assert!(!text.contains("premium"), "{text}");
        let old: LaterStep = toml::from_str("kind = \"build\"\neffort = \"low\"\n").unwrap();
        assert_eq!(old, plain);
        let premium = step(TaskKind::Build, None, None, true);
        let back: LaterStep = toml::from_str(&toml::to_string(&premium).unwrap()).unwrap();
        assert_eq!(back, premium);
    }

    #[test]
    fn the_nearest_entry_applies_with_its_premium_unless_this_turn_named_something() {
        let first = [step(
            TaskKind::Review,
            Some("claude-sonnet-5-5"),
            None,
            false,
        )];
        let near = [step(TaskKind::Review, None, None, true)];
        let chain = [
            ChainLink {
                task: TaskId::new(2).unwrap(),
                later_steps: &near,
                message: None,
            },
            ChainLink {
                task: TaskId::new(1).unwrap(),
                later_steps: &first,
                message: None,
            },
        ];
        let applied = applicable(TaskKind::Review, false, false, &chain).unwrap();
        assert_eq!(applied.from, TaskId::new(2).unwrap());
        assert!(applied.premium);
        assert_eq!(applied.model, None);
        assert_eq!(applicable(TaskKind::Review, false, true, &chain), None);
        assert_eq!(applicable(TaskKind::Review, true, false, &chain), None);
        assert_eq!(applicable(TaskKind::Build, false, false, &chain), None);
    }
}
