//! Finding the server, and starting it when there isn't one.
//!
//! Connect to `server.sock`; if nothing answers, start `quartermaster server run` and poll every
//! 50 ms for up to 15 s. On a Mac outside SSH the start goes through a one-shot launchd job in the
//! user's `gui` domain, which puts the server outside the terminal app's process tree, so quitting
//! Terminal.app or iTerm2 can't take it down. Over SSH, or if launchd refuses, it is a direct
//! spawn; the server calls `setsid` on itself either way.

use qm_types::data_dir::DataDir;
use qm_types::env::NEVER_PASSED_TO_AGENTS;
use std::fmt::Write as _;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tokio::net::UnixStream;

const POLL: Duration = Duration::from_millis(50);
const GIVE_UP: Duration = Duration::from_secs(15);
const LABEL_PREFIX: &str = "app.quartermaster.server.";

/// Set in the environment of a server started through launchd, so it can remove its own job.
pub const LAUNCHD_LABEL: &str = "QM_LAUNCHD_LABEL";
/// Set to anything to start servers directly, never through launchd.
pub const NO_LAUNCHD: &str = "QM_NO_LAUNCHD";

/// Whether nothing is serving: no socket, or one nobody listens on.
fn nobody_answers(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// A connection to the running server; `Ok(None)` when none is running.
pub async fn connect(dir: &DataDir) -> Result<Option<UnixStream>, String> {
    match UnixStream::connect(dir.server_socket()).await {
        Ok(stream) => Ok(Some(stream)),
        Err(e) if nobody_answers(&e) => Ok(None),
        Err(e) => Err(format!(
            "can't reach the server at {}: {e}",
            dir.server_socket().display()
        )),
    }
}

/// A connection to the server, starting it first if none is running.
pub async fn connect_or_start(dir: &DataDir) -> Result<UnixStream, String> {
    if let Some(stream) = connect(dir).await? {
        return Ok(stream);
    }
    start(dir)?;
    let began = Instant::now();
    loop {
        tokio::time::sleep(POLL).await;
        if let Some(stream) = connect(dir).await? {
            return Ok(stream);
        }
        if began.elapsed() >= GIVE_UP {
            return Err(format!(
                "the server didn't start within {} seconds; the log in {} says why",
                GIVE_UP.as_secs(),
                dir.logs().display()
            ));
        }
    }
}

/// Whether this session is over SSH, where launchd would start the server outside it.
fn over_ssh() -> bool {
    ["SSH_TTY", "SSH_CONNECTION"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

/// Starts `server run` without waiting for it.
fn start(dir: &DataDir) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("can't find this program: {e}"))?;
    if cfg!(target_os = "macos") && !over_ssh() && std::env::var_os(NO_LAUNCHD).is_none() {
        match launchd(dir, &exe) {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!("launchd wouldn't start the server, starting it directly: {e}");
            }
        }
    }
    Command::new(&exe)
        .args(["server", "run"])
        .current_dir(dir.root())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| format!("can't start the server: {e}"))
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The job description: this program as `server run`, with the environment of the window that
/// asked (launchd's own is nearly empty, and agents inherit the server's).
fn plist(label: &str, exe: &Path, root: &Path) -> String {
    let mut env = String::new();
    let vars = std::env::vars().chain([(LAUNCHD_LABEL.to_owned(), label.to_owned())]);
    for (k, v) in vars {
        let plain = !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if plain && !NEVER_PASSED_TO_AGENTS.contains(&k.as_str()) {
            let _ = writeln!(
                env,
                "    <key>{}</key><string>{}</string>",
                xml(&k),
                xml(&v)
            );
        }
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\n\
         <key>Label</key><string>{label}</string>\n\
         <key>ProgramArguments</key><array><string>{exe}</string><string>server</string><string>run</string></array>\n\
         <key>WorkingDirectory</key><string>{root}</string>\n\
         <key>RunAtLoad</key><true/>\n\
         <key>StandardInPath</key><string>/dev/null</string>\n\
         <key>StandardOutPath</key><string>/dev/null</string>\n\
         <key>StandardErrorPath</key><string>/dev/null</string>\n\
         <key>EnvironmentVariables</key><dict>\n{env}</dict>\n\
         </dict></plist>\n",
        exe = xml(&exe.display().to_string()),
        root = xml(&root.display().to_string()),
    )
}

fn launchd(dir: &DataDir, exe: &Path) -> Result<(), String> {
    let uid = nix::unistd::getuid();
    let domain = format!("gui/{uid}");
    remove_finished_jobs();
    let label = format!(
        "{LABEL_PREFIX}{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis())
    );
    // The file holds the environment, so it is private and gone as soon as launchd has read it.
    let file = dir.root().join("server-launch.plist");
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&file)
            .map_err(|e| e.to_string())?;
        f.write_all(plist(&label, exe, dir.root()).as_bytes())
            .map_err(|e| e.to_string())?;
    }
    let out = Command::new("launchctl")
        .args(["bootstrap", &domain])
        .arg(&file)
        .stdin(Stdio::null())
        .output();
    let _ = std::fs::remove_file(&file);
    let out = out.map_err(|e| format!("launchctl: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "launchctl bootstrap {domain}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Removes our earlier one-shot jobs that have exited, which launchd would otherwise keep listing.
fn remove_finished_jobs() {
    let Ok(out) = Command::new("launchctl").arg("list").output() else {
        return;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut cols = line.split_whitespace();
        let (Some(pid), Some(_status), Some(label)) = (cols.next(), cols.next(), cols.next())
        else {
            continue;
        };
        if pid == "-" && label.starts_with(LABEL_PREFIX) {
            let _ = remove_job(label);
        }
    }
}

/// `launchctl remove <label>`.
pub fn remove_job(label: &str) -> io::Result<()> {
    Command::new("launchctl")
        .args(["remove", label])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(drop)
}
