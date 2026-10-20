//! How the app counts repair attempts.
//!
//! "Counting needs facts, so the diagnosis is a tool call. `ci_diagnosis` declares one of three
//! things for a failing check: own regression, suspected flake (with evidence), or already failing
//! on main (with reproduction evidence from an untouched main). After an own-regression diagnosis,
//! the next push whose same check fails again counts as one failed repair. The third makes the app
//! hold the task. `ci_rerun` works once per task, only after a flake diagnosis with evidence.
//! 'Already failing on main' makes the crewmate file the separate task (`file_followup`) and
//! `declare_wait` on it. Pushing a fix without declaring a diagnosis is refused by `pr_open`, with
//! the reason."
//!
//! "The crewmate reads the logs and declares a diagnosis first, which costs nothing. … On the 3rd
//! failed repair, the app refuses to let the task finish, holds it, and wakes its supervisor, which
//! puts the hold to you as one decision with the fixed choices below. Your answer resets the
//! counters."

use qm_types::domain::CiDiagnosis;
use qm_types::error::Refusal;
use serde::{Deserialize, Serialize};

/// "3 attempts" for its own regression.
pub const MAX_FAILED_REPAIRS: u8 = 3;

/// One declared diagnosis, for a check that is failing on the current head.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declared {
    pub check: String,
    pub diagnosis: CiDiagnosis,
    pub evidence: bool,
}

/// A task's repair counters, stored with the task.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairState {
    /// Diagnoses declared since the last push.
    pub declared: Vec<Declared>,
    /// Checks the latest push is meant to fix as the crewmate's own regression.
    pub awaiting: Vec<String>,
    pub failed_repairs: u8,
    pub rerun_used: bool,
    /// The repair limit holds the task until you answer.
    pub held: bool,
}

/// What a set of check results did to the counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairOutcome {
    /// Nothing counted.
    Nothing,
    /// One more failed repair, with the number used so far.
    Counted { used: u8 },
    /// The third failed repair: hold the task and wake the supervisor.
    Hold,
}

impl RepairState {
    /// `ci_diagnosis`: free to call. A flake needs evidence; "already failing on main" needs
    /// reproduction evidence from an untouched main.
    pub fn declare(
        &mut self,
        check: &str,
        diagnosis: CiDiagnosis,
        evidence: bool,
    ) -> Result<(), Refusal> {
        match diagnosis {
            CiDiagnosis::SuspectedFlake if !evidence => {
                return Err(Refusal::new(
                    "a suspected flake needs evidence attached; declare it with the evidence",
                ));
            }
            CiDiagnosis::FailingOnMain if !evidence => {
                return Err(Refusal::new(
                    "already failing on main needs reproduction evidence from an untouched main",
                ));
            }
            _ => {}
        }
        self.declared.retain(|d| d.check != check);
        self.declared.push(Declared {
            check: check.to_owned(),
            diagnosis,
            evidence,
        });
        Ok(())
    }

    /// `pr_open` pushing while checks fail on the current head: every failing check needs a
    /// diagnosis first, and a held task can't push.
    pub fn on_push(&mut self, failing_now: &[&str]) -> Result<(), Refusal> {
        if self.held {
            return Err(Refusal::new(
                "the task is held after 3 failed repairs; the decision is with the user",
            ));
        }
        if let Some(missing) = failing_now
            .iter()
            .find(|c| !self.declared.iter().any(|d| d.check == **c))
        {
            return Err(Refusal::new(format!(
                "check {missing} is failing and has no diagnosis; declare one with ci_diagnosis before pushing a fix"
            )));
        }
        self.awaiting = self
            .declared
            .iter()
            .filter(|d| {
                d.diagnosis == CiDiagnosis::OwnRegression && failing_now.contains(&d.check.as_str())
            })
            .map(|d| d.check.clone())
            .collect();
        self.declared.clear();
        Ok(())
    }

    /// The finished results on the pushed head: a check the push was meant to fix that fails
    /// again counts one failed repair (one per push).
    pub fn on_results(&mut self, failing: &[&str]) -> RepairOutcome {
        let again = self.awaiting.iter().any(|c| failing.contains(&c.as_str()));
        self.awaiting.clear();
        if !again {
            return RepairOutcome::Nothing;
        }
        self.failed_repairs = self.failed_repairs.saturating_add(1);
        if self.failed_repairs >= MAX_FAILED_REPAIRS {
            self.held = true;
            RepairOutcome::Hold
        } else {
            RepairOutcome::Counted {
                used: self.failed_repairs,
            }
        }
    }

    /// `ci_rerun`: once per task, only after a flake diagnosis with evidence for that check.
    pub fn rerun(&mut self, check: &str) -> Result<(), Refusal> {
        if self.rerun_used {
            return Err(Refusal::new(
                "checks can be rerun once per task, and this task already has",
            ));
        }
        let flake = self
            .declared
            .iter()
            .any(|d| d.check == check && d.diagnosis == CiDiagnosis::SuspectedFlake && d.evidence);
        if !flake {
            return Err(Refusal::new(format!(
                "rerun {check} only after declaring it a suspected flake with evidence"
            )));
        }
        self.rerun_used = true;
        Ok(())
    }

    /// "The app refuses to let the task finish" while the repair limit holds it.
    pub fn check_finish(&self) -> Result<(), Refusal> {
        if self.held {
            Err(Refusal::new(
                "the task is held after 3 failed repairs and can't finish until the user decides",
            ))
        } else {
            Ok(())
        }
    }

    /// "Your answer resets the counters."
    ///
    /// The once-per-task rerun doesn't reset: it is "once per task", so it stays used.
    pub fn reset(&mut self) {
        self.declared.clear();
        self.awaiting.clear();
        self.failed_repairs = 0;
        self.held = false;
    }
}
