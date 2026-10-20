//! A small Server-Sent Events framer (the subset model APIs use).
//!
//! Bytes go in as they arrive; whole events come out. An event is a run of lines ended by a blank
//! line; `event:` names it, `data:` lines are joined with `\n`, comments (`:`) and `id:`/`retry:`
//! are ignored. CRLF and LF both end lines. At the end of the stream a last event without its
//! blank line still counts (as Pi's parser does). `data: [DONE]` is passed through; adapters
//! ignore it.

/// One event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, if any.
    pub event: Option<String>,
    /// The `data:` lines, joined with `\n`.
    pub data: String,
}

/// Turns a byte stream into events.
#[derive(Debug, Default)]
pub struct SseFramer {
    /// Bytes not yet ending in a newline.
    pending: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    has_field: bool,
}

impl SseFramer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds bytes; returns every event they completed.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            if let Some(event) = self.line(&line) {
                out.push(event);
            }
        }
        out
    }

    /// The stream ended: the last event, if one was left open.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            let line = String::from_utf8_lossy(&rest).into_owned();
            let line = line.strip_suffix('\r').unwrap_or(&line).to_owned();
            if let Some(event) = self.line(&line) {
                return Some(event);
            }
        }
        self.dispatch()
    }

    fn line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => {
                self.event = Some(value.to_owned());
                self.has_field = true;
            }
            "data" => {
                self.data.push(value.to_owned());
                self.has_field = true;
            }
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if !self.has_field {
            return None;
        }
        self.has_field = false;
        let event = SseEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data).join("\n"),
        };
        if event.data.is_empty() && event.event.is_none() {
            None
        } else {
            Some(event)
        }
    }
}
