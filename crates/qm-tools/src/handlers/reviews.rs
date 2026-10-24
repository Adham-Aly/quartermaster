//! `approve_review` for orchestrators: the user's go-ahead on a review's findings. The host
//! checks the review waits on it.

use super::conversation::started_by;
use crate::defs::{Ack, ApproveReviewArgs, ApproveReviewTool};
use crate::handler::{Handle, ToolCx, ToolFailure};
use qm_policy::overrides::{Authority, OverrideKind, check_override};
use qm_types::BoxFuture;

impl Handle for ApproveReviewTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: ApproveReviewArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            check_override(Authority::Turn(started_by(cx)), OverrideKind::ReviewGoAhead)?;
            cx.host.approve_review(cx.caller, args.task).await
        })
    }
}
