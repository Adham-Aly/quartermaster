//! Running a child process with a time limit and captured output.
//!
//! Shared by git invocations and the project's setup command. Each child runs in its own process
//! group, so a time limit stops everything it started, and Ctrl-C in the terminal never reaches
//! it. Standard input is always closed: nothing the app runs may wait for an answer.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

/// How long to wait for the output pipes to close after the child itself exited (a grandchild
/// may still hold them), before stopping the rest of its process group.
const PIPE_GRACE: Duration = Duration::from_secs(2);

/// How often a running child is checked.
const POLL: Duration = Duration::from_millis(10);

/// What a finished (or stopped) child left behind.
#[derive(Debug)]
pub(crate) struct Captured {
    /// `None` when the child was stopped by a signal (including our own time limit).
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

impl Captured {
    pub(crate) fn success(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }
}

/// Where the child's output goes.
#[derive(Clone, Copy, Debug)]
pub(crate) enum OutputMode {
    /// Separate stdout and stderr buffers (git, whose stdout is parsed).
    Separate,
    /// One buffer with both streams in the order written, kept in `stdout` (setup commands).
    Combined,
}

/// Runs `command` to completion or until `timeout`, then stops its whole process group.
///
/// At most `cap` bytes of each stream are kept; past that, the oldest bytes are dropped so the
/// end of the output (where errors usually are) survives.
pub(crate) fn run(
    mut command: Command,
    timeout: Duration,
    mode: OutputMode,
    cap: usize,
) -> std::io::Result<Captured> {
    command.stdin(Stdio::null()).process_group(0);
    let (out_buf, err_buf) = (Buffer::new(cap), Buffer::new(cap));
    let mut readers = Vec::new();
    let mut child = match mode {
        OutputMode::Separate => {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = command.spawn()?;
            if let Some(out) = child.stdout.take() {
                readers.push(out_buf.drain_from(out));
            }
            if let Some(err) = child.stderr.take() {
                readers.push(err_buf.drain_from(err));
            }
            child
        }
        OutputMode::Combined => {
            let (reader, writer) = std::io::pipe()?;
            command.stdout(writer.try_clone()?).stderr(writer);
            let child = command.spawn()?;
            // The Command holds the write ends; drop it so the pipe closes when the child exits.
            drop(command);
            readers.push(out_buf.drain_from(reader));
            child
        }
    };

    let deadline = Instant::now() + timeout;
    let (status, timed_out) = wait_until(&mut child, deadline)?;

    // Wait briefly for the pipes to close; a leftover grandchild holding them is stopped.
    let grace_end = Instant::now() + PIPE_GRACE;
    while readers.iter().any(|r| !r.is_finished()) && Instant::now() < grace_end {
        thread::sleep(POLL);
    }
    if readers.iter().any(|r| !r.is_finished()) {
        stop_group(&child);
        let stop_end = Instant::now() + PIPE_GRACE;
        while readers.iter().any(|r| !r.is_finished()) && Instant::now() < stop_end {
            thread::sleep(POLL);
        }
    }
    // Readers still blocked here (a process that left our group) are abandoned; they end when
    // their pipe finally closes. What they read so far is in the buffers.
    for reader in readers {
        if reader.is_finished() {
            let _ = reader.join();
        }
    }

    Ok(Captured {
        code: status.and_then(|s| s.code()),
        stdout: out_buf.take(),
        stderr: err_buf.take(),
        timed_out,
    })
}

fn wait_until(child: &mut Child, deadline: Instant) -> std::io::Result<(Option<ExitStatus>, bool)> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((Some(status), false));
        }
        if Instant::now() >= deadline {
            stop_group(child);
            let _ = child.kill();
            let status = child.wait().ok();
            return Ok((status, true));
        }
        thread::sleep(POLL);
    }
}

/// Sends SIGKILL to the child's process group (its pid, since it leads its own group).
fn stop_group(child: &Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
}

/// An output buffer filled by a reader thread, keeping at most `cap` trailing bytes.
#[derive(Clone)]
struct Buffer {
    bytes: Arc<Mutex<Vec<u8>>>,
    cap: usize,
}

impl Buffer {
    fn new(cap: usize) -> Self {
        Self {
            bytes: Arc::new(Mutex::new(Vec::new())),
            cap,
        }
    }

    fn drain_from(&self, mut source: impl Read + Send + 'static) -> thread::JoinHandle<()> {
        let buffer = self.clone();
        thread::spawn(move || {
            let mut chunk = [0_u8; 8192];
            loop {
                match source.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => buffer.push(&chunk[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        })
    }

    fn push(&self, chunk: &[u8]) {
        let mut bytes = self
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        bytes.extend_from_slice(chunk);
        if bytes.len() > self.cap {
            let excess = bytes.len() - self.cap;
            bytes.drain(..excess);
        }
    }

    fn take(&self) -> Vec<u8> {
        std::mem::take(
            &mut *self
                .bytes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}
