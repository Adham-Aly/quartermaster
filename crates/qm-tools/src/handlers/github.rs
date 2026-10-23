//! A crewmate's GitHub tools and the orchestrators' `settle_task`.
//!
//! Each handler checks shapes and limits, then calls one host method; the host checks the facts
//! (the task, its delivery mode and level, the commits, the PR, the checks, the gate).

use crate::defs::{
    CiReport, CiResultsArgs, CiResultsTool, GithubContent, GithubReadArgs, GithubReadTool,
    GithubWriteArgs, GithubWriteOutcome, GithubWriteTool, Held, PrOpenArgs, PrOpenTool, PrOpened,
    PrStatus, PrStatusArgs, PrStatusTool, ProposeReleaseTagArgs, ProposeReleaseTagTool,
    ReleaseStatus, ReleaseStatusArgs, ReleaseStatusTool, SettleOutcome, SettleTaskArgs,
    SettleTaskTool,
};
use crate::handler::{Handle, ToolCx, ToolFailure};
use qm_policy::names::{Boundary, check_pr_title};
use qm_types::BoxFuture;
use qm_types::domain::{GithubReadKind, GithubWriteAction};
use qm_types::error::Refusal;

/// The longest pull request body, in characters.
pub const MAX_PR_BODY_CHARS: usize = 60_000;
/// The longest `github_write` body, in characters.
pub const MAX_WRITE_BODY_CHARS: usize = 65_536;
/// The most labels one `github_write` call adds or removes.
pub const MAX_LABELS: usize = 20;
/// The longest issue title, in characters.
pub const MAX_ISSUE_TITLE_CHARS: usize = 256;

fn refused(reason: impl Into<String>) -> ToolFailure {
    Refusal::new(reason).into()
}

impl Handle for PrOpenTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: PrOpenArgs,
    ) -> BoxFuture<'a, Result<PrOpened, ToolFailure>> {
        Box::pin(async move {
            check_pr_title(&args.title, Boundary::PrOpen)?;
            if args.body.trim().is_empty() {
                return Err(refused("the pull request body can't be empty"));
            }
            let n = args.body.chars().count();
            if n > MAX_PR_BODY_CHARS {
                return Err(refused(format!(
                    "the pull request body is at most {MAX_PR_BODY_CHARS} characters (this one is {n})"
                )));
            }
            cx.host
                .open_pull_request(cx.caller, args.title, args.body)
                .await
        })
    }
}

impl Handle for PrStatusTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: PrStatusArgs,
    ) -> BoxFuture<'a, Result<PrStatus, ToolFailure>> {
        let PrStatusArgs {} = args;
        Box::pin(async move { cx.host.pull_request_status(cx.caller).await })
    }
}

impl Handle for CiResultsTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: CiResultsArgs,
    ) -> BoxFuture<'a, Result<CiReport, ToolFailure>> {
        Box::pin(async move {
            let check = args
                .check
                .map(|c| c.trim().to_owned())
                .filter(|c| !c.is_empty());
            cx.host.ci_results(cx.caller, check).await
        })
    }
}

impl Handle for GithubReadTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: GithubReadArgs,
    ) -> BoxFuture<'a, Result<GithubContent, ToolFailure>> {
        Box::pin(async move {
            match args.what {
                GithubReadKind::Issue | GithubReadKind::PullRequest | GithubReadKind::Comments
                    if args.number.is_none() =>
                {
                    return Err(refused(format!("reading {} needs number", args.what)));
                }
                GithubReadKind::File
                    if args.path.as_deref().is_none_or(|p| p.trim().is_empty()) =>
                {
                    return Err(refused("reading a file needs path"));
                }
                _ => {}
            }
            cx.host.github_read(cx.caller, args).await
        })
    }
}

/// `github_write`'s shape: limits, and what each action needs.
pub fn check_write(args: &GithubWriteArgs) -> Result<(), Refusal> {
    if let Some(body) = &args.body {
        let n = body.chars().count();
        if n > MAX_WRITE_BODY_CHARS {
            return Err(Refusal::new(format!(
                "the body is at most {MAX_WRITE_BODY_CHARS} characters (this one is {n})"
            )));
        }
    }
    if args.labels.len() > MAX_LABELS {
        return Err(Refusal::new(format!(
            "one call changes at most {MAX_LABELS} labels (this one has {})",
            args.labels.len()
        )));
    }
    if args.labels.iter().any(|l| l.trim().is_empty()) {
        return Err(Refusal::new("a label can't be empty"));
    }
    let blank = |s: &Option<String>| s.as_deref().is_none_or(|s| s.trim().is_empty());
    match args.action {
        GithubWriteAction::Comment => {
            if args.number.is_none() || blank(&args.body) {
                return Err(Refusal::new("a comment needs number and body"));
            }
        }
        GithubWriteAction::AddLabels | GithubWriteAction::RemoveLabels => {
            if args.number.is_none() || args.labels.is_empty() {
                return Err(Refusal::new("changing labels needs number and labels"));
            }
        }
        GithubWriteAction::OpenIssue => {
            if blank(&args.title) {
                return Err(Refusal::new("opening an issue needs title"));
            }
            let n = args.title.as_deref().map_or(0, |t| t.chars().count());
            if n > MAX_ISSUE_TITLE_CHARS {
                return Err(Refusal::new(format!(
                    "an issue title is at most {MAX_ISSUE_TITLE_CHARS} characters (this one is {n})"
                )));
            }
        }
    }
    Ok(())
}

impl Handle for GithubWriteTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: GithubWriteArgs,
    ) -> BoxFuture<'a, Result<GithubWriteOutcome, ToolFailure>> {
        Box::pin(async move {
            check_write(&args)?;
            cx.host.github_write(cx.caller, args).await
        })
    }
}

impl Handle for ProposeReleaseTagTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: ProposeReleaseTagArgs,
    ) -> BoxFuture<'a, Result<Held, ToolFailure>> {
        Box::pin(async move { cx.host.propose_release_tag(cx.caller, args).await })
    }
}

impl Handle for ReleaseStatusTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: ReleaseStatusArgs,
    ) -> BoxFuture<'a, Result<ReleaseStatus, ToolFailure>> {
        Box::pin(async move { cx.host.release_status(cx.caller, args).await })
    }
}

impl Handle for SettleTaskTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: SettleTaskArgs,
    ) -> BoxFuture<'a, Result<SettleOutcome, ToolFailure>> {
        Box::pin(async move { cx.host.settle_task(cx.caller, args.task).await })
    }
}
