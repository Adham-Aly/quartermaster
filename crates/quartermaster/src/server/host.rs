//! The host the server hands every window to.
//!
//! This is the seam for the terminal UI: the host owns the one copy of the UI state, takes each
//! window's input and size, and answers with the bytes that window should show. Until the UI host
//! lands, [`spawn`] runs a stub that draws a status page and a shared draft line, which is enough
//! to exercise the server end to end: every window mirrors the same draft, each at its own size.
//! The stub also holds the UI's end of the core link, so the crew keeps working with no window.

use qm_types::api::{Command, CoreClient, Update};
use qm_types::wire::{Control, ExitReason, Frame, Hello, InputEvent, Kind, Size};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// What a stop request found.
#[derive(Debug, PartialEq, Eq)]
pub enum QuitOutcome {
    /// Agents are working and the stop wasn't forced: nothing was stopped.
    Working(u32),
    /// The core is closing.
    Closing,
}

/// How many windows are attached, and how many agents the crew tree shows beyond the Quartermaster.
pub struct HostStatus {
    pub windows: u32,
    pub working: u32,
}

enum Msg {
    Attach {
        id: u64,
        hello: Hello,
        out: mpsc::Sender<Frame>,
    },
    Input {
        id: u64,
        event: InputEvent,
    },
    Resize {
        id: u64,
        size: Size,
    },
    Detach {
        id: u64,
    },
    Status(oneshot::Sender<HostStatus>),
    Quit {
        force: bool,
        reply: oneshot::Sender<QuitOutcome>,
    },
    Exit(ExitReason),
}

/// The server's handle on the host.
#[derive(Clone)]
pub struct Host {
    tx: mpsc::UnboundedSender<Msg>,
}

impl Host {
    pub fn attach(&self, id: u64, hello: Hello, out: mpsc::Sender<Frame>) {
        let _ = self.tx.send(Msg::Attach { id, hello, out });
    }

    pub fn input(&self, id: u64, event: InputEvent) {
        let _ = self.tx.send(Msg::Input { id, event });
    }

    pub fn resize(&self, id: u64, size: Size) {
        let _ = self.tx.send(Msg::Resize { id, size });
    }

    pub fn detach(&self, id: u64) {
        let _ = self.tx.send(Msg::Detach { id });
    }

    pub async fn status(&self) -> HostStatus {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(Msg::Status(reply));
        rx.await.unwrap_or(HostStatus {
            windows: 0,
            working: 0,
        })
    }

    /// Asks the core to quit: it stops the crew, or says how many agents are working when `force`
    /// is off.
    pub async fn quit(&self, force: bool) -> QuitOutcome {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(Msg::Quit { force, reply });
        rx.await.unwrap_or(QuitOutcome::Closing)
    }

    /// Tells every window to leave, then lets go of them.
    pub fn exit_all(&self, reason: ExitReason) {
        let _ = self.tx.send(Msg::Exit(reason));
    }
}

/// Starts the host on the current runtime; the join handle finishes when the core link closes.
pub fn spawn(client: CoreClient) -> (Host, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(Stub::default().run(client, rx));
    (Host { tx }, task)
}

struct Window {
    size: Size,
    out: mpsc::Sender<Frame>,
    /// When the last Ctrl+C was pressed on an empty draft.
    armed: Option<Instant>,
}

#[derive(Default)]
struct Stub {
    windows: BTreeMap<u64, Window>,
    draft: String,
    crew: u32,
}

const DETACH_WINDOW: Duration = Duration::from_secs(2);

impl Stub {
    async fn run(mut self, mut client: CoreClient, mut rx: mpsc::UnboundedReceiver<Msg>) {
        let mut pending: Option<oneshot::Sender<QuitOutcome>> = None;
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { return };
                    match msg {
                        Msg::Quit { force, reply } => {
                            let command = if force { Command::Quit } else { Command::RequestQuit };
                            if client.send(command).is_err() {
                                let _ = reply.send(QuitOutcome::Closing);
                            } else {
                                pending = Some(reply);
                            }
                        }
                        other => self.handle(other),
                    }
                }
                update = client.recv() => match update {
                    Some(Update::ConfirmQuit { working }) => {
                        if let Some(reply) = pending.take() {
                            let _ = reply.send(QuitOutcome::Working(working));
                        }
                    }
                    Some(Update::Closing) => {
                        if let Some(reply) = pending.take() {
                            let _ = reply.send(QuitOutcome::Closing);
                        }
                    }
                    Some(Update::Snapshot(snapshot)) => {
                        let crew = u32::try_from(snapshot.crew.iter().filter(|r| r.depth > 0).count())
                            .unwrap_or(u32::MAX);
                        if crew != self.crew {
                            self.crew = crew;
                            self.paint_all();
                        }
                    }
                    Some(_) => {}
                    None => {
                        if let Some(reply) = pending.take() {
                            let _ = reply.send(QuitOutcome::Closing);
                        }
                        return;
                    }
                },
            }
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Attach { id, hello, out } => {
                self.windows.insert(
                    id,
                    Window {
                        size: hello.size,
                        out,
                        armed: None,
                    },
                );
                self.paint_all();
            }
            Msg::Input { id, event } => self.input(id, event),
            Msg::Resize { id, size } => {
                if let Some(w) = self.windows.get_mut(&id) {
                    w.size = size;
                }
                self.paint(id);
            }
            Msg::Detach { id } => {
                self.windows.remove(&id);
                self.paint_all();
            }
            Msg::Status(reply) => {
                let _ = reply.send(HostStatus {
                    windows: u32::try_from(self.windows.len()).unwrap_or(u32::MAX),
                    working: self.crew,
                });
            }
            Msg::Exit(reason) => {
                for w in std::mem::take(&mut self.windows).into_values() {
                    let _ = w
                        .out
                        .try_send(Frame::json(Kind::Control, &Control::Exit { reason }));
                }
            }
            Msg::Quit { .. } => {}
        }
    }

    fn input(&mut self, id: u64, event: InputEvent) {
        let InputEvent::Key { code, mods } = event else {
            return;
        };
        let now = Instant::now();
        if mods.ctrl && code == "c" {
            // Ctrl+C twice on an empty draft detaches this window; the server stays.
            let empty = self.draft.is_empty();
            if let Some(w) = self.windows.get_mut(&id) {
                let second = w
                    .armed
                    .is_some_and(|t| now.duration_since(t) < DETACH_WINDOW);
                if empty && second {
                    let exit = Control::Exit {
                        reason: ExitReason::Detached,
                    };
                    let _ = w.out.try_send(Frame::json(Kind::Control, &exit));
                    self.windows.remove(&id);
                    self.paint_all();
                    return;
                }
                w.armed = empty.then_some(now);
            }
        } else if mods.ctrl || mods.alt {
            return;
        } else {
            match code.as_str() {
                "backspace" => {
                    self.draft.pop();
                }
                "enter" => self.draft.clear(),
                c if c.chars().count() == 1 => self.draft.push_str(c),
                _ => return,
            }
        }
        self.paint_all();
    }

    fn paint_all(&self) {
        for id in self.windows.keys() {
            self.paint(*id);
        }
    }

    /// Draws one window at its own size. A window that stopped reading just misses the frame.
    fn paint(&self, id: u64) {
        let Some(w) = self.windows.get(&id) else {
            return;
        };
        let cols = usize::from(w.size.cols.max(20));
        let fit = |s: String| -> String { s.chars().take(cols).collect() };
        let lines = [
            fit(format!(
                "Quartermaster server {} (stub host)",
                env!("CARGO_PKG_VERSION")
            )),
            fit(format!(
                "window {id} of {} · {}x{} · crew {}",
                self.windows.len(),
                w.size.cols,
                w.size.rows,
                self.crew
            )),
            String::new(),
            fit(format!("draft: {}", self.draft)),
            String::new(),
            fit("Ctrl+C twice on an empty draft, or closing the window, detaches.".into()),
        ];
        let mut bytes = b"\x1b[?2026h\x1b[H\x1b[2J".to_vec();
        bytes.extend(lines.join("\r\n").into_bytes());
        bytes.extend_from_slice(b"\x1b[?2026l");
        let _ = w.out.try_send(Frame {
            kind: Kind::Output,
            payload: bytes,
        });
    }
}
