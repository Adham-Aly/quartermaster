//! The macOS supervisor.
//!
//! - **Spawn**: `tokio::process::Command` with `process_group(0)`, so every child leads its own
//!   new group and its children inherit it; `env_clear()` plus exactly `spec.env`; all three
//!   pipes; `kill_on_drop(false)` (stopping is [`super::stop`]'s job, and a dropped handle must
//!   not kill a process the caller still tracks through its group). Right after spawning, the
//!   leader's start time is read from the OS with libproc (`proc_pidinfo(PROC_PIDTBSDINFO)`,
//!   `pbi_start_tvsec`/`pbi_start_tvusec`, kept to the millisecond) and the group is recorded.
//!   A Tokio task reaps the leader and reports its exit through the [`ExitWatch`] and the
//!   recorder.
//! - **Signals**: `killpg`; a group that no longer exists (`ESRCH`) is not an error.
//! - **Liveness**: `killpg(group, 0)`: success or `EPERM` means some member lives.
//! - **Members**: libproc's `proc_listpids(PROC_PGRP_ONLY)`, each member with its current name
//!   and start time.
//! - **Listeners**: `lsof -nP -iTCP:<lo>-<hi> -sTCP:LISTEN -F pcn`, which ships with macOS.
//!
//! No `unsafe`: libproc and nix wrap the system calls. Spawning needs a Tokio runtime with the
//! `process` and `rt` features.

use super::{
    Child, ChildOutput, ChildStdin, ExitCause, ExitWatch, GroupMember, PortListener, ProcessGroup,
    ProcessInfo, ProcessRecorder, Signal, SpawnSpec, Supervisor, SupervisorError,
};
use jiff::Timestamp;
use libproc::libproc::bsd_info::BSDInfo;
use libproc::libproc::proc_pid::pidinfo;
use libproc::processes::{ProcFilter, pids_by_type};
use nix::errno::Errno;
use nix::sys::signal::{Signal as NixSignal, killpg};
use nix::unistd::Pid;
use std::collections::BTreeSet;
use std::fmt;
use std::ops::RangeInclusive;
use std::os::raw::c_char;
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::sync::Arc;

/// The real supervisor: process groups, signals and libproc on macOS.
#[derive(Clone, Default)]
pub struct OsSupervisor {
    recorder: Option<Arc<dyn ProcessRecorder>>,
}

impl fmt::Debug for OsSupervisor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OsSupervisor")
            .field("recording", &self.recorder.is_some())
            .finish()
    }
}

impl OsSupervisor {
    /// A supervisor that records every group it starts through `recorder` (the core backs it
    /// with the `processes` table); `None` records nothing.
    pub fn new(recorder: Option<Arc<dyn ProcessRecorder>>) -> Self {
        Self { recorder }
    }
}

impl Supervisor for OsSupervisor {
    fn spawn(&self, spec: SpawnSpec) -> Result<Child, SupervisorError> {
        let program = program_name(&spec.program);
        let spawn_error = |message: String| SupervisorError::Spawn {
            program: program.clone(),
            message,
        };

        let mut command = tokio::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false)
            .process_group(0);
        let mut child = command.spawn().map_err(|e| spawn_error(e.to_string()))?;
        let pid = child
            .id()
            .ok_or_else(|| spawn_error("the process exited before it could be recorded".into()))?;
        let pgid =
            i32::try_from(pid).map_err(|_| spawn_error(format!("pid {pid} is too large")))?;

        let started_at = process_start(pid).unwrap_or_else(|| {
            // Only when the leader already exited and was reaped between spawn and this read:
            // such a group can't be a leftover, so the recorded time is never compared.
            Timestamp::now()
        });
        let info = ProcessInfo {
            pid,
            group: ProcessGroup::new(pgid),
            program,
            owner: spec.owner,
            started_at,
        };
        if let Some(recorder) = &self.recorder {
            recorder.started(&info);
        }

        let stdin = child.stdin.take().map(|s| Box::new(s) as ChildStdin);
        let stdout = child.stdout.take().map(|s| Box::new(s) as ChildOutput);
        let stderr = child.stderr.take().map(|s| Box::new(s) as ChildOutput);

        let (notifier, exit) = ExitWatch::channel();
        let recorder = self.recorder.clone();
        let group = info.group;
        tokio::spawn(async move {
            let cause = match child.wait().await {
                Ok(status) => match (status.code(), status.signal()) {
                    (Some(code), _) => ExitCause::Exited { code },
                    (None, Some(signal)) => ExitCause::Signaled { signal },
                    (None, None) => ExitCause::Signaled { signal: 9 },
                },
                // Waiting can only fail if the child was already reaped elsewhere; treat it as
                // killed, as `ExitWatch::wait` does for a dropped notifier.
                Err(_) => ExitCause::Signaled { signal: 9 },
            };
            if let Some(recorder) = &recorder {
                recorder.exited(group, cause);
            }
            notifier.exited(cause);
        });

        Ok(Child {
            info,
            stdin,
            stdout,
            stderr,
            exit,
        })
    }

    fn signal_group(&self, group: ProcessGroup, signal: Signal) -> Result<(), SupervisorError> {
        let signal = match signal {
            Signal::Term => NixSignal::SIGTERM,
            Signal::Kill => NixSignal::SIGKILL,
        };
        match killpg(Pid::from_raw(group.get()), signal) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(e) => Err(SupervisorError::Signal {
                group,
                message: e.desc().to_owned(),
            }),
        }
    }

    fn group_alive(&self, group: ProcessGroup) -> bool {
        if group.get() <= 1 {
            return false;
        }
        match killpg(Pid::from_raw(group.get()), None) {
            Ok(()) | Err(Errno::EPERM) => true,
            Err(_) => false,
        }
    }

    fn group_members(&self, group: ProcessGroup) -> Vec<GroupMember> {
        let Ok(pgrpid) = u32::try_from(group.get()) else {
            return Vec::new();
        };
        if pgrpid <= 1 {
            return Vec::new();
        }
        let Ok(pids) = pids_by_type(ProcFilter::ByProgramGroup { pgrpid }) else {
            return Vec::new();
        };
        pids.into_iter()
            .filter(|&pid| pid != 0)
            .filter_map(|pid| {
                let info = bsd_info(pid)?;
                // A member that changed group, or a zombie, between listing and reading.
                if info.pbi_pgid != pgrpid || info.pbi_status == SZOMB {
                    return None;
                }
                Some(GroupMember {
                    pid,
                    program: member_name(&info),
                    started_at: start_of(&info)?,
                })
            })
            .collect()
    }

    fn listeners(&self, ports: RangeInclusive<u16>) -> Result<Vec<PortListener>, SupervisorError> {
        let (lo, hi) = (*ports.start(), *ports.end());
        if lo > hi {
            return Ok(Vec::new());
        }
        let output = std::process::Command::new("/usr/sbin/lsof")
            .arg("-nP")
            .arg(format!("-iTCP:{lo}-{hi}"))
            .arg("-sTCP:LISTEN")
            .arg("-F")
            .arg("pcn")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| SupervisorError::Listeners(e.to_string()))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            // lsof exits 1 with no output when nothing matches.
            if output.status.code() == Some(1) && stdout.trim().is_empty() {
                return Ok(Vec::new());
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SupervisorError::Listeners(format!(
                "lsof exited with {}: {}",
                output.status,
                stderr.trim()
            )));
        }
        Ok(parse_lsof(&stdout, &ports))
    }
}

/// `SZOMB` from `<sys/proc.h>`: a process that exited and wasn't reaped yet.
const SZOMB: u32 = 5;

fn program_name(program: &std::path::Path) -> String {
    program.file_name().map_or_else(
        || program.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn bsd_info(pid: u32) -> Option<BSDInfo> {
    let pid = i32::try_from(pid).ok()?;
    pidinfo::<BSDInfo>(pid, 0).ok()
}

/// The process's start time as the OS reports it, to the millisecond.
fn process_start(pid: u32) -> Option<Timestamp> {
    start_of(&bsd_info(pid)?)
}

fn start_of(info: &BSDInfo) -> Option<Timestamp> {
    let secs = i64::try_from(info.pbi_start_tvsec).ok()?;
    let micros = i64::try_from(info.pbi_start_tvusec).ok()?;
    Timestamp::from_millisecond(secs.checked_mul(1000)?.checked_add(micros / 1000)?).ok()
}

/// The member's name: the long `pbi_name` when set, else the 16-character `pbi_comm`.
fn member_name(info: &BSDInfo) -> String {
    let name = c_chars(&info.pbi_name);
    if name.is_empty() {
        c_chars(&info.pbi_comm)
    } else {
        name
    }
}

fn c_chars(chars: &[c_char]) -> String {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c.to_ne_bytes()[0])
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Parses `lsof -F pcn` output: `p<pid>`, `c<command>` and `n<address:port>` lines, one process
/// set per `p` line.
fn parse_lsof(output: &str, ports: &RangeInclusive<u16>) -> Vec<PortListener> {
    let mut seen = BTreeSet::new();
    let mut listeners = Vec::new();
    let mut pid: Option<u32> = None;
    let mut program = String::new();
    for line in output.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let value = &line[tag.len_utf8()..];
        match tag {
            'p' => {
                pid = value.parse().ok();
                program.clear();
            }
            'c' => value.clone_into(&mut program),
            'n' => {
                let Some(pid) = pid else { continue };
                let Some(port) = value
                    .rsplit(':')
                    .next()
                    .and_then(|p| p.trim_end_matches(" (LISTEN)").parse::<u16>().ok())
                else {
                    continue;
                };
                if ports.contains(&port) && seen.insert((pid, port)) {
                    listeners.push(PortListener {
                        port,
                        pid,
                        program: program.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    listeners
}
