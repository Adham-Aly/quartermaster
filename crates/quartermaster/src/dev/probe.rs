//! `quartermaster dev probe claude [--claude-bin PATH]`: detection and the startup check.
//!
//! Opens zero-token orchestrator and crewmate sessions (no model turn), prints what `claude`
//! reported, including the crewmate's tool list, and the check's verdict.

use super::{Flags, dev_fail, inherited_env, ping_tool};
use crate::Failure;
use qm_engine::supervisor::OsSupervisor;
use qm_engine_claude::detect::{LoginState, detect, detect_at};
use qm_engine_claude::{ClaudeConfig, ClaudeEngine};
use qm_types::clock::SystemClock;
use qm_types::config::Config;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;

pub(super) async fn run(flags: &Flags) -> Result<(), Failure> {
    let inherited = inherited_env();
    let install = match flags.get("--claude-bin") {
        Some(path) => detect_at(&PathBuf::from(path), &inherited),
        None => detect(&inherited),
    };
    println!(
        "detect path={} version={} login={}",
        install
            .path
            .as_ref()
            .map_or_else(|| "-".into(), |p| p.display().to_string()),
        install.version.as_deref().unwrap_or("-"),
        match &install.login {
            LoginState::Subscription { subscription_type } => {
                format!("subscription({subscription_type})")
            }
            LoginState::ApiKey => "api_key".into(),
            LoginState::LoggedOut => "logged_out".into(),
            LoginState::Missing => "missing".into(),
        }
    );
    if let Some(reason) = install.unavailable_reason() {
        println!("check {reason}");
        return Err(dev_fail(reason));
    }
    let Some(claude) = install.path.clone() else {
        return Err(dev_fail("claude wasn't found"));
    };
    let engine = ClaudeEngine::new(ClaudeConfig {
        claude,
        supervisor: Arc::new(OsSupervisor::new(None)),
        clock: Arc::new(SystemClock),
        inherited_env: inherited,
        settings: watch::channel(Config::default()).1,
    });
    engine.set_install(install);
    let report = qm_engine_claude::check::probe(&engine, vec![ping_tool()])
        .await
        .map_err(|e| dev_fail(format!("the probe failed: {e}")))?;
    println!(
        "orchestrator version={} permission_mode={} tools={:?} capabilities={:?}",
        report.orchestrator.engine_version.as_deref().unwrap_or("-"),
        report
            .orchestrator
            .permission_mode
            .as_deref()
            .unwrap_or("-"),
        report.orchestrator.tools,
        report.orchestrator.capabilities,
    );
    println!(
        "crewmate permission_mode={} tools={:?}",
        report.crewmate.permission_mode.as_deref().unwrap_or("-"),
        report.crewmate_tools(),
    );
    println!("models {}", report.models.len());
    match report.check {
        Ok(()) => {
            println!("check ok");
            Ok(())
        }
        Err(e) => {
            println!("check {e}");
            Err(dev_fail(e.to_string()))
        }
    }
}
