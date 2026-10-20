//! The verification tools: `ci_diagnosis`, `ci_rerun`, `present_hold` and `set_verification`.
//!
//! Each handler checks the shape of its arguments (and `set_verification` its override),
//! then calls one host method; the host (`qm_core::verification` and `qm_core::decisions`) checks
//! the facts: the failing check, the repair counters, the task's status, direct reports only.

use super::conversation::started_by;
use crate::defs::{
    Ack, CiDiagnosisArgs, CiDiagnosisTool, CiRerunArgs, CiRerunTool, Held, PresentHoldArgs,
    PresentHoldTool, SetVerificationArgs, SetVerificationTool, VerificationChanged,
};
use crate::handler::{Handle, ToolCx, ToolFailure};
use qm_policy::overrides::{Authority, OverrideKind, check_override};
use qm_types::BoxFuture;
use qm_types::domain::CiDiagnosis;
use qm_types::error::Refusal;

/// The longest evidence `ci_diagnosis` takes, in characters.
pub const MAX_EVIDENCE_CHARS: usize = 4_000;
/// The longest check name, in characters.
pub const MAX_CHECK_CHARS: usize = 200;
/// The longest `present_hold` summary, in characters (matches `descriptions/present_hold.md`).
pub const MAX_HOLD_SUMMARY_CHARS: usize = 600;

/// A check's name: not blank, not too long.
fn check_name(check: &str) -> Result<String, Refusal> {
    let check = check.trim();
    if check.is_empty() {
        return Err(Refusal::new("name the check"));
    }
    let n = check.chars().count();
    if n > MAX_CHECK_CHARS {
        return Err(Refusal::new(format!(
            "a check's name is at most {MAX_CHECK_CHARS} characters (this one is {n})"
        )));
    }
    Ok(check.to_owned())
}

impl Handle for CiDiagnosisTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: CiDiagnosisArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            let check = check_name(&args.check)?;
            let evidence = args
                .evidence
                .as_deref()
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .map(str::to_owned);
            if let Some(e) = &evidence {
                let n = e.chars().count();
                if n > MAX_EVIDENCE_CHARS {
                    return Err(Refusal::new(format!(
                        "evidence is at most {MAX_EVIDENCE_CHARS} characters (this is {n}); put \
                         the rest in your evidence folder and point to it"
                    ))
                    .into());
                }
            }
            match (args.diagnosis, &evidence) {
                (CiDiagnosis::SuspectedFlake, None) => {
                    return Err(Refusal::new(
                        "a suspected flake needs evidence: what shows the check is flaky",
                    )
                    .into());
                }
                (CiDiagnosis::FailingOnMain, None) => {
                    return Err(Refusal::new(
                        "failing on main needs evidence: your reproduction on an untouched main",
                    )
                    .into());
                }
                _ => {}
            }
            cx.host
                .declare_diagnosis(
                    cx.caller,
                    CiDiagnosisArgs {
                        check,
                        diagnosis: args.diagnosis,
                        evidence,
                    },
                )
                .await
        })
    }
}

impl Handle for CiRerunTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: CiRerunArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            let check = check_name(&args.check)?;
            cx.host.rerun_check(cx.caller, check).await
        })
    }
}

impl Handle for PresentHoldTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: PresentHoldArgs,
    ) -> BoxFuture<'a, Result<Held, ToolFailure>> {
        Box::pin(async move {
            let summary = args.summary.trim();
            if summary.is_empty() {
                return Err(Refusal::new(
                    "the summary is your view in two or three sentences; it can't be empty",
                )
                .into());
            }
            let n = summary.chars().count();
            if n > MAX_HOLD_SUMMARY_CHARS {
                return Err(Refusal::new(format!(
                    "the summary is at most {MAX_HOLD_SUMMARY_CHARS} characters (this one is {n})"
                ))
                .into());
            }
            cx.host
                .present_hold(cx.caller, args.task, summary.to_owned())
                .await
        })
    }
}

impl Handle for SetVerificationTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: SetVerificationArgs,
    ) -> BoxFuture<'a, Result<VerificationChanged, ToolFailure>> {
        Box::pin(async move {
            check_override(
                Authority::Turn(started_by(cx)),
                OverrideKind::VerificationLevel,
            )?;
            cx.host
                .set_verification(cx.caller, args.task, args.level)
                .await
        })
    }
}
