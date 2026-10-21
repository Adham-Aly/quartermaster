//! `quartermaster dev session`: one agent on a real engine, driven from stdin.
//!
//! Commands, one per line after the `qm-dev>` prompt: `turn <text>`, `inject <text>`,
//! `interrupt`, `kill` (SIGKILL the agent's process group: a crash), `close`, `status`, `quit`.
//! Every event is printed as one line, `event <name> <JSON>`, and committed text also as
//! `text │ <line>`. Calls to `ping` are answered here, so the round trip goes engine → binary →
//! engine through the real `SessionHandle`.

use super::{Flags, Startup, dev_fail, inherited_env, ping_tool};
use crate::Failure;
use qm_auth::FileCredentialStore;
use qm_core::processes::StoreRecorder;
use qm_engine::scripted::{Script, ScriptedEngine};
use qm_engine::supervisor::{
    Child, GroupMember, OsSupervisor, PortListener, ProcessGroup, ProcessOwner, Signal, SpawnSpec,
    Supervisor, SupervisorError,
};
use qm_engine::{
    AccountRef, Builtins, Engine, EngineEvent, SessionHandle, SessionLimits, SessionSpec,
    SystemPrompt, ToolResult, TurnInput,
};
use qm_engine_claude::{ClaudeConfig, ClaudeEngine};
use qm_providers::{FileToolsFactory, ProviderEngine, ProviderEngineConfig};
use qm_types::clock::{Clock, SystemClock};
use qm_types::config::Config;
use qm_types::domain::{Effort, Provider, Role};
use qm_types::ids::{AccountId, AgentId, ModelId};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// The orchestrator prompt of the prototype.
pub(super) const PROMPT: &str =
    "You are Quartermaster's test agent. Use only the tools you are given. Be terse.";

/// The OS supervisor, remembering the agent's process group for `kill`.
#[derive(Debug)]
struct Tracked {
    inner: OsSupervisor,
    agent_group: Mutex<Option<ProcessGroup>>,
}

impl Tracked {
    fn agent_group(&self) -> Option<ProcessGroup> {
        *self
            .agent_group
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Supervisor for Tracked {
    fn spawn(&self, spec: SpawnSpec) -> Result<Child, SupervisorError> {
        let is_agent = matches!(spec.owner, ProcessOwner::Agent { .. });
        let child = self.inner.spawn(spec)?;
        if is_agent {
            *self
                .agent_group
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(child.info.group);
        }
        Ok(child)
    }

    fn signal_group(&self, group: ProcessGroup, signal: Signal) -> Result<(), SupervisorError> {
        self.inner.signal_group(group, signal)
    }

    fn group_alive(&self, group: ProcessGroup) -> bool {
        self.inner.group_alive(group)
    }

    fn group_members(&self, group: ProcessGroup) -> Vec<GroupMember> {
        self.inner.group_members(group)
    }

    fn listeners(&self, ports: RangeInclusive<u16>) -> Result<Vec<PortListener>, SupervisorError> {
        self.inner.listeners(ports)
    }
}

/// One engine's wiring for the session.
struct Wiring {
    engine: Arc<dyn Engine>,
    account: AccountRef,
    default_model: &'static str,
    prompt: SystemPrompt,
    builtins: Builtins,
    env: BTreeMap<String, String>,
}

/// What `status` prints.
#[derive(Debug, Default)]
struct Status {
    session: Option<String>,
    in_turn: bool,
    down: bool,
    closed: bool,
    jobs: usize,
}

#[allow(clippy::too_many_lines)] // One flat sequence: parse, wire, run.
pub(super) async fn run(startup: &Startup, flags: &Flags) -> Result<(), Failure> {
    let engine_name = flags.get("--engine").unwrap_or("scripted");
    let role = match flags.get("--role").unwrap_or("quartermaster") {
        "quartermaster" => Role::Quartermaster,
        "secondmate" => Role::Secondmate,
        "crewmate" => Role::Crewmate,
        other => return Err(dev_fail(format!("unknown role {other}"))),
    };
    let effort: Effort = flags
        .get("--effort")
        .unwrap_or("low")
        .parse()
        .map_err(|e| dev_fail(format!("{e}")))?;
    let idle_shutdown = flags
        .get("--idle-shutdown")
        .map(|s| {
            s.parse::<u64>()
                .map(Duration::from_secs)
                .map_err(|_| dev_fail("--idle-shutdown takes seconds"))
        })
        .transpose()?;
    let root = startup.dir.root().join("dev");
    let cwd = flags
        .get("--cwd")
        .map_or_else(|| root.join("cwd"), PathBuf::from);
    let session_dir = root.join("session");
    for dir in [&cwd, &session_dir] {
        crate::home::create_private_dir(dir)
            .map_err(|e| dev_fail(format!("can't create {}: {e}", dir.display())))?;
    }

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let recorder = StoreRecorder::new(startup.db.clone(), Arc::clone(&clock));
    let tracked = Arc::new(Tracked {
        inner: OsSupervisor::new(Some(Arc::new(recorder))),
        agent_group: Mutex::new(None),
    });
    let supervisor: Arc<dyn Supervisor> = tracked.clone();
    let orchestrator = matches!(role, Role::Quartermaster | Role::Secondmate);

    let Wiring {
        engine,
        account,
        default_model,
        prompt,
        builtins,
        env,
    } = match engine_name {
        "scripted" => {
            let path = flags
                .get("--script")
                .ok_or_else(|| dev_fail("--engine scripted needs --script FILE"))?;
            let script = Script::load(std::path::Path::new(path))
                .map_err(|e| dev_fail(format!("can't load the script: {e}")))?;
            Wiring {
                engine: Arc::new(ScriptedEngine::new(script, Arc::clone(&clock))),
                account: account(Provider::Scripted, None),
                default_model: "scripted",
                prompt: SystemPrompt::Replace {
                    text: PROMPT.into(),
                },
                builtins: Builtins::None,
                env: BTreeMap::new(),
            }
        }
        "claude" => {
            let inherited = inherited_env();
            let claude = match flags.get("--claude-bin") {
                Some(path) => PathBuf::from(path),
                None => qm_engine_claude::detect::detect(&inherited)
                    .path
                    .ok_or_else(|| dev_fail("claude isn't installed (or pass --claude-bin)"))?,
            };
            let env = BTreeMap::new();
            let engine = ClaudeEngine::new(ClaudeConfig {
                claude,
                supervisor: Arc::clone(&supervisor),
                clock: Arc::clone(&clock),
                inherited_env: inherited,
                settings: watch::channel(Config::default()).1,
            });
            let (prompt, builtins) = if orchestrator {
                (
                    SystemPrompt::Replace {
                        text: PROMPT.into(),
                    },
                    Builtins::None,
                )
            } else {
                (
                    SystemPrompt::AppendToClaudeCode {
                        text: PROMPT.into(),
                    },
                    Builtins::ClaudeCode,
                )
            };
            Wiring {
                engine: Arc::new(engine),
                account: account(Provider::Claude, None),
                default_model: qm_engine_claude::check::PROBE_MODEL,
                prompt,
                builtins,
                env,
            }
        }
        "provider" => {
            let reference = flags
                .get("--account")
                .ok_or_else(|| dev_fail("--engine provider needs --account REF"))?;
            let provider = if reference.starts_with("openai:") {
                Provider::OpenAi
            } else {
                Provider::ChatGpt
            };
            let credentials = FileCredentialStore::open(startup.dir.auth_file())
                .map_err(|e| dev_fail(format!("can't open the credential file: {e}")))?;
            let engine = ProviderEngine::new(ProviderEngineConfig {
                tokens: Arc::new(qm_auth::TokenKeeper::new(
                    Arc::new(credentials),
                    qm_auth::LoginEndpoints::default(),
                    Arc::new(qm_auth::LoginHealth::new()),
                )),
                supervisor: Arc::clone(&supervisor),
                clock: Arc::clone(&clock),
                models: crate::wiring::provider_models(),
                builtins: Some(Arc::new(FileToolsFactory)),
                inherited_env: inherited_env(),
                user_agent: format!("quartermaster/{}", env!("CARGO_PKG_VERSION")),
            });
            let builtins = if orchestrator {
                Builtins::None
            } else {
                Builtins::ProviderFileTools {
                    write_roots: vec![cwd.clone()],
                }
            };
            Wiring {
                engine: Arc::new(engine),
                account: account(provider, Some(reference.to_owned())),
                default_model: qm_providers::model::CHEAPEST_CHATGPT_MODEL,
                prompt: SystemPrompt::Replace {
                    text: PROMPT.into(),
                },
                builtins,
                env: BTreeMap::new(),
            }
        }
        other => return Err(dev_fail(format!("unknown engine {other}"))),
    };

    let model = ModelId::new(flags.get("--model").unwrap_or(default_model))
        .map_err(|e| dev_fail(format!("{e}")))?;
    let mut spec = SessionSpec::new(
        AgentId::new(1),
        role,
        account,
        model,
        effort,
        cwd,
        session_dir,
        prompt,
    );
    spec.tools = vec![ping_tool()];
    spec.builtins = builtins;
    spec.env = env;
    spec.idle_shutdown = idle_shutdown;
    if let Some(reference) = flags.get("--resume") {
        spec.resume = Some(
            qm_types::ids::SessionRef::new(reference)
                .map_err(|e| dev_fail(format!("--resume: {e}")))?,
        );
    }
    if orchestrator {
        spec.limits = Some(SessionLimits {
            max_output_tokens: 8_192,
            max_retries: 2,
        });
    }
    let mut handle = engine
        .open(spec)
        .map_err(|e| dev_fail(format!("the engine refused the session: {e}")))?;
    drive(&mut handle, &tracked).await
}

fn account(provider: Provider, credential: Option<String>) -> AccountRef {
    AccountRef {
        id: AccountId::new(1),
        provider,
        credential,
        base_url: None,
    }
}

/// Reads commands from stdin and prints every event until the session closes or `quit`.
async fn drive(handle: &mut SessionHandle, tracked: &Tracked) -> Result<(), Failure> {
    let (tx, mut lines) = mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line.trim_end().to_owned()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut status = Status::default();
    let mut quitting = false;
    prompt();
    loop {
        tokio::select! {
            event = handle.next_event() => {
                let Some(event) = event else { break };
                print_event(&event);
                observe(&mut status, &event);
                if let EngineEvent::ToolCall { call, name, args } = &event {
                    let result = answer(name, args);
                    let _ = handle.tool_result(call.clone(), result);
                }
                if status.closed {
                    break;
                }
            }
            line = lines.recv(), if !quitting => {
                let Some(line) = line else {
                    // stdin closed: like `quit`.
                    quitting = true;
                    let _ = handle.close();
                    continue;
                };
                let (command, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
                match command {
                    "" => {}
                    "turn" => report(handle.start_turn(TurnInput::text(rest))),
                    "inject" => report(handle.inject(TurnInput::text(rest))),
                    "interrupt" => report(handle.interrupt()),
                    "close" => report(handle.close()),
                    "kill" => match tracked.agent_group() {
                        Some(group) => match tracked.signal_group(group, Signal::Kill) {
                            Ok(()) => println!("killed process group {group}"),
                            Err(e) => println!("error {e}"),
                        },
                        None => println!("error this engine has no agent process to kill"),
                    },
                    "status" => println!(
                        "status session={} in_turn={} down={} jobs={} group={}",
                        status.session.as_deref().unwrap_or("-"),
                        status.in_turn,
                        status.down,
                        status.jobs,
                        tracked.agent_group().map_or_else(|| "-".into(), |g| g.to_string()),
                    ),
                    "quit" => {
                        quitting = true;
                        let _ = handle.close();
                    }
                    other => println!("error unknown command {other}"),
                }
                prompt();
            }
        }
    }
    Ok(())
}

fn prompt() {
    print!("qm-dev> ");
    let _ = std::io::stdout().flush();
}

fn report(result: Result<(), qm_engine::EngineError>) {
    if let Err(e) = result {
        println!("error {e}");
    }
}

fn print_event(event: &EngineEvent) {
    let json = serde_json::to_string(event).unwrap_or_else(|_| "{}".into());
    println!("event {} {json}", event.name());
    if let EngineEvent::TextLine { text } = event {
        println!("text │ {text}");
    }
}

fn observe(status: &mut Status, event: &EngineEvent) {
    match event {
        EngineEvent::SessionReady { session, .. } => {
            status.session = Some(session.as_str().to_owned());
            status.down = false;
        }
        EngineEvent::TurnStarted => status.in_turn = true,
        EngineEvent::TurnEnded { outcome, .. } => {
            status.in_turn = false;
            if let qm_engine::TurnOutcome::Failed { failure } = outcome
                && failure.kind == qm_engine::FailureKind::ProcessExited
            {
                status.down = true;
            }
        }
        EngineEvent::Suspended => status.down = true,
        EngineEvent::JobStarted { .. } => status.jobs += 1,
        EngineEvent::JobFinished { .. } => status.jobs = status.jobs.saturating_sub(1),
        EngineEvent::SessionClosed { .. } => status.closed = true,
        _ => {}
    }
}

/// The dev module's tools: `ping` only.
fn answer(name: &str, args: &Value) -> ToolResult {
    if name != "ping" {
        return ToolResult::error(format!("unknown tool {name}"));
    }
    match args.get("text").and_then(Value::as_str) {
        Some(text) => ToolResult::ok(format!("pong from rust: {text}")),
        None => ToolResult::error("ping: missing required string 'text'"),
    }
}
