//! A streaming message, a line at a time.
//!
//! While a message is still arriving, only its complete lines show, so it grows by whole lines
//! and no line already on screen ever reflows.

/// The part of `text` to show: everything up to and including the last `\n` while `streaming`,
/// all of it once the message is done.
pub(crate) fn committed(text: &str, streaming: bool) -> &str {
    if !streaming {
        return text;
    }
    text.rfind('\n').map_or("", |end| &text[..=end])
}
