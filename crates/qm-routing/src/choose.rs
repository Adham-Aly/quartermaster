//! Routing, step by step: facts in, one recorded decision out.
//!
//! [`route`] runs the whole procedure as one plain function over the declared facts, the pool and
//! your settings, and returns the [`RoutingRecord`] that goes into `routing_decisions`. The order
//! of precedence, highest first:
//!
//! 1. A model you named for this task in your own words: "If you named a model for this task in
//!    your own words, that comes first." If the named model cannot serve, the task waits rather
//!    than substituting another model.
//! 2. A `later_steps` entry recorded on your turn. Your words for this task (a model, an effort or
//!    premium) replace the entry altogether, and so does a stronger-model retry you pick.
//! 3. A stronger-model retry or premium, which you asked for: premium sits above high and is
//!    used only when you ask for it, choose "try again with a stronger model", or set a task kind
//!    to it (step 5).
//! 4. The "always use" pin: "'Always use' stays as a setting on the pool, with its fallback model
//!    and a notice while it's on."
//! 5. The tier of the task's row in the rule table (yours or the default), with fallbacks and
//!    ranking.
//!
//! # Premium
//!
//! Premium is asked for in one of four ways: the `premium` flag, naming a model placed in premium
//! (for this task or in its `later_steps` entry), an entry with `premium` set, or a stronger-model
//! retry whose target is premium. An explicitly named model must serve or the task waits. Without
//! a named model, every premium model is ranked as usual, then high → medium → low
//! ([`fallback::order`]). The pin and the rule table never come first. When premium runs on another
//! tier, the routing line names each premium model that couldn't serve and why, and the record
//! has the fallback step.
//!
//! A `Decider` failure routes high: "if the call fails, times out or can't decide, the task routes
//! high. It never blocks a task".

use crate::decider::{Decided, DecidedBy};
use crate::effort::{EffortInput, choose_effort, tier_default};
use crate::fallback;
use crate::later_steps::AppliedLaterStep;
use crate::rank::{Candidate, Needs, placement_skip_reason, rank, skip_reason};
use crate::record::{
    BuildsOn, BuildsOnFact, DeclaredFacts, FallbackStep, RoutedChoice, RoutingRecord, SkippedModel,
    UserChoices,
};
use crate::rules::{Matched, RuleTable};
use qm_types::config::{AlwaysUse, TierEfforts};
use qm_types::domain::{Effort, KindRow, TaskKind, Tier};
use qm_types::ids::{AccountId, ModelId};

/// "Skipped when the outcome is already decided: you named a model, you asked for premium, or the
/// task is a stronger-model retry. It runs at most once per task".
pub fn decider_needed(user: &UserChoices) -> bool {
    user.model.is_none() && !user.premium && !user.stronger_retry
}

/// Everything routing reads for one task.
#[derive(Clone, Debug)]
pub struct RouteRequest<'a> {
    /// The decider's answer about the kind, or the declared kind as an answer when the decider was
    /// skipped ([`decider_needed`]).
    pub kind: Decided<TaskKind>,
    /// Who was asked, recorded when the answer is `Undecided`.
    pub decider: DecidedBy,
    /// What the work builds on, with the fact the app checked ([`crate::rules::builds_on_fact`]).
    pub builds_on: Option<BuildsOn>,
    pub needs: Needs,
    /// The task runs at the `quick` verification level.
    pub quick_verification: bool,
    /// Your words for this task, each already accepted by the override check.
    pub user: UserChoices,
    /// A stronger-model retry's target tier, worked out by `qm_policy::holds`. Set exactly when
    /// `user.stronger_retry` is.
    pub stronger_retry_target: Option<Tier>,
    /// The `later_steps` entry that applies ([`crate::later_steps::applicable`]).
    pub later_step: Option<AppliedLaterStep>,
    pub always_use: Option<&'a AlwaysUse>,
    pub preferred_account: Option<AccountId>,
    pub tiers: &'a TierEfforts,
    /// Every model placed in premium, whether or not it can serve now (an account whose login
    /// stopped working is left out of the pool, but naming its premium model still asks for
    /// premium). Models placed in premium in `Pool::placed` count too.
    pub premium_models: &'a [ModelId],
}

/// The pool routing picks from.
#[derive(Clone, Copy, Debug)]
pub struct Pool<'a> {
    /// Every placement: (model, account) pairs in tiers.
    pub placed: &'a [Candidate],
    /// Pairs for the "always use" model and its fallback on each account that serves them, which
    /// need not be placed in a tier ("ignoring tiers"). Their `tier` and `effort_override` are ignored.
    pub pinned: &'a [Candidate],
}

struct Picked<'a> {
    candidate: &'a Candidate,
}

fn pick<'a>(
    candidates: impl Iterator<Item = &'a Candidate>,
    needs: Needs,
    preferred: Option<AccountId>,
    skipped: &mut Vec<SkippedModel>,
    check: fn(&Candidate, Needs) -> Option<String>,
) -> Option<Picked<'a>> {
    let mut ok: Vec<&Candidate> = Vec::new();
    for c in candidates {
        match check(c, needs) {
            Some(reason) => {
                if !skipped
                    .iter()
                    .any(|s| s.model == c.model && s.account == c.account)
                {
                    skipped.push(SkippedModel {
                        model: c.model.clone(),
                        account: c.account,
                        reason,
                    });
                }
            }
            None => ok.push(c),
        }
    }
    rank(&mut ok, preferred);
    ok.first().map(|c| Picked { candidate: c })
}

/// The one plain-language line for the row the rule table read.
fn rule_line(kind: TaskKind, builds_on: Option<&BuildsOn>, matched: Matched) -> String {
    let tier = matched.tier;
    match (matched.row, builds_on) {
        (KindRow::BuildOnFinishedPlan, Some(b)) => {
            format!("Building from the plan in task {}", b.task)
        }
        (KindRow::FixOnConfirmedDiagnosis, Some(b)) => {
            format!("Building on the confirmed diagnosis in task {}", b.task)
        }
        _ if matched.yours => format!("You set {kind} work to route {tier}"),
        _ => match kind {
            TaskKind::Lookup => "Lookups run on low".to_owned(),
            TaskKind::Unclear => "Unclear work always routes high".to_owned(),
            _ => format!("{kind} work routes {tier}"),
        },
    }
}

/// One routing decision in progress: the context every step reads, and what the steps record.
struct Router<'r, 'a> {
    req: &'r RouteRequest<'a>,
    pool: Pool<'r>,
    kind: TaskKind,
    matched: Matched,
    retry_target: Option<Tier>,
    later_step: Option<AppliedLaterStep>,
    /// You asked for premium, in any of the four ways (module docs).
    premium_asked: bool,
    tier_landed: Tier,
    skipped: Vec<SkippedModel>,
    fallbacks: Vec<FallbackStep>,
    notes: Vec<String>,
}

impl<'r, 'a> Router<'r, 'a> {
    fn new(req: &'r RouteRequest<'a>, pool: Pool<'r>, kind: TaskKind, matched: Matched) -> Self {
        let retry_target = if req.user.stronger_retry {
            // A retry with no computed target can't climb; it goes to the strongest tier.
            Some(req.stronger_retry_target.unwrap_or(Tier::Premium))
        } else {
            None
        };
        // Your words for this task replace a later step's entry altogether.
        let named_now = req.user.model.is_some() || req.user.effort.is_some() || req.user.premium;
        let later_step = if retry_target.is_some() || named_now {
            None
        } else {
            req.later_step.clone()
        };
        let named = req
            .user
            .model
            .as_ref()
            .or_else(|| later_step.as_ref().and_then(|l| l.model.as_ref()));
        let premium_asked = req.user.premium
            || later_step.as_ref().is_some_and(|l| l.premium)
            || named.is_some_and(|m| is_premium_model(req, pool, m));
        let tier_landed = match retry_target {
            Some(t) => t,
            None if premium_asked => Tier::Premium,
            None => matched.tier,
        };
        let mut notes = Vec::new();
        if let Decided::Undecided { .. } = req.kind {
            notes.push(format!(
                "The kind couldn't be decided, so it routes as unclear ({})",
                matched.tier
            ));
        }
        Self {
            req,
            pool,
            kind,
            matched,
            retry_target,
            later_step,
            premium_asked: premium_asked && retry_target.is_none(),
            tier_landed,
            skipped: Vec::new(),
            fallbacks: Vec::new(),
            notes,
        }
    }

    fn named_effort(&self) -> Option<Effort> {
        self.req
            .user
            .effort
            .or(self.later_step.as_ref().and_then(|l| l.effort))
    }

    fn placement_check(&self) -> fn(&Candidate, Needs) -> Option<String> {
        // Your task/later-step effort deliberately replaces the combo and retains its existing
        // cap-to-supported semantics. Otherwise the account must run the selected combo exactly.
        if self.named_effort().is_some() {
            skip_reason
        } else {
            placement_skip_reason
        }
    }

    fn pick_from<'c>(
        &mut self,
        candidates: impl Iterator<Item = &'c Candidate>,
        check: fn(&Candidate, Needs) -> Option<String>,
    ) -> Option<&'c Candidate> {
        pick(
            candidates,
            self.req.needs,
            self.req.preferred_account,
            &mut self.skipped,
            check,
        )
        .map(|p| p.candidate)
    }

    fn effort_for(
        &self,
        c: &Candidate,
        base_tier: Tier,
        placement_override: Option<Effort>,
    ) -> Effort {
        choose_effort(EffortInput {
            tier_default: tier_default(self.req.tiers, base_tier),
            placement_override,
            named: self.named_effort(),
            quick_verification: self.req.quick_verification,
            supported: &c.efforts,
            resumed: None,
        })
        .effort
    }

    fn choice(
        &self,
        c: &Candidate,
        tier: Tier,
        effort_tier: Tier,
        placement_override: Option<Effort>,
    ) -> RoutedChoice {
        RoutedChoice {
            tier,
            model: c.model.clone(),
            account: c.account,
            effort: self.effort_for(c, effort_tier, placement_override),
        }
    }

    /// Steps 1–2: an explicitly named model must serve or the task waits. The core checks
    /// placement separately: absence from the available pool does not mean it is unplaced.
    fn try_named(&mut self) -> Result<Option<(RoutedChoice, String)>, String> {
        let (model, later_from) = match (&self.req.user.model, &self.later_step) {
            (Some(m), _) => (m.clone(), None),
            (None, Some(l)) => {
                let Some(model) = &l.model else {
                    return Ok(None);
                };
                (model.clone(), Some(l.from))
            }
            (None, None) => return Ok(None),
        };
        let why = match later_from {
            Some(t) => format!("You asked for this model for later steps in task {t}"),
            None => "You asked for this model".to_owned(),
        };
        let premium = self.premium_asked && is_premium_model(self.req, self.pool, &model);
        let placed = self.pool.placed;
        if let Some(c) = self.pick_from(
            placed
                .iter()
                .filter(|c| c.model == model && (!premium || c.tier == Tier::Premium)),
            self.placement_check(),
        ) {
            return Ok(Some((
                self.choice(c, c.tier, c.tier, c.effort_override),
                why,
            )));
        }
        let reasons: Vec<&str> = self
            .skipped
            .iter()
            .filter(|s| s.model == model)
            .map(|s| s.reason.as_str())
            .collect();
        let reason = if reasons.is_empty() {
            "no usable account currently offers it".to_owned()
        } else {
            reasons.join("; ")
        };
        Err(format!(
            "{why} ({model}), but it cannot serve: {reason}; the task waits for it instead of using another model"
        ))
    }

    /// Step 4: the "always use" pin, unless you asked for premium or a stronger model.
    fn try_pin(&mut self) -> Option<(RoutedChoice, String)> {
        if self.retry_target.is_some() || self.premium_asked {
            return None;
        }
        let pin = self.req.always_use?;
        let tries =
            std::iter::once((&pin.model, false)).chain(pin.fallback.iter().map(|f| (f, true)));
        let pinned = self.pool.pinned;
        for (model, is_fallback) in tries {
            if let Some(c) =
                self.pick_from(pinned.iter().filter(|c| &c.model == model), skip_reason)
            {
                let why = if is_fallback {
                    format!(
                        "You set {} to always be used; it couldn't serve, so its fallback is used",
                        pin.model
                    )
                } else {
                    "You set this model to always be used".to_owned()
                };
                // Pins name a model, not a tier entry; they keep the landed tier's effort rules.
                return Some((
                    self.choice(c, self.tier_landed, self.tier_landed, None),
                    why,
                ));
            }
        }
        // Neither the pin nor its fallback can serve: route normally and say so, rather than
        // leaving the task waiting with no end.
        self.notes.push(format!(
            "The model you set to always be used ({}) and its fallback couldn't serve, so routing picked",
            pin.model
        ));
        None
    }

    /// Steps 3 and 5: the tier, with fallbacks.
    fn try_tiers(&mut self) -> Option<(RoutedChoice, String)> {
        let landed = self.tier_landed;
        let order = match self.retry_target {
            Some(t) => fallback::retry_order(t),
            None => fallback::order(landed),
        };
        let placed = self.pool.placed;
        let mut prev = landed;
        for &tier in order {
            if tier != landed {
                self.fallbacks.push(FallbackStep {
                    from: prev,
                    to: tier,
                });
                prev = tier;
            }
            let Some(c) = self.pick_from(
                placed.iter().filter(|c| c.tier == tier),
                self.placement_check(),
            ) else {
                continue;
            };
            let mut why = if self.retry_target.is_some() {
                "You chose to try a stronger model".to_owned()
            } else if self.premium_asked {
                "You asked for premium".to_owned()
            } else {
                rule_line(self.kind, self.req.builds_on.as_ref(), self.matched)
            };
            if landed == Tier::Premium {
                if tier != landed {
                    why = format!(
                        "{why}. {}, so {}",
                        self.premium_unavailable(),
                        fallback::runs_on_line(landed, tier)
                    );
                }
            } else if tier != landed {
                why = format!(
                    "No {landed} model was available ({}). {why}",
                    fallback::runs_on_line(landed, tier)
                );
            }
            // A selected combo keeps its own effort, including on fallback. Legacy entries with
            // no effort still inherit the landed tier's default (and its quick-verification rule).
            return Some((self.choice(c, tier, landed, c.effort_override), why));
        }
        None
    }

    /// Why no premium model served: each one skipped, with its reason, or that none is placed.
    fn premium_unavailable(&self) -> String {
        let mut reasons: Vec<String> = Vec::new();
        for c in self.pool.placed.iter().filter(|c| c.tier == Tier::Premium) {
            let Some(s) = self
                .skipped
                .iter()
                .find(|s| s.model == c.model && s.account == c.account)
            else {
                continue;
            };
            let line = format!("{}: {}", s.model, s.reason);
            if !reasons
                .iter()
                .any(|r| r.starts_with(&format!("{}: ", s.model)))
            {
                reasons.push(line);
            }
        }
        if reasons.is_empty() {
            "No model is placed in premium".to_owned()
        } else {
            format!("No premium model could serve ({})", reasons.join("; "))
        }
    }

    /// Waiting.
    fn waiting_line(&self) -> String {
        match self.retry_target {
            Some(t) if t != Tier::Premium => fallback::retry_waiting_line(t),
            // Every tier is empty or can't serve. The task waits until one can.
            _ => "No model in any tier can serve this task; it waits".to_owned(),
        }
    }
}

/// The model is placed in premium (whether or not it can serve now).
fn is_premium_model(req: &RouteRequest<'_>, pool: Pool<'_>, model: &ModelId) -> bool {
    req.premium_models.contains(model)
        || pool
            .placed
            .iter()
            .any(|c| &c.model == model && c.tier == Tier::Premium)
}

/// Routes one task. Never fails: a decision either chose a model, account, effort and tier, or is
/// waiting.
pub fn route(table: &RuleTable, req: &RouteRequest<'_>, pool: Pool<'_>) -> RoutingRecord {
    let kind = req.kind.kind_or_unclear();
    let decided_by = match &req.kind {
        Decided::Answer { by, .. } => by.clone(),
        Decided::Undecided { .. } => req.decider.clone(),
    };
    let fact = req
        .builds_on
        .as_ref()
        .map_or(BuildsOnFact::Nothing, |b| b.fact);
    let matched = table.tier_for(kind, fact);
    let mut router = Router::new(req, pool, kind, matched);

    let (choice, mut why) = match router.try_named() {
        Ok(Some((choice, why))) => (Some(choice), why),
        Ok(None) => match router.try_pin().or_else(|| router.try_tiers()) {
            Some((choice, why)) => (Some(choice), why),
            None => (None, router.waiting_line()),
        },
        Err(why) => (None, why),
    };
    for n in &router.notes {
        why.push_str(". ");
        why.push_str(n);
    }

    let mut user = req.user.clone();
    user.stronger_retry = router.retry_target.is_some();
    RoutingRecord {
        facts: DeclaredFacts {
            kind,
            builds_on: req.builds_on.clone(),
            needs_image_input: req.needs.image_input,
            quick_verification: req.quick_verification,
            user,
            later_step_from: router.later_step.as_ref().map(|l| l.from),
            prefer_provider_other_than: None,
        },
        rule: matched.to_string(),
        decided_by,
        tier_landed: router.tier_landed,
        choice,
        skipped: router.skipped,
        fallbacks: router.fallbacks,
        why,
    }
}

#[cfg(test)]
mod premium_tests;
#[cfg(test)]
mod tests;
