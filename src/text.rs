//! Cutting text down to size. Every module that shortens something for a
//! prompt, a status line, or a log does it through here, so "how long is too
//! long" is the only thing a caller decides.

use serde_json::Value;

/// The longest prefix of `text` that fits in `max` bytes without splitting a
/// character.
pub fn prefix(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `text` held to `max` bytes, with `tail` marking that something was cut.
pub fn clip_bytes(text: &str, max: usize, tail: &str) -> String {
    if text.len() <= max { text.to_owned() } else { format!("{}{tail}", prefix(text, max)) }
}

/// `text` held to `max` characters, with `tail` marking that something was cut.
pub fn clip(text: &str, max: usize, tail: &str) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}{tail}", &text[..end]),
        None => text.to_owned(),
    }
}

/// `text` on one line: line breaks become spaces.
pub fn flat(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

/// `text` on one line with every run of whitespace collapsed to a space.
pub fn squeeze(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The most recent thing the assistant said, or nothing.
pub fn last_reply(messages: &[Value]) -> &str {
    messages
        .iter()
        .rev()
        .filter(|message| message["role"] == "assistant")
        .find_map(|message| message["content"].as_str())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_never_split_a_character() {
        assert_eq!(prefix("héllo", 2), "h");
        assert_eq!(clip_bytes("héllo", 2, "…"), "h…");
        assert_eq!(clip("héllo", 2, "…"), "hé…");
        assert_eq!(clip("hé", 2, "…"), "hé", "nothing cut, nothing marked");
    }

    #[test]
    fn one_line_forms_differ_only_in_how_they_treat_runs() {
        assert_eq!(flat("a\n\nb"), "a  b");
        assert_eq!(squeeze(" a\n\n b "), "a b");
    }
}
