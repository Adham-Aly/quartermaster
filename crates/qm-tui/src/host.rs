//! The UI host: one [`App`] drawn into many windows.
//!
//! A window is a [`Term`] plus a ratatui terminal on a fixed viewport that draws into a byte
//! buffer; the host sends those bytes to the window's client as [`Frame::Output`]. Input and
//! updates from the core wake the host; a 40 ms timer drives the time-based parts (the quit
//! window, flashes, the busy indicator, the setup panes). Before the host handles a window's input
//! or draws for it, it swaps that window's `Term` into the `App`, and swaps it back out after, so
//! the screens keep reading `app.term`.
//!
//! Windows reach the host as byte streams over [`Conn`]: a `tokio::io::duplex` in-process, a
//! socket later. The host ends when the UI quits, the core closes, or (once no more streams can
//! arrive) its last window leaves.

use crate::app::{App, Quit};
use crate::backend::Backend16;
use crate::convert;
use crate::input::prefs;
use crate::term::{Background, Term};
use crate::terminal::draw_synchronized;
use qm_types::api::{Command, CoreClient};
use qm_types::wire::{
    self, Control, ExitReason, Frame, Hello, Refuse, Welcome, read_frame, write_frame,
};
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// How often the time-based parts are checked.
const TICK: Duration = Duration::from_millis(40);

/// How long a new connection has to say hello.
const HELLO_WITHIN: Duration = Duration::from_secs(5);

/// Frames queued for one window before it counts as stalled and gets a full repaint instead.
const QUEUE: usize = 64;

/// A byte stream to a client.
pub trait Conn: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Conn for T {}

/// Where the host receives new windows' streams.
pub type Windows = mpsc::UnboundedReceiver<Box<dyn Conn>>;

/// What the tasks around the host tell it.
enum Msg {
    /// A connection said a good hello.
    Joined(Hello, ReadHalf<Box<dyn Conn>>, WriteHalf<Box<dyn Conn>>),
    /// A window's frame.
    Frame(u64, Frame),
    /// A window's stream ended.
    Gone(u64),
    /// A new connection didn't become a window.
    TurnedAway,
}

/// One attached window.
struct Window {
    id: u64,
    term: Term,
    terminal: Terminal<Backend16<Sink>>,
    /// What the terminal has drawn since it was last taken.
    drawn: Sink,
    out: mpsc::Sender<Frame>,
    writer: JoinHandle<()>,
    reader: JoinHandle<()>,
    /// Frames were dropped because it stopped reading: the next frame must repaint everything.
    stalled: bool,
}

impl Window {
    fn send(&mut self, frame: Frame) {
        if self.out.try_send(frame).is_err() {
            self.stalled = true;
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        // A fixed viewport never asks the real terminal for its size; resizing clears it, so the
        // next frame draws every cell.
        let _ = self.terminal.resize(Rect::new(0, 0, cols, rows));
        self.stalled = false;
    }
}

/// A byte buffer the terminal draws into and the host reads out of.
#[derive(Clone, Debug, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Sink {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.lock())
    }
}

impl io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn new_terminal(cols: u16, rows: u16, sink: Sink) -> io::Result<Terminal<Backend16<Sink>>> {
    Terminal::with_options(
        Backend16::new(sink),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
        },
    )
}

/// Runs the UI for the windows that arrive on `windows`, until the user quits, the core closes, or
/// no window is left and none can arrive.
///
/// # Errors
/// A terminal that can't be set up for a window is turned away; this itself does not fail except
/// on a broken runtime.
pub async fn run(mut client: CoreClient, mut windows: Windows) -> io::Result<()> {
    if client.send(Command::Refresh).is_err() {
        return Ok(());
    }
    let prefs_path = prefs::default_path();
    let mut app = App::new();
    prefs_path
        .as_deref()
        .map(prefs::load)
        .unwrap_or_default()
        .apply(&mut app);
    app.prefs_path.clone_from(&prefs_path);
    crate::sample::install_from_env(&mut app);

    let (inbox_tx, mut inbox) = mpsc::unbounded_channel::<Msg>();
    let mut host = Host {
        app,
        windows: Vec::new(),
        next_id: 1,
        greeting: 0,
        inbox: inbox_tx,
    };
    let mut accepting = true;
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            stream = windows.recv(), if accepting => match stream {
                Some(stream) => host.greet(stream),
                None => accepting = false,
            },
            Some(msg) = inbox.recv() => host.on_msg(msg),
            update = client.recv() => match update {
                Some(update) => host.on_update(update),
                None => host.app.finish(),
            },
            _ = tick.tick() => {}
        }
        // Everything already waiting goes into one frame.
        while let Ok(msg) = inbox.try_recv() {
            host.on_msg(msg);
        }
        loop {
            match client.try_recv() {
                Ok(Some(update)) => host.on_update(update),
                Ok(None) => break,
                Err(_closed) => {
                    host.app.finish();
                    break;
                }
            }
        }
        for command in host.app.take_commands() {
            if client.send(command).is_err() {
                host.app.finish();
            }
        }
        if host.app.exiting() || (host.windows.is_empty() && host.greeting == 0 && !accepting) {
            break;
        }
        host.tick(Instant::now());
        if host.app.take_dirty() {
            host.draw_all();
        }
        host.suspend_requests();
    }
    host.close().await;
    Ok(())
}

struct Host {
    app: App,
    windows: Vec<Window>,
    next_id: u64,
    /// Connections that haven't said hello yet; the host waits for them before it can end.
    greeting: usize,
    inbox: mpsc::UnboundedSender<Msg>,
}

impl Host {
    /// Reads a new connection's hello in the background; the host hears of it when it's good.
    fn greet(&mut self, stream: Box<dyn Conn>) {
        self.greeting += 1;
        let inbox = self.inbox.clone();
        tokio::spawn(async move {
            let (mut read, mut write) = tokio::io::split(stream);
            let hello = tokio::time::timeout(HELLO_WITHIN, read_frame(&mut read)).await;
            match hello {
                Ok(Ok(Some(Frame::Hello(hello)))) if hello.protocol == wire::PROTOCOL => {
                    let _ = inbox.send(Msg::Joined(hello, read, write));
                    return;
                }
                Ok(Ok(Some(Frame::Hello(hello)))) => {
                    let reason = format!(
                        "This Quartermaster speaks protocol {} and your terminal speaks {}; \
                         restart one of them so they match.",
                        wire::PROTOCOL,
                        hello.protocol
                    );
                    let _ = write_frame(&mut write, &Frame::Refuse(Refuse::new(&reason))).await;
                }
                _ => {}
            }
            let _ = inbox.send(Msg::TurnedAway);
        });
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Joined(hello, read, write) => {
                self.greeting = self.greeting.saturating_sub(1);
                self.join(&hello, read, write);
            }
            Msg::TurnedAway => self.greeting = self.greeting.saturating_sub(1),
            Msg::Frame(id, frame) => self.on_frame(id, frame),
            Msg::Gone(id) => self.windows.retain(|w| w.id != id),
        }
    }

    fn join(
        &mut self,
        hello: &Hello,
        read: ReadHalf<Box<dyn Conn>>,
        mut write: WriteHalf<Box<dyn Conn>>,
    ) {
        let cols = hello.cols.max(1);
        let rows = hello.rows.max(1);
        let id = self.next_id;
        let drawn = Sink::default();
        let Ok(terminal) = new_terminal(cols, rows, drawn.clone()) else {
            tokio::spawn(async move {
                let reason = Refuse::new("Quartermaster can't draw for a window this size.");
                let _ = write_frame(&mut write, &Frame::Refuse(reason)).await;
            });
            return;
        };
        self.next_id += 1;
        let (out, mut queue) = mpsc::channel::<Frame>(QUEUE);
        let writer = tokio::spawn(async move {
            while let Some(frame) = queue.recv().await {
                if write_frame(&mut write, &frame).await.is_err() {
                    break;
                }
            }
        });
        let inbox = self.inbox.clone();
        let reader = tokio::spawn(async move {
            let mut read = read;
            while let Ok(Some(frame)) = read_frame(&mut read).await {
                let detach = frame == Frame::Detach;
                if inbox.send(Msg::Frame(id, frame)).is_err() || detach {
                    return;
                }
            }
            let _ = inbox.send(Msg::Gone(id));
        });
        let mut term = Term::new();
        term.ssh = hello.ssh;
        term.size = (cols, rows);
        term.background = match hello.background {
            wire::Background::Light => Background::Light,
            wire::Background::Dark => Background::Dark,
            wire::Background::Unknown => Background::Unknown,
        };
        let mut window = Window {
            id,
            term,
            terminal,
            drawn,
            out,
            writer,
            reader,
            stalled: false,
        };
        window.send(Frame::Welcome(Welcome::new(id, env!("CARGO_PKG_VERSION"))));
        self.windows.push(window);
        self.app.redraw();
    }

    fn on_frame(&mut self, id: u64, frame: Frame) {
        let Some(at) = self.windows.iter().position(|w| w.id == id) else {
            return;
        };
        match frame {
            Frame::Input(event) => {
                let event = convert::from_wire(event);
                self.with_window(at, |app| app.on_event(event, Instant::now()));
            }
            Frame::Resize { cols, rows } => {
                self.windows[at].resize(cols.max(1), rows.max(1));
                self.app.redraw();
            }
            Frame::Detach => {
                let window = self.windows.remove(at);
                window.reader.abort();
            }
            // Frames only the host sends, and a second hello.
            _ => {}
        }
    }

    /// Runs `f` on the app with window `at`'s `Term` swapped in.
    fn with_window<T>(&mut self, at: usize, f: impl FnOnce(&mut App) -> T) -> T {
        std::mem::swap(&mut self.app.term, &mut self.windows[at].term);
        let result = f(&mut self.app);
        std::mem::swap(&mut self.app.term, &mut self.windows[at].term);
        result
    }

    /// Applies an update from the core. Window-state it sets on the app's own `Term` (a bell, the
    /// quit dialog) is moved to the windows it belongs to.
    fn on_update(&mut self, update: qm_types::api::Update) {
        self.app.on_update(update);
        let scratch = std::mem::replace(&mut self.app.term, Term::new());
        if scratch.bells > 0 {
            for window in &mut self.windows {
                window.term.bells = 1;
            }
        }
        if matches!(scratch.quit, Quit::Confirm { .. }) {
            // The dialog belongs to the window that asked to quit; with none waiting, to all.
            let asked = self
                .windows
                .iter()
                .any(|w| matches!(w.term.quit, Quit::Requested));
            for window in &mut self.windows {
                if !asked || matches!(window.term.quit, Quit::Requested) {
                    window.term.quit = scratch.quit;
                }
            }
        }
    }

    fn tick(&mut self, now: Instant) {
        if self.windows.is_empty() {
            self.app.tick(now);
            return;
        }
        for at in 0..self.windows.len() {
            self.with_window(at, |app| app.tick(now));
        }
    }

    /// Sends each window what it needs to look current.
    fn draw_all(&mut self) {
        for at in 0..self.windows.len() {
            if self.windows[at].stalled {
                let (cols, rows) = self.windows[at].term.size;
                self.windows[at].resize(cols, rows);
            }
            self.draw_window(at);
        }
    }

    fn draw_window(&mut self, at: usize) {
        std::mem::swap(&mut self.app.term, &mut self.windows[at].term);
        let window = &mut self.windows[at];
        let app = &mut self.app;
        let drawn = draw_synchronized(&mut window.terminal, |frame| app.draw(frame));
        app.after_draw();
        let mut bytes = window.drawn.take();
        if let Some(osc52) = app.term.osc52.take() {
            // Outside the synchronized update, so the terminal acts on it at once.
            bytes.extend_from_slice(osc52.as_bytes());
        }
        if std::mem::take(&mut app.term.bells) > 0 {
            bytes.push(0x07);
        }
        std::mem::swap(&mut self.app.term, &mut self.windows[at].term);
        if drawn.is_ok() && !bytes.is_empty() {
            self.windows[at].send(Frame::Output(bytes));
        }
    }

    /// `Ctrl`+`Z` in a window: that window's client leaves full-screen mode and stops.
    fn suspend_requests(&mut self) {
        for window in &mut self.windows {
            if std::mem::take(&mut window.term.suspend) {
                window.send(Frame::Control(Control::Suspend));
            }
        }
    }

    /// Tells every window to leave and waits for what's queued to be written.
    async fn close(mut self) {
        let exit = Frame::Control(Control::Exit {
            reason: ExitReason::Quit,
        });
        for window in &mut self.windows {
            window.send(exit.clone());
            window.reader.abort();
        }
        for window in self.windows {
            drop(window.out);
            let _ = tokio::time::timeout(Duration::from_secs(2), window.writer).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qm_types::wire::{InputEvent, KeyCode, KeyKind, Modifiers};
    use tokio::io::DuplexStream;

    async fn window(
        windows: &mpsc::UnboundedSender<Box<dyn Conn>>,
        cols: u16,
        rows: u16,
    ) -> DuplexStream {
        let (host_end, mut mine) = tokio::io::duplex(1 << 20);
        windows.send(Box::new(host_end)).unwrap();
        write_frame(&mut mine, &Frame::Hello(Hello::new("t", "t", cols, rows)))
            .await
            .unwrap();
        assert!(matches!(
            read_frame(&mut mine).await.unwrap(),
            Some(Frame::Welcome(_))
        ));
        mine
    }

    /// Everything the host sends until it has been quiet for a moment, as text.
    async fn drawn(stream: &mut DuplexStream) -> String {
        let mut text = Vec::new();
        while let Ok(Ok(Some(frame))) =
            tokio::time::timeout(Duration::from_millis(300), read_frame(stream)).await
        {
            if let Frame::Output(bytes) = frame {
                text.extend(bytes);
            }
        }
        String::from_utf8_lossy(&text).into_owned()
    }

    #[tokio::test]
    async fn typing_in_one_window_shows_in_the_other() {
        let (client, mut core) = qm_types::api::link();
        let (tx, windows) = mpsc::unbounded_channel();
        let host = tokio::spawn(run(client, windows));
        let mut a = window(&tx, 100, 30).await;
        let mut b = window(&tx, 80, 24).await;
        let first_a = drawn(&mut a).await;
        let first_b = drawn(&mut b).await;
        assert!(first_a.contains("Projects") && first_b.contains("Projects"));
        for c in "zqxj".chars() {
            let key = InputEvent::Key {
                code: KeyCode::Char(c),
                modifiers: Modifiers::default(),
                kind: KeyKind::Press,
            };
            write_frame(&mut a, &Frame::Input(key)).await.unwrap();
        }
        assert!(drawn(&mut a).await.contains('z'));
        assert!(drawn(&mut b).await.contains('z'));
        // Leaving one window leaves the other; the last one leaving ends the host.
        write_frame(&mut a, &Frame::Detach).await.unwrap();
        drop(tx);
        write_frame(&mut b, &Frame::Detach).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), host)
            .await
            .expect("the host ends")
            .unwrap();
        // The core sees its UI go.
        assert!(core.recv().await.is_some());
    }
}
