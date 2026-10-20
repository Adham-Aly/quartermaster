//! Helper agents: the provider crewmate's `agent` tool, the provider engine's counterpart of Claude
//! Code's `Agent`.
//!
//! A call starts a child: a fresh provider session on the crewmate's account and current model,
//! in its worktree, with the same credential-free `bash` environment and write roots. The child
//! is offered exactly the file and shell tools (`read`, `write`, `edit`, `grep`, `find`, `ls`,
//! `bash`, `job_output`, `job_kill`): its tools come straight from the [`BuiltinToolsFactory`],
//! never wrapped in `HelperTools`, so it has no `agent` (no nesting), and its spec carries no
//! tools of ours (no lifecycle or reporting tool). Its final text is the call's result.
//!
//! - **Concurrency.** Calls in one assistant message already run concurrently in the loop; at
//!   most [`MAX_CONCURRENT_HELPERS`] children run at once per crewmate, the rest wait for a slot.
//! - **Cancellation.** Each child is driven by its own task (`child`). Dropping the call's
//!   future (an interrupt, a stop or a quit drops the turn's running tools) closes the child,
//!   which stops its `bash` jobs; [`BuiltinTools::shutdown`] waits until every child has closed.
//! - **Usage.** The children's calls are added up as they finish and handed to the loop through
//!   [`BuiltinTools::take_usage`], which adds them to the crewmate's turn.
//! - **Limits.** A child gets the crewmate's `limits` (its output cap and retries) and the same
//!   stream idle limit; it is stopped after [`HELPER_MAX_MODEL_CALLS`] model calls, and a child
//!   waiting on background jobs gives up after [`HELPER_JOB_WAIT`] without one exiting.
//! - **Crash recovery.** A child in flight is lost with the app; the crewmate's transcript holds
//!   the call without a result, which replay answers with an error, as for any in-flight tool.
//!
//! Each child's transcript is a session file under the crewmate's `session/helpers/`, and its job
//! logs go to their own folder under the task's `jobs/helpers/`, so its job labels never collide
//! with the crewmate's.

mod child;

use crate::builtin::{BuiltinCx, BuiltinOutcome, BuiltinTools, BuiltinToolsFactory, JobNotice};
use crate::engine::Shared;
use crate::session;
use qm_engine::{SessionSpec, SystemPrompt, ToolDescriptor, TurnUsage, session_channel};
use qm_tools::defs::files::AgentArgs;
use qm_types::BoxFuture;
use qm_types::domain::Role;
use qm_types::ids::ModelId;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;

/// The tool's name.
pub const HELPER_TOOL: &str = "agent";

/// How many helpers one crewmate runs at once; more calls wait for a slot. Four covers the
/// parallel searches a broad exploration fans out into, while bounding what one crewmate puts on
/// its account at a time: every child streams from the same account (a subscription's rate
/// window is shared) and may run its own `bash` jobs on this Mac. The tool's description says
/// "up to 4 at once".
pub const MAX_CONCURRENT_HELPERS: usize = 4;

/// A child is interrupted after this many model calls and the call fails with its last message:
/// a runaway child can't keep the crewmate waiting forever. A hundred rounds of tool calls is
/// well past what a focused exploration needs.
pub const HELPER_MAX_MODEL_CALLS: u32 = 100;

/// A child that ended its turn while background jobs run is woken when one exits; after this
/// long without an exit its jobs are stopped and its last message is the result.
pub const HELPER_JOB_WAIT: Duration = Duration::from_mins(30);

/// The child's system prompt; the working folder is added after it.
pub const HELPER_SYSTEM_PROMPT: &str = include_str!("prompt.md");

/// Adds `extra` (helpers' usage) to a turn's usage; `duration_ms` stays the turn's own.
pub(crate) fn add_usage(usage: &mut TurnUsage, extra: &TurnUsage) {
    usage.model_calls += extra.model_calls;
    usage.input_tokens += extra.input_tokens;
    usage.output_tokens += extra.output_tokens;
    usage.thinking_tokens += extra.thinking_tokens;
    usage.cache_read_tokens += extra.cache_read_tokens;
    usage.cache_write_tokens += extra.cache_write_tokens;
    usage.cost_usd = match (usage.cost_usd, extra.cost_usd) {
        (Some(a), Some(b)) => Some(a + b),
        (a, b) => a.or(b),
    };
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Plain data behind these locks; a panic elsewhere leaves nothing half-done.
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A crewmate's built-in tools with `agent` added.
pub(crate) struct HelperTools {
    inner: Arc<dyn BuiltinTools>,
    shared: Arc<Shared>,
    factory: Arc<dyn BuiltinToolsFactory>,
    /// The crewmate's own tools context; each child gets its own jobs folder.
    cx: BuiltinCx,
    /// What every child's spec starts from (the model is filled in per call).
    template: SessionSpec,
    model: Mutex<ModelId>,
    seed: u64,
    started: AtomicU64,
    slots: Arc<Semaphore>,
    usage: Arc<Mutex<TurnUsage>>,
    /// The children's driver tasks, so shutdown can wait for them.
    drivers: Mutex<Vec<JoinHandle<()>>>,
}

impl fmt::Debug for HelperTools {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HelperTools")
            .field("inner", &self.inner)
            .field("cx", &self.cx)
            .field("free_slots", &self.slots.available_permits())
            .finish_non_exhaustive()
    }
}

impl HelperTools {
    /// Adds `agent` to a crewmate's tools; any other role's tools are returned as they are
    /// (the Quartermaster and secondmates keep their orchestration-only lists).
    pub(crate) fn wrap(
        inner: Arc<dyn BuiltinTools>,
        shared: Arc<Shared>,
        factory: Arc<dyn BuiltinToolsFactory>,
        cx: BuiltinCx,
        spec: &SessionSpec,
        seed: u64,
    ) -> Arc<dyn BuiltinTools> {
        if spec.role != Role::Crewmate {
            return inner;
        }
        let prompt = format!(
            "{}\n\nYour working folder is {}.\n",
            HELPER_SYSTEM_PROMPT.trim_end(),
            spec.cwd.display()
        );
        let mut template = SessionSpec::new(
            spec.agent,
            spec.role,
            spec.account.clone(),
            spec.model.clone(),
            spec.effort,
            spec.cwd.clone(),
            spec.session_dir.join("helpers"),
            SystemPrompt::Replace { text: prompt },
        );
        template.builtins = spec.builtins.clone();
        template.env = spec.env.clone();
        template.limits = spec.limits;
        Arc::new(Self {
            inner,
            shared,
            factory,
            cx,
            model: Mutex::new(spec.model.clone()),
            template,
            seed,
            started: AtomicU64::new(0),
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT_HELPERS)),
            usage: Arc::new(Mutex::new(TurnUsage::default())),
            drivers: Mutex::new(Vec::new()),
        })
    }

    /// One `agent` call: waits for a slot, starts the child and waits for its answer. Dropping
    /// this future closes the child.
    async fn helper(&self, args: serde_json::Value) -> BuiltinOutcome {
        let args: AgentArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(e) => return failed(String::new(), format!("invalid arguments for agent: {e}")),
        };
        let summary = crate::builtin::summarize_args(
            HELPER_TOOL,
            &serde_json::json!({ "description": args.description }),
        );
        if args.prompt.trim().is_empty() {
            return failed(
                summary,
                "the prompt is empty: say what the helper should do".into(),
            );
        }
        let Ok(slot) = Arc::clone(&self.slots).acquire_owned().await else {
            return failed(
                summary,
                "the session is closing; no helper was started".into(),
            );
        };
        let n = self.started.fetch_add(1, Ordering::Relaxed);
        let (handle, side) = session_channel();
        let mut spec = self.template.clone();
        spec.model = lock(&self.model).clone();
        let mut cx = self.cx.clone();
        cx.jobs_dir = cx
            .jobs_dir
            .join("helpers")
            .join(uuid::Uuid::now_v7().simple().to_string());
        let tools = self.factory.create(cx);
        tokio::spawn(session::supervise(
            Arc::clone(&self.shared),
            spec,
            side,
            Some(tools),
            self.seed.wrapping_add(n + 1),
        ));
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        let (result_tx, result_rx) = oneshot::channel::<child::Answer>();
        let driver = tokio::spawn(child::drive(child::Child {
            handle,
            prompt: args.prompt,
            usage: Arc::clone(&self.usage),
            clock: Arc::clone(&self.shared.clock),
            cancel: cancel_rx,
            answer: result_tx,
            slot,
        }));
        {
            let mut drivers = lock(&self.drivers);
            drivers.retain(|d| !d.is_finished());
            drivers.push(driver);
        }
        // Held until the answer comes; dropped with this future, which closes the child.
        let _cancel = cancel_tx;
        match result_rx.await {
            Ok(child::Answer { text, is_error }) => BuiltinOutcome {
                text,
                is_error,
                images: Vec::new(),
                summary,
                job_started: None,
            },
            Err(_) => failed(summary, "the helper ended without an answer".into()),
        }
    }
}

fn failed(summary: String, text: String) -> BuiltinOutcome {
    BuiltinOutcome {
        text,
        is_error: true,
        images: Vec::new(),
        summary,
        job_started: None,
    }
}

impl BuiltinTools for HelperTools {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let mut tools = self.inner.descriptors();
        if let Some(def) = qm_tools::registry::find(Role::Crewmate, HELPER_TOOL) {
            tools.push(def.descriptor());
        }
        tools
    }

    fn handles(&self, name: &str) -> bool {
        name == HELPER_TOOL || self.inner.handles(name)
    }

    fn run<'a>(&'a self, name: &'a str, args: serde_json::Value) -> BoxFuture<'a, BuiltinOutcome> {
        if name == HELPER_TOOL {
            Box::pin(self.helper(args))
        } else {
            self.inner.run(name, args)
        }
    }

    fn take_job_notices(&self) -> Option<mpsc::UnboundedReceiver<JobNotice>> {
        self.inner.take_job_notices()
    }

    fn running_jobs(&self) -> usize {
        self.inner.running_jobs()
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            // No new helper starts; the running ones are closing (their calls were dropped with
            // the turn), and each driver ends once its child has closed and stopped its jobs.
            self.slots.close();
            self.inner.shutdown().await;
            let drivers: Vec<JoinHandle<()>> = std::mem::take(&mut *lock(&self.drivers));
            for driver in drivers {
                let _ = driver.await;
            }
        })
    }

    fn take_usage(&self) -> TurnUsage {
        std::mem::take(&mut *lock(&self.usage))
    }

    fn model_changed(&self, model: &ModelId) {
        *lock(&self.model) = model.clone();
    }
}
