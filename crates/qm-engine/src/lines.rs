//! Committing streamed text a line at a time.

/// Collects text deltas and hands back only complete lines. Engines feed every assistant text
/// delta through one of these and emit each line as `EngineEvent::TextLine`, then call
/// [`LineBuffer::flush`] when the text block ends.
#[derive(Clone, Debug, Default)]
pub struct LineBuffer {
    partial: String,
}

impl LineBuffer {
    /// Adds a delta; returns the lines it completed, without their line endings.
    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.partial.push_str(delta);
        let mut lines = Vec::new();
        while let Some(end) = self.partial.find('\n') {
            let mut line: String = self.partial.drain(..=end).collect();
            line.pop(); // the '\n'
            if line.ends_with('\r') {
                line.pop();
            }
            lines.push(line);
        }
        lines
    }

    /// The unfinished last line, if there is one.
    pub fn flush(&mut self) -> Option<String> {
        if self.partial.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.partial))
        }
    }
}
