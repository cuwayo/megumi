//! Reading a command name and its arguments out of message text.

use crate::args::Args;
use whatsapp_rust::prelude::MessageExt;

/// The text a command is read from: the message body, or, when media replaces
/// the body, the caption that came with it.
pub fn command_text(message: &whatsapp_rust::prelude::wa::Message) -> Option<&str> {
    message.text_content().or_else(|| message.get_caption())
}

/// Reads a command name and the arguments after it out of `text`.
///
/// Returns `None` when `text` does not start with `prefix`, or when the prefix
/// is not followed by a name. The name is returned as typed, with the arguments
/// trimmed of the whitespace that separated them.
pub fn parse_command_text<'a>(text: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    let text = text.trim_start();
    if !text.starts_with(prefix) {
        return None;
    }

    let after_prefix = &text[prefix.len()..];
    let (name, rest) = after_prefix
        .split_once(char::is_whitespace)
        .unwrap_or((after_prefix, ""));
    if name.is_empty() {
        return None;
    }
    Some((name, rest.trim_start()))
}

/// Wraps raw argument text in an [`Args`], the entry point a caller outside
/// dispatch uses to build one.
pub fn parse_args(input: &str) -> Args {
    Args::from_rest(input)
}
