//! Routing, step by step: facts in, one recorded decision out.
//!
//! [`route`] runs the whole procedure as one plain function over the declared facts, the pool and
//! your settings, and returns the [`RoutingRecord`] that goes into `routing_decisions`. The order
//! of precedence, highest first:
//!
//! 1. A model you named for this task in your own words: "If you named a model for this task in
//!    your own words, that comes first. If the named model can't serve, the app falls back normally
//!    and says so."
//! 2. A `later_steps` entry recorded on your turn; "newer words of yours, or a stronger-model retry
//!    you pick, take precedence".
//! 3. A stronger-model retry or premium, which you asked for: "Premium sits above high and is
//!    never picked automatically. It is used only when you explicitly ask for it, or when you
//!    choose 'try again with a stronger model'".
//! 4. The "always use" pin: "'Always use' stays as a setting on the pool, with its fallback model
//!    and a notice while it's on."
//! 5. The rule table's tier, with fallbacks and ranking.
//!
//! A `Decider` failure routes high: "if the call fails, times out or can't decide, the task routes
//! high. It never blocks a task".

use crate::decider::{Decided, DecidedBy};
use crate::effort::{EffortInput, choose_effort, tier_default};
use crate::fallback;
use crate::later_steps::AppliedLaterStep;
use crate::rank::{Candidate, Needs, rank, skip_reason};
use crate::record::{
    BuildsOn, BuildsOnFact, DeclaredFacts, FallbackStep, RoutedChoice, RoutingRecord, SkippedModel,
    UserChoices,
};
use crate::rules::RuleTable;
use qm_types::config::{AlwaysUse, TierEfforts};
use qm_types::domain::{Effort, TaskKind, Tier};
use qm_types::ids::AccountId;

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
}

/// The pool routing picks from.
#[derive(Clone, Copy, Debug)]
pub struct Pool<'a> {
    /// Every placement: (model, account) pairs in tiers.
    pub placed: &'a [Candidate],
    /// Pairs for the "always use" model and its fallback on each account that serves them, which
    /// need not be placed in a tier ("ignoring tiers"). Their `tier` is ignored.
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
) -> Option<Picked<'a>> {
    let mut ok: Vec<&Candidate> = Vec::new();
    for c in candidates {
        match skip_reason(c, needs) {
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

/// The one plain-language line for a fact the rule table read.
fn rule_line(kind: TaskKind, builds_on: Option<&BuildsOn>, tier: Tier) -> String {
    match builds_on {
        Some(b) if b.fact == BuildsOnFact::FinishedPlan && tier == Tier::Medium => {
            format!("Building from the plan in task {}", b.task)
        }
        Some(b) if b.fact == BuildsOnFact::ConfirmedDiagnosis && tier == Tier::Medium => {
            format!("Building on the confirmed diagnosis in task {}", b.task)
        }
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
    rule_tier: Tier,
    retry_target: Option<Tier>,
    later_step: Option<AppliedLaterStep>,
    tier_landed: Tier,
    skipped: Vec<SkippedModel>,
    fallbacks: Vec<FallbackStep>,
    notes: Vec<String>,
}

impl<'r, 'a> Router<'r, 'a> {
    fn new(req: &'r RouteRequest<'a>, pool: Pool<'r>, kind: TaskKind, rule_tier: Tier) -> Self {
        let retry_target = if req.user.stronger_retry {
            // A retry with no computed target can't climb; it waits at the strongest tier.
            Some(req.stronger_retry_target.unwrap_or(Tier::Premium))
        } else {
            None
        };
        let later_step = if retry_target.is_some() {
            None
        } else {
            req.later_step.clone()
        };
        let tier_landed = match retry_target {
            Some(t) => t,
            None if req.user.premium => Tier::Premium,
            None => rule_tier,
        };
        let mut notes = Vec::new();
        if let Decided::Undecided { .. } = req.kind {
            notes.push("The kind couldn't be decided, so it routes high".to_owned());
        }
        Self {
            req,
            pool,
            kind,
            rule_tier,
            retry_target,
            later_step,
            tier_landed,
            skipped: Vec::new(),
            fallbacks: Vec::new(),
            notes,
        }
    }

    fn pick_from<'c>(
        &mut self,
        candidates: impl Iterator<Item = &'c Candidate>,
    ) -> Option<&'c Candidate> {
        pick(
            candidates,
            self.req.needs,
            self.req.preferred_account,
            &mut self.skipped,
        )
        .map(|p| p.candidate)
    }

    fn effort_for(&self, c: &Candidate, base_tier: Tier) -> Effort {
        let named = self
            .req
            .user
            .effort
            .or(self.later_step.as_ref().and_then(|l| l.effort));
        let placement_override = if c.tier == base_tier {
            c.effort_override
        } else {
            None
        };
        choose_effort(EffortInput {
            tier_default: tier_default(self.req.tiers, base_tier),
            placement_override,
            named,
            quick_verification: self.req.quick_verification,
            supported: &c.efforts,
            resumed: None,
        })
        .effort
    }

    fn choice(&self, c: &Candidate, tier: Tier, effort_tier: Tier) -> RoutedChoice {
        RoutedChoice {
            tier,
            model: c.model.clone(),
            account: c.account,
            effort: self.effort_for(c, effort_tier),
        }
    }

    /// Steps 1–2: a model you named, in this task's words or a later step's.
    fn try_named(&mut self) -> Option<(RoutedChoice, String)> {
        let from_later = self.later_step.as_ref().map(|l| l.from);
        let (model, later_from) = match (&self.req.user.model, &self.later_step) {
            (Some(m), _) => (m.clone(), None),
            (None, Some(l)) => (l.model.clone()?, from_later),
            (None, None) => return None,
        };
        let placed = self.pool.placed;
        if let Some(c) = self.pick_from(placed.iter().filter(|c| c.model == model)) {
            let why = match later_from {
                Some(t) => format!("You asked for this model for later steps in task {t}"),
                None => "You asked for this model".to_owned(),
            };
            return Some((self.choice(c, c.tier, c.tier), why));
        }
        let note = if placed.iter().any(|c| c.model == model) {
            format!("The model you named ({model}) couldn't serve, so routing picked")
        } else {
            format!("The model you named ({model}) isn't in any tier, so routing picked")
        };
        self.notes.push(note);
        None
    }

    /// Step 4: the "always use" pin, unless you asked for premium or a stronger model.
    fn try_pin(&mut self) -> Option<(RoutedChoice, String)> {
        if self.retry_target.is_some() || self.req.user.premium {
            return None;
        }
        let pin = self.req.always_use?;
        let tries =
            std::iter::once((&pin.model, false)).chain(pin.fallback.iter().map(|f| (f, true)));
        let pinned = self.pool.pinned;
        for (model, is_fallback) in tries {
            if let Some(c) = self.pick_from(pinned.iter().filter(|c| &c.model == model)) {
                let why = if is_fallback {
                    format!(
                        "You set {} to always be used; it couldn't serve, so its fallback is used",
                        pin.model
                    )
                } else {
                    "You set this model to always be used".to_owned()
                };
                return Some((self.choice(c, self.tier_landed, self.tier_landed), why));
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
        let retry_tiers;
        let order: &[Tier] = match self.retry_target {
            Some(t) => {
                retry_tiers = fallback::retry_order(t);
                &retry_tiers
            }
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
            let Some(c) = self.pick_from(placed.iter().filter(|c| c.tier == tier)) else {
                continue;
            };
            let mut why = if self.retry_target.is_some() {
                "You chose to try a stronger model".to_owned()
            } else if self.req.user.premium {
                "You asked for premium".to_owned()
            } else {
                rule_line(self.kind, self.req.builds_on.as_ref(), self.rule_tier)
            };
            if tier != landed {
                why = format!(
                    "No {landed} model was available ({}). {why}",
                    fallback::runs_on_line(landed, tier)
                );
            }
            // "A fallback keeps the effort level of the tier the work landed in, capped by the
            // model actually used."
            return Some((self.choice(c, tier, landed), why));
        }
        None
    }

    /// Waiting.
    fn waiting_line(&self) -> String {
        match self.retry_target {
            Some(t) => fallback::retry_waiting_line(t),
            None if self.tier_landed == Tier::Premium => fallback::PREMIUM_WAITS.to_owned(),
            // Every tier is empty or can't serve. The task waits until one can.
            None => "No model in any tier can serve this task; it waits".to_owned(),
        }
    }
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
    let mut router = Router::new(req, pool, kind, matched.tier);

    let mut picked = router.try_named();
    if picked.is_none() {
        picked = router.try_pin();
    }
    if picked.is_none() {
        picked = router.try_tiers();
    }
    let (choice, mut why) = match picked {
        Some((choice, why)) => (Some(choice), why),
        None => (None, router.waiting_line()),
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
        rule: matched.rule.to_string(),
        decided_by,
        tier_landed: router.tier_landed,
        choice,
        skipped: router.skipped,
        fallbacks: router.fallbacks,
        why,
    }
}
