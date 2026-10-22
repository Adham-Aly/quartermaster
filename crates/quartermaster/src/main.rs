//! `quartermaster`: the one binary.
//!
//! This crate owns arguments, startup, the run lock and the wiring between the crates: it builds
//! the core (`qm-core`), hands it the engines (`qm-engine-claude`, `qm-providers`, and the scripted
//! engine for the developer seat), and connects the terminal UI (`qm-tui`) to the core through the
//! internal API in `qm_types::api`. Nothing else depends on this crate.
//!
//! Commands: `quartermaster` attaches to the background server (starting it), `--foreground` runs
//! server and UI in one process, `server run|status|stop|restart` manage the server (see [`server`]).
//!
//! Startup: create the data directory, take `run.lock` (a second foreground instance with the same
//! data directory says so and exits non-zero; a second server exits quietly), start logging,
//! load `config.toml`, migrate the store on its database thread, start the core (`wiring`: engines, detection, reconciliation, the seat,
//! catching up) and run the UI.

mod catalog;
mod core_link;
mod dev;
mod home;
mod lock;
mod logging;
mod server;
mod wiring;

use qm_store::db::Db;
use qm_types::data_dir::DataDir;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure { message }) => {
            eprintln!("quartermaster: {message}");
            tracing::error!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// A startup failure, already worded for you.
struct Failure {
    message: String,
}

fn fail(what: &str) -> impl FnOnce(&dyn std::fmt::Display) -> Failure {
    move |e| Failure {
        message: format!("{what}: {e}"),
    }
}

/// What a startup that holds the run lock has ready for the core.
struct Booted {
    config: qm_types::config::Config,
    db: Db,
    dev_script: Option<qm_engine::scripted::Script>,
}

/// Logging, settings and the database, after the run lock is held.
fn boot(dir: &DataDir) -> Result<Booted, Failure> {
    logging::init(
        &dir.logs(),
        std::sync::Arc::new(qm_types::clock::SystemClock),
    )
    .map_err(|e| fail("can't start logging")(&e))?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        data_dir = %dir.root().display(),
        "starting"
    );

    // Behind the developer gate: the scripted seat's script, checked before anything runs.
    let dev_script = wiring::dev_seat_script(dir).map_err(|message| Failure { message })?;

    let loaded = qm_core::config_file::load(dir).map_err(|e| fail("can't load settings")(&e))?;
    for key in &loaded.unknown_keys {
        tracing::warn!(key = %key, "config.toml has a setting this version doesn't know; it is ignored");
    }

    // Every pending migration in one transaction; a failure rolls back cleanly.
    let db = Db::open(&dir.database()).map_err(|e| fail("can't open the database")(&e))?;
    tracing::info!("database ready");
    Ok(Booted {
        config: loaded.config,
        db,
        dev_script,
    })
}

fn run() -> Result<(), Failure> {
    if let Some(result) = dev::dispatch() {
        return result;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match server::parse(&args).map_err(|message| Failure { message })? {
        server::Command::Attach => server::attach(),
        server::Command::Foreground => foreground(),
        server::Command::ServerRun => server::run_server(),
        control => server::control(&control),
    }
}

/// Server and client in this one process, ending with the UI.
fn foreground() -> Result<(), Failure> {
    // Everything the app creates (database, logs, settings) is private to you.
    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
    let dir = DataDir::resolve().map_err(|e| fail("can't find the data directory")(&e))?;
    home::prepare(&dir).map_err(|e| {
        fail(&format!(
            "can't create the data directory {}",
            dir.root().display()
        ))(&e)
    })?;

    // The run lock, before anything writes to the data directory, and before the UI starts.
    let _lock = match lock::RunLock::acquire(&dir.run_lock()) {
        Ok(lock) => lock,
        Err(lock::LockError::AlreadyRunning) => {
            return Err(Failure {
                message: format!(
                    "Quartermaster is already running on this Mac (data directory {}). \
                     Run `quartermaster` to attach to it, or `quartermaster server stop` first.",
                    dir.root().display()
                ),
            });
        }
        Err(e) => return Err(fail("can't take the run lock")(&e)),
    };

    let Booted {
        config,
        db,
        dev_script,
    } = boot(&dir)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("qm-core")
        .build()
        .map_err(|e| fail("can't start the runtime")(&e))?;

    let (client, server) = qm_types::api::link();
    // The engines, detection in the background, the seat (recovered, its session
    // resumed) and the core's API.
    let core = runtime.spawn(wiring::start_core(
        dir.clone(),
        db,
        config,
        server,
        core_link::machine_name(),
        dev_script,
    ));

    let ui = qm_tui::run(client);

    // The UI has returned and dropped its end of the link, so the core loop ends too.
    let _ = runtime.block_on(core);
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    ui.map_err(|e| fail("the terminal UI failed")(&e))?;
    tracing::info!("stopped");
    Ok(())
}
